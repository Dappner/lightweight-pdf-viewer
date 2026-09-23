//! Background render thread: rasterizes pages with CoreGraphics (Preview's engine) and reads
//! document structure (outline, links) with PDFKit.
//!
//! Every document lives on this one thread. At most one document is kept open at a time:
//! touching a different document closes the previous one, which frees its per-document
//! caches (decoded images, fonts, parsed content streams).
//!
//! The UI replaces the whole render queue every frame with what it currently wants, so
//! work for pages that scrolled out of view is dropped without explicit cancellation.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};

use eframe::egui::{self, Pos2, Rect, Vec2, pos2, vec2};
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2_core_foundation::{CFRetained, CGAffineTransform, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGColorSpace, CGContext, CGImageAlphaInfo, CGInterpolationQuality,
    CGPDFBox, CGPDFDocument, CGPDFPage, kCGColorSpaceSRGB,
};
use objc2_foundation::NSURL;
use objc2_pdf_kit::{PDFActionGoTo, PDFActionURL, PDFDestination, PDFDocument, PDFOutline};

pub type DocId = u64;

/// Tile edge length in physical pixels.
pub const TILE: u32 = 512;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Key {
    /// `scale` is physical pixels per PDF point, in 1/256 steps (see [`scale_key`]).
    Tile { doc: DocId, page: u32, scale: u32, tx: u32, ty: u32 },
    Thumb { doc: DocId, page: u32, width: u32 },
    /// The page's link annotations.
    Links { doc: DocId, page: u32 },
}

impl Key {
    pub fn doc(&self) -> DocId {
        match *self {
            Key::Tile { doc, .. } | Key::Thumb { doc, .. } | Key::Links { doc, .. } => doc,
        }
    }

    fn page(&self) -> u32 {
        match *self {
            Key::Tile { page, .. } | Key::Thumb { page, .. } | Key::Links { page, .. } => page,
        }
    }
}

/// Where a link or outline entry points.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    /// `y` is the vertical position on the page, 0 = top and 1 = bottom, when specified.
    Page { page: u32, y: Option<f32> },
    Url(String),
}

#[derive(Clone, Debug)]
pub struct Link {
    /// The clickable area as fractions of the displayed page (top-left origin).
    pub rect: Rect,
    pub target: Target,
}

#[derive(Clone, Debug)]
pub struct OutlineEntry {
    pub title: String,
    pub depth: usize,
    pub target: Option<Target>,
}

pub fn scale_key(pixels_per_point: f32) -> u32 {
    (pixels_per_point * 256.0).round().max(1.0) as u32
}

/// Size in physical pixels of a page rendered at `scale` (a [`scale_key`]).
pub fn page_pixels(size: Vec2, scale: u32) -> [u32; 2] {
    let s = scale as f32 / 256.0;
    [(size.x * s).round().max(1.0) as u32, (size.y * s).round().max(1.0) as u32]
}

pub fn thumb_height(size: Vec2, width: u32) -> u32 {
    (width as f32 * size.y / size.x).round().max(1.0) as u32
}

