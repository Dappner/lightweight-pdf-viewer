use std::time::{Duration, Instant};

use objc2::AnyThread;
use objc2::rc::{Retained, autoreleasepool};
use objc2_core_foundation::{CGAffineTransform, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGColorSpace, CGContext, CGImageAlphaInfo, CGInterpolationQuality, CGPDFBox, CGPDFDocument,
    CGPDFPage, kCGColorSpaceSRGB,
};
use objc2_foundation::NSURL;
use objc2_pdf_kit::PDFDocument;

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// One bitmap and one page draw per tile, as the render thread does.
    Tile,
    /// One draw per row of tiles, sliced into tiles.
    Row,
    /// One draw for the whole page, sliced into tiles.
    Page,
}

#[derive(Clone, Copy)]
struct Variant {
    name: &'static str,
    tile: u32,
    interp: CGInterpolationQuality,
    opaque: bool,
    mode: Mode,
}

const BASE: Variant = Variant {
    name: "512 tiles (current)",
    tile: 512,
    interp: CGInterpolationQuality::High,
    opaque: false,
    mode: Mode::Tile,
};

fn variants() -> Vec<Variant> {
    vec![
        BASE,
        Variant { name: "256 tiles", tile: 256, ..BASE },
        Variant { name: "1024 tiles", tile: 1024, ..BASE },
        Variant { name: "interp default", interp: CGInterpolationQuality::Default, ..BASE },
        Variant { name: "interp medium", interp: CGInterpolationQuality::Medium, ..BASE },
        Variant { name: "interp low", interp: CGInterpolationQuality::Low, ..BASE },
        Variant { name: "opaque (NoneSkipLast)", opaque: true, ..BASE },
        Variant { name: "one draw per row", mode: Mode::Row, ..BASE },
        Variant { name: "one draw per page", mode: Mode::Page, ..BASE },
    ]
}

