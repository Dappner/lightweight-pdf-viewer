use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use objc2::AnyThread;
use objc2::rc::{Retained, autoreleasepool};
use objc2_core_foundation::CGPoint;
use objc2_foundation::{NSRange, NSString, NSURL};
use objc2_pdf_kit::{PDFDisplayBox, PDFDocument, PDFPage};

const SAMPLE_PAGES: usize = 60;
const HIT_POINTS: usize = 200;
/// As the app's text worker does (`search::REOPEN_PAGES`), draining the autorelease pool
/// after every page.
const REOPEN_EVERY: usize = 16;

pub fn main() {
    let files: Vec<String> = std::env::args().skip(1).filter(|a| !a.starts_with("--")).collect();
    if std::env::var("SPIKE_CHILD").is_ok() {
        child(&files[0]);
        return;
    }
    assert!(!files.is_empty(), "usage: cargo bench --bench text_spike -- a.pdf [...]");
    for file in &files {
        let name = std::path::Path::new(file).file_name().unwrap().to_string_lossy();
        let pages = unsafe { open(file).pageCount() };
        println!("\n== {name} ({pages} pages)");
        let status = Command::new(std::env::current_exe().unwrap())
            .arg(file)
            .env("SPIKE_CHILD", "1")
            .env("MallocSpaceEfficient", "1")
            .status()
            .expect("spawn child");
        if !status.success() {
            println!("  child failed ({status})");
        }
    }
}

fn open(path: &str) -> Retained<PDFDocument> {
    let url = NSURL::from_file_path(path).expect("url");
    unsafe { PDFDocument::initWithURL(PDFDocument::alloc(), &url) }.expect("open")
}

/// Every UTF-16 unit's box on `page`, whose string is `text`, as the app reads them: from a
/// selection of its character, both units of a surrogate pair sharing one
/// (`characterBoundsAtIndex` counts glyphs, not units of the string, so its boxes drift from
/// the text); and how many are empty.
fn char_bounds(page: &PDFPage, text: &NSString) -> (Vec<[f32; 4]>, usize) {
    let len = text.length();
    let mut empty = 0;
    let mut rects = Vec::with_capacity(len);
    while rects.len() < len {
        let i = rects.len();
        let pair = (0xD800..0xDC00).contains(&text.characterAtIndex(i))
            && i + 1 < len
            && (0xDC00..0xE000).contains(&text.characterAtIndex(i + 1));
        let units = if pair { 2 } else { 1 };
        let b = unsafe { page.selectionForRange(NSRange::new(i, units)) }
            .map_or(Default::default(), |s| unsafe { s.boundsForPage(page) });
        if b.size.width <= 0.0 || b.size.height <= 0.0 {
            empty += units;
        }
        let rect = [b.origin.x as f32, b.origin.y as f32, b.size.width as f32, b.size.height as f32];
        rects.extend(std::iter::repeat_n(rect, units));
    }
    (rects, empty)
}

fn child(path: &str) {
    let count = unsafe { open(path).pageCount() };
    let step = (count / SAMPLE_PAGES).max(1);
    let sample: Vec<usize> = (0..count).step_by(step).take(SAMPLE_PAGES).collect();

    let mut string_t = vec![];
    let mut bounds_t = vec![];
    let mut bounds_per_char = vec![];
    let mut lines_t = vec![];
    let mut hit_t = vec![];
    let (mut chars, mut empty, mut lines, mut hits_on_text) = (0usize, 0usize, 0usize, 0usize);
    let mut max_chars = 0;
    let mut text_pages = 0;
    for chunk in sample.chunks(REOPEN_EVERY) {
        let doc = autoreleasepool(|_| open(path));
        for &i in chunk {
            autoreleasepool(|_| {
                let page = unsafe { doc.pageAtIndex(i) }.unwrap();
                let t = Instant::now();
                let text = unsafe { page.string() };
                let len = text.as_ref().map_or(0, |s| s.length());
                string_t.push(t.elapsed());
                let Some(text) = text.filter(|_| len > 0) else { return };
                text_pages += 1;

                let t = Instant::now();
                let (rects, e) = char_bounds(&page, &text);
                let took = t.elapsed();
                std::hint::black_box(&rects);
                bounds_t.push(took);
                bounds_per_char.push(took / len as u32);
                chars += len;
                max_chars = max_chars.max(len);
                empty += e;

                let t = Instant::now();
                if let Some(sel) = unsafe { page.selectionForRange(NSRange::new(0, len)) } {
                    let by_line = unsafe { sel.selectionsByLine() };
                    for l in 0..by_line.count() {
                        std::hint::black_box(unsafe { by_line.objectAtIndex(l).boundsForPage(&page) });
                    }
                    lines += by_line.count();
                }
                lines_t.push(t.elapsed());

                let b = unsafe { page.boundsForBox(PDFDisplayBox::CropBox) };
                for n in 0..HIT_POINTS {
                    // A deterministic scatter over the page.
                    let fx = (n as f64 * 0.618_034).fract();
                    let fy = (n as f64 * 0.414_214).fract();
                    let p = CGPoint { x: b.origin.x + fx * b.size.width, y: b.origin.y + fy * b.size.height };
                    let t = Instant::now();
                    let hit = unsafe { page.characterIndexAtPoint(p) };
                    hit_t.push(t.elapsed());
                    hits_on_text += (hit >= 0) as usize;
                }
            });
        }
        autoreleasepool(|_| drop(doc));
    }
    let n = text_pages.max(1);
    println!(
        "  sample: {} pages, {text_pages} with text; {:.0} chars/page avg, {max_chars} max; \
         {:.0}% of chars have empty bounds; {:.0} lines/page",
        sample.len(),
        chars as f64 / n as f64,
        100.0 * empty as f64 / chars.max(1) as f64,
        lines as f64 / n as f64,
    );
    println!("  string (first touch)   p50/p95/max {}", summary(string_t));
    println!("  bounds, every char     p50/p95/max {}", summary(bounds_t));
    println!("    per char             p50/p95/max {}", summary_us(bounds_per_char));
    println!("  lines, whole page      p50/p95/max {}", summary(lines_t));
    println!(
        "  hit test (PDFKit)      p50/p95/max {} per call; {:.0}% of points on a character",
        summary_us(hit_t),
        100.0 * hits_on_text as f64 / (n * HIT_POINTS) as f64,
    );

    whole_document(path, count);
}