/// A page placed inside a snapshot bitmap, in snapshot pixels.
pub struct SnapshotPage {
    pub page: u32,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

pub struct SnapshotJob {
    pub doc: DocId,
    pub size: [u32; 2],
    pub pages: Vec<SnapshotPage>,
}

pub enum Output {
    Opened { doc: DocId, result: Result<Vec<Vec2>, String> },
    Image { key: Key, size: [usize; 2], rgba: Vec<u8> },
    Failed(Key),
    /// QOI-compressed RGBA viewport image, kept while the tab is in the background.
    Snapshot { doc: DocId, size: [u32; 2], qoi: Vec<u8> },
    Links { doc: DocId, page: u32, links: Vec<Link> },
    Outline { doc: DocId, entries: Vec<OutlineEntry> },
}

impl Output {
    /// The queued key this output answers, if any.
    fn key(&self) -> Option<Key> {
        match *self {
            Output::Image { key, .. } | Output::Failed(key) => Some(key),
            Output::Links { doc, page, .. } => Some(Key::Links { doc, page }),
            _ => None,
        }
    }
}

enum Control {
    Open(DocId),
    Snapshot(SnapshotJob),
    Outline(DocId),
}

impl Control {
    fn doc(&self) -> DocId {
        match self {
            Control::Open(doc) | Control::Outline(doc) => *doc,
            Control::Snapshot(job) => job.doc,
        }
    }
}

#[derive(Default)]
struct Queue {
    control: VecDeque<Control>,
    wanted: Vec<Key>,
    busy: Vec<Key>,
    done: Vec<Output>,
    paths: HashMap<DocId, PathBuf>,
}

pub struct Renderer {
    shared: Arc<(Mutex<Queue>, Condvar)>,
}

impl Renderer {
    pub fn spawn(ctx: egui::Context) -> Self {
        let shared = Arc::new((Mutex::new(Queue::default()), Condvar::new()));
        let worker_shared = shared.clone();
        std::thread::Builder::new()
            .name("pdf-render".into())
            .spawn(move || worker(&worker_shared, &ctx))
            .expect("spawn render thread");
        Self { shared }
    }

    fn with_queue(&self, f: impl FnOnce(&mut Queue)) {
        let (lock, cvar) = &*self.shared;
        f(&mut lock.lock().unwrap());
        cvar.notify_one();
    }

    pub fn open(&self, doc: DocId, path: PathBuf) {
        self.with_queue(|q| {
            q.paths.insert(doc, path);
            q.control.push_back(Control::Open(doc));
        });
    }

    /// Drops the document; the worker closes it if it is open.
    pub fn forget(&self, doc: DocId) {
        self.with_queue(|q| {
            q.paths.remove(&doc);
            q.wanted.retain(|k| k.doc() != doc);
        });
    }

    pub fn snapshot(&self, job: SnapshotJob) {
        self.with_queue(|q| q.control.push_back(Control::Snapshot(job)));
    }

    /// Reads the document outline. Resolving entries to pages parses those pages, so this is
    /// only requested when the outline is shown.
    pub fn outline(&self, doc: DocId) {
        self.with_queue(|q| q.control.push_back(Control::Outline(doc)));
    }

    pub fn take_done(&self) -> Vec<Output> {
        std::mem::take(&mut self.shared.0.lock().unwrap().done)
    }