pub fn main() {
    let files: Vec<String> = std::env::args().skip(1).filter(|a| !a.starts_with("--")).collect();
    assert!(!files.is_empty(), "usage: cargo bench --bench render_spike -- a.pdf [...]");
    let width: f64 = std::env::var("RENDER_WIDTH").ok().and_then(|v| v.parse().ok()).unwrap_or(1800.0);
    let max_pages: usize = std::env::var("RENDER_PAGES").ok().and_then(|v| v.parse().ok()).unwrap_or(12);
    let rounds: usize = std::env::var("RENDER_ROUNDS").ok().and_then(|v| v.parse().ok()).unwrap_or(3);
    let only = std::env::var("RENDER_ONLY").ok();
    let wants = |m: &str| only.as_deref().is_none_or(|o| o == m);

    for file in &files {
        let name = std::path::Path::new(file).file_name().unwrap().to_string_lossy();
        let count = CGPDFDocument::number_of_pages(Some(&open(file).1));
        let pages: Vec<usize> = (0..max_pages.min(count)).map(|i| i * count / max_pages.min(count)).collect();
        println!("\n== {name} ({count} pages, rendering {} at {width} px wide, best of {rounds})", pages.len());

        if wants("variants") {
            println!(
                "  {:<24} {:>12} {:>14} {:>12} {:>10}",
                "variant", "ms / page", "first tile ms", "vs current", "draws"
            );
            let mut base_total = 0.0;
            for v in variants() {
                let best = (0..rounds)
                    .map(|_| run_variant(file, &pages, width, v))
                    .min_by(|a, b| a.total.cmp(&b.total))
                    .unwrap();
                let per_page = ms(best.total) / pages.len() as f64;
                if v.name == BASE.name {
                    base_total = per_page;
                }
                println!(
                    "  {:<24} {:>12.1} {:>14.1} {:>11.0}% {:>10}",
                    v.name,
                    per_page,
                    ms(best.first) / pages.len() as f64,
                    100.0 * per_page / base_total,
                    best.draws
                );
            }
        }
        if wants("quality") {
            quality(file, &pages, width);
        }
        if wants("qos") {
            qos(file, &pages, width, rounds);
        }
        if wants("parallel") {
            parallel(file, &pages, width, rounds);
        }
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn open(path: &str) -> (Retained<PDFDocument>, Retained<CGPDFDocument>) {
    let url = NSURL::from_file_path(path).expect("url");
    let pdf = unsafe { PDFDocument::initWithURL(PDFDocument::alloc(), &url) }.expect("open");
    let cg = unsafe { pdf.documentRef() }.expect("documentRef");
    (pdf, cg)
}

struct Timing {
    total: Duration,
    /// Summed over pages: time from a page's start to its first tile being ready.
    first: Duration,
    draws: usize,
}

/// Renders every page's tiles with a freshly opened document, as the render thread would
/// after reopening one.
fn run_variant(file: &str, pages: &[usize], width: f64, v: Variant) -> Timing {
    autoreleasepool(|_| {
        let (_pdf, cg) = open(file);
        let mut t = Timing { total: Duration::ZERO, first: Duration::ZERO, draws: 0 };
        for &p in pages {
            let page = CGPDFDocument::page(Some(&cg), p + 1).unwrap();
            let start = Instant::now();
            let mut first = None;
            t.draws += render_page(&page, width, v, &mut |_tile| {
                first.get_or_insert_with(|| start.elapsed());
            });
            t.total += start.elapsed();
            t.first += first.unwrap_or_default();
        }
        t
    })
}

fn page_px(page: &CGPDFPage, width: f64) -> [u32; 2] {
    let size = display_size(page);
    [width.round() as u32, (width * size.height / size.width).round() as u32]
}

/// Renders one page's tiles in `v`'s way, handing each tile's RGBA to `tile`. Returns how
/// many times the page was drawn.
fn render_page(page: &CGPDFPage, width: f64, v: Variant, tile: &mut dyn FnMut(Vec<u8>)) -> usize {
    let [pw, ph] = page_px(page, width);
    let t = v.tile;
    let (cols, rows) = (pw.div_ceil(t), ph.div_ceil(t));
    match v.mode {
        Mode::Tile => {
            for ty in 0..rows {
                for tx in 0..cols {
                    let (w, h) = (t.min(pw - tx * t), t.min(ph - ty * t));
                    tile(draw(page, [pw, ph], [tx * t, ty * t], [w, h], v));
                }
            }
            (rows * cols) as usize
        }
        Mode::Row | Mode::Page => {
            let band = if v.mode == Mode::Row { t } else { ph };
            let mut draws = 0;
            for y0 in (0..ph).step_by(band as usize) {
                let bh = band.min(ph - y0);
                let strip = draw(page, [pw, ph], [0, y0], [pw, bh], v);
                draws += 1;
                for ty in 0..bh.div_ceil(t) {
                    for tx in 0..cols {
                        let (x, y) = (tx * t, ty * t);
                        let (w, h) = (t.min(pw - x), t.min(bh - y));
                        let mut out = Vec::with_capacity((w * h * 4) as usize);
                        for row in y..y + h {
                            let start = ((row * pw + x) * 4) as usize;
                            out.extend_from_slice(&strip[start..start + (w * 4) as usize]);
                        }
                        tile(out);
                    }
                }
            }
            draws
        }
    }
}

/// Draws the `size` pixels at `origin` of the page rendered at `page_px`, as
/// `engine::coregraphics::Bitmap::draw` does.
fn draw(page: &CGPDFPage, page_px: [u32; 2], origin: [u32; 2], size: [u32; 2], v: Variant) -> Vec<u8> {
    let [w, h] = size;
    let mut rgba = vec![0u8; (w * h * 4) as usize];
    let srgb = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB })).unwrap();
    let alpha = if v.opaque { CGImageAlphaInfo::NoneSkipLast } else { CGImageAlphaInfo::PremultipliedLast };
    let ctx = unsafe {
        CGBitmapContextCreate(rgba.as_mut_ptr().cast(), w as usize, h as usize, 8, w as usize * 4, Some(&srgb), alpha.0)
    }
    .unwrap();
    let c = Some(&*ctx);
    CGContext::set_interpolation_quality(c, v.interp);
    CGContext::set_allows_font_smoothing(c, true);
    CGContext::set_should_smooth_fonts(c, true);
    let display = display_size(page);
    CGContext::translate_ctm(c, -(origin[0] as f64), h as f64 + origin[1] as f64 - page_px[1] as f64);
    CGContext::scale_ctm(c, page_px[0] as f64 / display.width, page_px[1] as f64 / display.height);
    let bounds = CGRect { origin: CGPoint { x: 0.0, y: 0.0 }, size: display };
    CGContext::clip_to_rect(c, bounds);
    CGContext::set_rgb_fill_color(c, 1.0, 1.0, 1.0, 1.0);
    CGContext::fill_rect(c, bounds);
    CGContext::concat_ctm(c, page_to_display(page));
    CGContext::draw_pdf_page(c, Some(page));
    drop(ctx);
    rgba
}

fn rotation(page: &CGPDFPage) -> i32 {
    CGPDFPage::rotation_angle(Some(page)).rem_euclid(360) / 90 * 90
}

fn display_size(page: &CGPDFPage) -> CGSize {
    let crop = CGPDFPage::box_rect(Some(page), CGPDFBox::CropBox).size;
    if rotation(page) % 180 == 90 { CGSize { width: crop.height, height: crop.width } } else { crop }
}