/// Builds every page's text layer, as a worker would for the whole document.
fn whole_document(path: &str, count: usize) {
    let base = footprint_mb();
    let sampler = PeakSampler::start();
    let t = Instant::now();
    let mut last_report = Instant::now();
    let mut layers: Vec<(String, Vec<[f32; 4]>)> = Vec::with_capacity(count);
    for start in (0..count).step_by(REOPEN_EVERY) {
        let doc = autoreleasepool(|_| open(path));
        for i in start..(start + REOPEN_EVERY).min(count) {
            autoreleasepool(|_| {
                let page = unsafe { doc.pageAtIndex(i) }.unwrap();
                let layer = match unsafe { page.string() } {
                    Some(text) => (text.to_string(), char_bounds(&page, &text).0),
                    None => (String::new(), vec![]),
                };
                layers.push(layer);
            });
        }
        autoreleasepool(|_| drop(doc));
        if last_report.elapsed() > Duration::from_secs(10) {
            eprintln!("  … {}/{count} pages", layers.len());
            last_report = Instant::now();
        }
    }
    let total = t.elapsed();
    let peak = sampler.finish();
    let text_bytes: usize = layers.iter().map(|(t, _)| t.len()).sum();
    let rect_bytes: usize = layers.iter().map(|(_, r)| r.len() * 16).sum();
    println!(
        "  whole document: {:.2} s ({:.1} ms/page), peak +{:.1} MB; layer {:.2} MB text + {:.2} MB rects \
         ({:.0} KB/page avg)",
        total.as_secs_f64(),
        ms(total) / count.max(1) as f64,
        peak - base,
        mb(text_bytes),
        mb(rect_bytes),
        (text_bytes + rect_bytes) as f64 / 1024.0 / count.max(1) as f64,
    );
}

// ---------------------------------------------------------------------------------------------
// Measurement helpers

struct PeakSampler {
    stop: Arc<AtomicBool>,
    peak: Arc<AtomicU64>,
    thread: std::thread::JoinHandle<()>,
}

impl PeakSampler {
    fn start() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let peak = Arc::new(AtomicU64::new(footprint_mb().to_bits()));
        let thread = {
            let (stop, peak) = (stop.clone(), peak.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let now = footprint_mb();
                    if now > f64::from_bits(peak.load(Ordering::Relaxed)) {
                        peak.store(now.to_bits(), Ordering::Relaxed);
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
            })
        };
        Self { stop, peak, thread }
    }

    fn finish(self) -> f64 {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.join().unwrap();
        f64::from_bits(self.peak.load(Ordering::Relaxed)).max(footprint_mb())
    }
}

fn footprint_mb() -> f64 {
    let mut info: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        libc::proc_pid_rusage(libc::getpid(), libc::RUSAGE_INFO_V2, &mut info as *mut _ as *mut libc::rusage_info_t)
    };
    assert_eq!(ok, 0, "proc_pid_rusage failed");
    info.ri_phys_footprint as f64 / 1_048_576.0
}

fn mb(bytes: usize) -> f64 {
    bytes as f64 / 1_048_576.0
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn quantiles(mut samples: Vec<Duration>) -> Option<[Duration; 3]> {
    if samples.is_empty() {
        return None;
    }
    samples.sort_unstable();
    let at = |q: f64| samples[((samples.len() - 1) as f64 * q).round() as usize];
    Some([at(0.5), at(0.95), at(1.0)])
}

/// "p50 / p95 / max" in milliseconds.
fn summary(samples: Vec<Duration>) -> String {
    quantiles(samples).map_or("-".into(), |[a, b, c]| format!("{:.2} / {:.2} / {:.2} ms", ms(a), ms(b), ms(c)))
}

/// "p50 / p95 / max" in microseconds.
fn summary_us(samples: Vec<Duration>) -> String {
    let us = |d: Duration| d.as_secs_f64() * 1e6;
    quantiles(samples).map_or("-".into(), |[a, b, c]| format!("{:.2} / {:.2} / {:.2} µs", us(a), us(b), us(c)))
}