    /// Replaces the render queue. Keys already in flight, or finished but not yet taken,
    /// are skipped so nothing is rendered twice.
    pub fn set_wanted(&self, mut wanted: Vec<Key>) {
        self.with_queue(|q| {
            wanted.retain(|k| {
                !q.busy.contains(k) && !q.done.iter().any(|o| o.key() == Some(*k))
            });
            q.wanted = wanted;
        });
    }
}

enum Work {
    Control(Control),
    Render(Vec<Key>),
}

fn worker(shared: &(Mutex<Queue>, Condvar), ctx: &egui::Context) {
    let mut open: Option<(DocId, Document)> = None;

    loop {
        let (lock, cvar) = shared;
        let (work, path) = {
            let mut q = lock.lock().unwrap();
            loop {
                if open.as_ref().is_some_and(|(id, _)| !q.paths.contains_key(id)) {
                    open = None;
                }
                if let Some(c) = q.control.pop_front() {
                    match q.paths.get(&c.doc()) {
                        Some(path) => break (Work::Control(c), path.clone()),
                        None => continue,
                    }
                }
                if !q.wanted.is_empty() {
                    // Batch every queued job for the same page so it is only looked up once.
                    let first = q.wanted.remove(0);
                    let mut batch = vec![first];
                    q.wanted.retain(|k| {
                        let same = k.doc() == first.doc() && k.page() == first.page();
                        if same {
                            batch.push(*k);
                        }
                        !same
                    });
                    if let Some(path) = q.paths.get(&first.doc()).cloned() {
                        q.busy = batch.clone();
                        break (Work::Render(batch), path);
                    }
                }
                q = cvar.wait(q).unwrap();
            }
        };

        let doc_id = match &work {
            Work::Control(c) => c.doc(),
            Work::Render(batch) => batch[0].doc(),
        };
        // CoreGraphics may autorelease internally; drain per job so nothing piles up on
        // this long-lived thread.
        let outputs = objc2::rc::autoreleasepool(|_| {
            if open.as_ref().is_none_or(|(id, _)| *id != doc_id) {
                // Close the previous document before opening the next one.
                drop(open.take());
                open = open_document(&path).map(|d| (doc_id, d));
            }
            match (&open, work) {
                (Some((_, doc)), work) => run(doc, doc_id, work),
                (None, Work::Control(Control::Open(doc))) => vec![Output::Opened {
                    doc,
                    result: Err(format!("Could not open {}", path.display())),
                }],
                (None, Work::Control(Control::Snapshot(_))) => vec![],
                (None, Work::Control(Control::Outline(doc))) => vec![Output::Outline { doc, entries: vec![] }],
                (None, Work::Render(batch)) => batch.into_iter().map(Output::Failed).collect(),
            }
        });

        let mut q = lock.lock().unwrap();
        q.done.extend(outputs);
        q.busy.clear();
        drop(q);
        ctx.request_repaint();
    }
}

/// An open document: PDFKit for structure, and the CoreGraphics document it wraps for
/// rendering.
struct Document {
    pdf: Retained<PDFDocument>,
    cg: Retained<CGPDFDocument>,
}

fn open_document(path: &std::path::Path) -> Option<Document> {
    let url = NSURL::from_file_path(path)?;
    // SAFETY: `url` is a valid file URL.
    let pdf = unsafe { PDFDocument::initWithURL(PDFDocument::alloc(), &url) }?;
    let cg = unsafe { pdf.documentRef() }?;
    (CGPDFDocument::number_of_pages(Some(&cg)) > 0).then_some(Document { pdf, cg })
}

fn run(doc: &Document, doc_id: DocId, work: Work) -> Vec<Output> {
    match work {
        Work::Control(Control::Open(_)) => {
            let count = CGPDFDocument::number_of_pages(Some(&doc.cg));
            let sizes = (0..count)
                .map(|i| get_page(&doc.cg, i as u32).map_or(vec2(612.0, 792.0), |p| PageGeometry::of(&p).size()))
                .collect();
            vec![Output::Opened { doc: doc_id, result: Ok(sizes) }]
        }
        Work::Control(Control::Snapshot(job)) => render_snapshot(&doc.cg, &job).into_iter().collect(),
        Work::Control(Control::Outline(_)) => vec![Output::Outline { doc: doc_id, entries: outline(doc) }],
        Work::Render(batch) => {
            let Some(page) = get_page(&doc.cg, batch[0].page()) else {
                return batch.into_iter().map(Output::Failed).collect();
            };
            let geometry = PageGeometry::of(&page);
            batch
                .into_iter()
                .map(|key| match key {
                    Key::Links { doc: d, page: p } => {
                        Output::Links { doc: d, page: p, links: page_links(doc, p, &geometry) }
                    }
                    _ => render_key(&page, &geometry, key).unwrap_or(Output::Failed(key)),
                })
                .collect()
        }
    }
}

/// Resolves a PDFKit destination to a page and vertical position.
fn destination_target(doc: &Document, destination: &PDFDestination) -> Option<Target> {
    // SAFETY: plain PDFKit getters on objects owned by `doc`.
    let page = unsafe { destination.page() }?;
    let index = unsafe { doc.pdf.indexForPage(&page) } as u32;
    let point = unsafe { destination.point() };
    // PDFKit uses a huge sentinel (kPDFDestinationUnspecifiedValue) for "no position".
    let y = (point.y.abs() < 1e6)
        .then(|| get_page(&doc.cg, index).map(|p| PageGeometry::of(&p).to_fraction(point.x.min(1e6), point.y).y))
        .flatten()
        .map(|y| y.clamp(0.0, 1.0));
    Some(Target::Page { page: index, y })
}

fn page_links(doc: &Document, page: u32, geometry: &PageGeometry) -> Vec<Link> {
    // SAFETY: plain PDFKit getters on objects owned by `doc`.
    let Some(pdf_page) = (unsafe { doc.pdf.pageAtIndex(page as usize) }) else { return vec![] };
    let annotations = unsafe { pdf_page.annotations() };
    annotations
        .iter()
        .filter(|a| unsafe { a.r#type() }.is_some_and(|t| t.to_string() == "Link"))
        .filter_map(|a| {
            let action = unsafe { a.action() };
            let url = unsafe { a.URL() }
                .or_else(|| action.clone()?.downcast::<PDFActionURL>().ok().and_then(|u| unsafe { u.URL() }));
            let target = match url.and_then(|u| u.absoluteString()) {
                Some(url) => Target::Url(url.to_string()),
                None => {
                    let destination = unsafe { a.destination() }
                        .or_else(|| action?.downcast::<PDFActionGoTo>().ok().map(|g| unsafe { g.destination() }))?;
                    destination_target(doc, &destination)?
                }
            };
            let b = unsafe { a.bounds() };
            let corners = [
                geometry.to_fraction(b.origin.x, b.origin.y),
                geometry.to_fraction(b.origin.x + b.size.width, b.origin.y + b.size.height),
            ];
            Some(Link { rect: Rect::from_two_pos(corners[0], corners[1]), target })
        })
        .collect()
}

fn outline(doc: &Document) -> Vec<OutlineEntry> {
    fn walk(doc: &Document, node: &PDFOutline, depth: usize, out: &mut Vec<OutlineEntry>) {
        // SAFETY: plain PDFKit getters on objects owned by `doc`.
        for i in 0..unsafe { node.numberOfChildren() } {
            let Some(child) = (unsafe { node.childAtIndex(i) }) else { continue };
            let title = unsafe { child.label() }.map(|l| l.to_string()).unwrap_or_default();
            let destination = unsafe { child.destination() }.or_else(|| {
                unsafe { child.action() }?.downcast::<PDFActionGoTo>().ok().map(|g| unsafe { g.destination() })
            });
            let target = destination.and_then(|d| destination_target(doc, &d));
            out.push(OutlineEntry { title: title.trim().to_owned(), depth, target });
            walk(doc, &child, depth + 1, out);
        }
    }
    let mut entries = Vec::new();
    if let Some(root) = unsafe { doc.pdf.outlineRoot() } {
        walk(doc, &root, 0, &mut entries);
    }
    entries
}

/// `page` is zero-based; CoreGraphics numbers pages from 1.
fn get_page(doc: &CGPDFDocument, page: u32) -> Option<CFRetained<CGPDFPage>> {
    CGPDFDocument::page(Some(doc), page as usize + 1)
}

/// Crop box and rotation of a page, which together define what is displayed.
struct PageGeometry {
    crop: CGRect,
    /// Clockwise rotation in degrees: 0, 90, 180 or 270.
    rotation: i32,
}

impl PageGeometry {
    fn of(page: &CGPDFPage) -> Self {
        let crop = CGPDFPage::box_rect(Some(page), CGPDFBox::CropBox);
        let rotation = CGPDFPage::rotation_angle(Some(page)).rem_euclid(360) / 90 * 90;
        Self { crop, rotation }
    }

    /// Displayed size in PDF points, after rotation.
    fn size(&self) -> Vec2 {
        let (w, h) = (self.crop.size.width as f32, self.crop.size.height as f32);
        if self.rotation % 180 == 90 { vec2(h, w) } else { vec2(w, h) }
    }

    /// A point in PDF user space as fractions of the displayed page, top-left origin.
    fn to_fraction(&self, x: f64, y: f64) -> Pos2 {
        let t = self.page_to_display();
        let size = self.size();
        let u = t.a * x + t.c * y + t.tx;
        let v = t.b * x + t.d * y + t.ty;
        pos2(u as f32 / size.x, 1.0 - v as f32 / size.y)
    }

    /// Maps PDF user space onto the displayed page, with the origin at its bottom-left.
    fn page_to_display(&self) -> CGAffineTransform {
        let CGRect { origin: CGPoint { x, y }, size: CGSize { width: w, height: h } } = self.crop;
        let (a, b, c, d, tx, ty) = match self.rotation {
            90 => (0.0, -1.0, 1.0, 0.0, -y, w + x),
            180 => (-1.0, 0.0, 0.0, -1.0, w + x, h + y),
            270 => (0.0, 1.0, -1.0, 0.0, h + y, -x),
            _ => (1.0, 0.0, 0.0, 1.0, -x, -y),
        };
        CGAffineTransform { a, b, c, d, tx, ty }
    }
}

/// A bitmap context drawing into `rgba` (premultiplied RGBA, sRGB). CoreGraphics' origin is
/// the bottom-left corner.
fn bitmap_context(rgba: &mut [u8], width: u32, height: u32) -> Option<CFRetained<CGContext>> {
    let srgb = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }))?;
    // SAFETY: `rgba` holds width * height * 4 bytes and outlives the returned context,
    // which callers drop before reading the buffer.
    let ctx = unsafe {
        CGBitmapContextCreate(
            rgba.as_mut_ptr().cast(),
            width as usize,
            height as usize,
            8,
            width as usize * 4,
            Some(&srgb),
            CGImageAlphaInfo::PremultipliedLast.0,
        )
    }?;
    CGContext::set_interpolation_quality(Some(&ctx), CGInterpolationQuality::High);
    CGContext::set_allows_font_smoothing(Some(&ctx), true);
    CGContext::set_should_smooth_fonts(Some(&ctx), true);
    Some(ctx)
}