fn page_to_display(page: &CGPDFPage) -> CGAffineTransform {
    let CGRect { origin: CGPoint { x, y }, size: CGSize { width: w, height: h } } =
        CGPDFPage::box_rect(Some(page), CGPDFBox::CropBox);
    let (a, b, c, d, tx, ty) = match rotation(page) {
        90 => (0.0, -1.0, 1.0, 0.0, -y, w + x),
        180 => (-1.0, 0.0, 0.0, -1.0, w + x, h + y),
        270 => (0.0, 1.0, -1.0, 0.0, h + y, -x),
        _ => (1.0, 0.0, 0.0, 1.0, -x, -y),
    };
    CGAffineTransform { a, b, c, d, tx, ty }
}

/// How far each variant's pixels are from the current ones, and whether an opaque context
/// leaves the alpha byte at 255 (egui reads it).
fn quality(file: &str, pages: &[usize], width: f64) {
    println!("  pixel differences from current (whole page drawn once, so tiling is not a factor):");
    let whole = |v: Variant| Variant { tile: 1 << 20, mode: Mode::Tile, ..v };
    for v in variants().into_iter().filter(|v| v.mode == Mode::Tile && v.tile == BASE.tile && v.name != BASE.name) {
        let (mut differing, mut max, mut total, mut alpha_bad) = (0usize, 0u8, 0usize, 0usize);
        autoreleasepool(|_| {
            let (_pdf, cg) = open(file);
            for &p in pages {
                let page = CGPDFDocument::page(Some(&cg), p + 1).unwrap();
                let mut a = vec![];
                let mut b = vec![];
                render_page(&page, width, whole(BASE), &mut |t| a = t);
                render_page(&page, width, whole(v), &mut |t| b = t);
                for (pa, pb) in a.chunks_exact(4).zip(b.chunks_exact(4)) {
                    let d = (0..3).map(|i| pa[i].abs_diff(pb[i])).max().unwrap();
                    differing += (d > 2) as usize;
                    max = max.max(d);
                    alpha_bad += (pb[3] != 255) as usize;
                }
                total += a.len() / 4;
            }
        });
        println!(
            "    {:<24} {:>8.4}% of pixels differ by > 2   max diff {:>3}   alpha != 255: {}",
            v.name,
            100.0 * differing as f64 / total as f64,
            max,
            alpha_bad
        );
    }
}

fn qos_name(q: libc::qos_class_t) -> &'static str {
    use libc::qos_class_t::*;
    match q {
        QOS_CLASS_USER_INTERACTIVE => "user-interactive",
        QOS_CLASS_USER_INITIATED => "user-initiated",
        QOS_CLASS_DEFAULT => "default",
        QOS_CLASS_UTILITY => "utility",
        QOS_CLASS_BACKGROUND => "background",
        QOS_CLASS_UNSPECIFIED => "unspecified",
    }
}

/// The current variant on a spawned thread at several QoS classes. "inherited" is what
/// `std::thread::spawn` from the main thread gets, as the render thread does today.
fn qos(file: &str, pages: &[usize], width: f64, rounds: usize) {
    use libc::qos_class_t::*;
    println!("  thread QoS (current variant):");
    for class in [None, Some(QOS_CLASS_USER_INITIATED), Some(QOS_CLASS_UTILITY), Some(QOS_CLASS_BACKGROUND)] {
        let (file, n, pages) = (file.to_owned(), pages.len(), pages.to_vec());
        let (actual, best) = std::thread::spawn(move || {
            if let Some(class) = class {
                unsafe { libc::pthread_set_qos_class_self_np(class, 0) };
            }
            let mut actual = QOS_CLASS_UNSPECIFIED;
            unsafe { libc::pthread_get_qos_class_np(libc::pthread_self(), &mut actual, std::ptr::null_mut()) };
            let best = (0..rounds).map(|_| run_variant(&file, &pages, width, BASE).total).min().unwrap();
            (actual, best)
        })
        .join()
        .unwrap();
        println!(
            "    {:<24} {:>12.1} ms / page",
            if class.is_none() { format!("inherited ({})", qos_name(actual)) } else { qos_name(actual).to_owned() },
            ms(best) / n as f64
        );
    }
}

/// All pages on one thread, against half each on two threads with their own documents.
fn parallel(file: &str, pages: &[usize], width: f64, rounds: usize) {
    println!("  workers (current variant, wall time for all pages):");
    for workers in [1usize, 2, 3] {
        let best = (0..rounds)
            .map(|_| {
                let start = Instant::now();
                std::thread::scope(|s| {
                    for w in 0..workers {
                        let mine: Vec<usize> = pages.iter().copied().skip(w).step_by(workers).collect();
                        s.spawn(move || run_variant(file, &mine, width, BASE));
                    }
                });
                start.elapsed()
            })
            .min()
            .unwrap();
        println!("    {workers} worker(s) {:>12.1} ms / page", ms(best) / pages.len() as f64);
    }
}