/// Draws the page on a white background, scaled to `page_px` pixels with its top-left
/// corner at `top_left` (bitmap pixels, measured from the top-left of a `bitmap_height`
/// tall bitmap).
fn draw_page(
    ctx: &CGContext,
    page: &CGPDFPage,
    geometry: &PageGeometry,
    page_px: [f64; 2],
    top_left: [f64; 2],
    bitmap_height: f64,
) {
    let display = geometry.size();
    let (dw, dh) = (display.x as f64, display.y as f64);
    let c = Some(ctx);
    CGContext::save_g_state(c);
    CGContext::translate_ctm(c, top_left[0], bitmap_height - top_left[1] - page_px[1]);
    CGContext::scale_ctm(c, page_px[0] / dw, page_px[1] / dh);
    let bounds = CGRect { origin: CGPoint { x: 0.0, y: 0.0 }, size: CGSize { width: dw, height: dh } };
    CGContext::clip_to_rect(c, bounds);
    CGContext::set_rgb_fill_color(c, 1.0, 1.0, 1.0, 1.0);
    CGContext::fill_rect(c, bounds);
    CGContext::concat_ctm(c, geometry.page_to_display());
    CGContext::draw_pdf_page(c, Some(page));
    CGContext::restore_g_state(c);
}

fn render_key(page: &CGPDFPage, geometry: &PageGeometry, key: Key) -> Option<Output> {
    let size = geometry.size();
    let (page_px, origin, w, h) = match key {
        Key::Tile { scale, tx, ty, .. } => {
            let page_px = page_pixels(size, scale);
            let origin = [tx * TILE, ty * TILE];
            let w = TILE.min(page_px[0].checked_sub(origin[0])?);
            let h = TILE.min(page_px[1].checked_sub(origin[1])?);
            (page_px, origin, w, h)
        }
        Key::Thumb { width, .. } => {
            let h = thumb_height(size, width);
            ([width, h], [0, 0], width, h)
        }
        Key::Links { .. } => return None,
    };
    let mut rgba = vec![0u8; (w * h * 4) as usize];
    let ctx = bitmap_context(&mut rgba, w, h)?;
    draw_page(
        &ctx,
        page,
        geometry,
        [page_px[0] as f64, page_px[1] as f64],
        [-(origin[0] as f64), -(origin[1] as f64)],
        h as f64,
    );
    drop(ctx);
    Some(Output::Image { key, size: [w as usize, h as usize], rgba })
}

fn render_snapshot(doc: &CGPDFDocument, job: &SnapshotJob) -> Option<Output> {
    let [w, h] = job.size;
    let mut rgba = vec![0u8; (w * h * 4) as usize];
    let ctx = bitmap_context(&mut rgba, w, h)?;
    for p in &job.pages {
        let Some(page) = get_page(doc, p.page) else { continue };
        draw_page(
            &ctx,
            &page,
            &PageGeometry::of(&page),
            [p.width as f64, p.height as f64],
            [p.x as f64, p.y as f64],
            h as f64,
        );
    }
    drop(ctx);
    let qoi = qoi::encode_to_vec(&rgba, w, h).ok()?;
    Some(Output::Snapshot { doc: job.doc, size: job.size, qoi })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_png(path: &str, rgba: &[u8], w: u32, h: u32) {
        let mut enc = png::Encoder::new(std::fs::File::create(path).unwrap(), w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.write_header().unwrap().write_image_data(rgba).unwrap();
    }

    /// Headless render check: `RENDER_TEST_PDF=<pdf> RENDER_TEST_OUT=<dir> cargo test`.
    #[test]
    fn renders_tiles_thumbs_and_snapshots() {
        let (Ok(pdf), Ok(out)) = (std::env::var("RENDER_TEST_PDF"), std::env::var("RENDER_TEST_OUT")) else {
            return;
        };
        let document = open_document(std::path::Path::new(&pdf)).expect("open");
        let entries = outline(&document);
        println!("outline: {} entries", entries.len());
        for e in entries.iter().take(4) {
            println!("  {}{} -> {:?}", "  ".repeat(e.depth), e.title, e.target);
        }
        let with_links = (0..CGPDFDocument::number_of_pages(Some(&document.cg)).min(60) as u32).find_map(|p| {
            let page = get_page(&document.cg, p)?;
            let g = PageGeometry::of(&page);
            let links = page_links(&document, p, &g);
            (!links.is_empty()).then_some((p, links))
        });
        if let Some((p, links)) = with_links {
            println!("page {} has {} links, e.g. {:?}", p + 1, links.len(), links[0]);
            assert!(links.iter().all(|l| l.rect.min.x >= -0.01 && l.rect.max.y <= 1.01), "link rects should be page fractions");
        }
        let doc = &document.cg;
        let page = get_page(doc, 1).expect("page");
        let geometry = PageGeometry::of(&page);
        let size = geometry.size();
        println!("page size {size:?}, rotation {}", geometry.rotation);

        for key in [
            Key::Tile { doc: 0, page: 1, scale: scale_key(0.8), tx: 0, ty: 0 },
            Key::Tile { doc: 0, page: 1, scale: scale_key(2.0), tx: 1, ty: 1 },
            Key::Thumb { doc: 0, page: 1, width: 108 },
        ] {
            let Some(Output::Image { key, size, rgba }) = render_key(&page, &geometry, key) else { panic!("render {key:?}") };
            let name = match key {
                Key::Tile { scale, tx, ty, .. } => format!("tile_{scale}_{tx}_{ty}"),
                Key::Thumb { .. } | Key::Links { .. } => "thumb".into(),
            };
            write_png(&format!("{out}/{name}.png"), &rgba, size[0] as u32, size[1] as u32);
        }

        let job = SnapshotJob {
            doc: 0,
            size: [700, 500],
            pages: vec![
                SnapshotPage { page: 0, x: 20, y: -200, width: 576, height: 432 },
                SnapshotPage { page: 1, x: 20, y: 244, width: 576, height: 432 },
            ],
        };
        let Some(Output::Snapshot { size, qoi, .. }) = render_snapshot(doc, &job) else { panic!("snapshot") };
        let (_, rgba) = qoi::decode_to_vec(&qoi).unwrap();
        println!("snapshot qoi {} KB (raw {} KB)", qoi.len() / 1024, rgba.len() / 1024);
        write_png(&format!("{out}/snapshot.png"), &rgba, size[0], size[1]);
    }
}
