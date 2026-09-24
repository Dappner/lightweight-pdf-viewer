//! Measurement spike for Cmd+F text search: what extracting, caching and highlighting text
//! would cost, headless. Not a regression benchmark.
//!
//!   cargo bench --bench search_spike -- a.pdf b.pdf [...]
//!
//! Every measurement runs in a fresh child process (with `MallocSpaceEfficient=1`, as the
//! default preset) so footprints don't bleed into each other. Modes, per file:
//!
//! - extract: reads every page's text through PDFKit starting mid-document and wrapping, as
//!   search would from the current page, reopening the document every N pages (never, 16,
//!   32, 256). Reports time to the first page, total, per-page latency, peak footprint, and
//!   footprint after releasing the document and then the text.
//! - analyze (with batch 32): cache sizes (text, UTF-16, normalized, offset mappings), and
//!   per query: matches, retained range bytes, scan time over the cache, time to first match.
//! - geometry: highlight rectangles (selectionForRange → selectionsByLine → boundsForPage)
//!   on pages never touched, already rendered, or whose text was already read.
//! - contention: tile render latency on one thread, alone vs. with extraction running.
//!
//! SPIKE_ONLY=extract|geometry|contention runs one mode.

use std::collections::HashMap;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use objc2::AnyThread;
use objc2::rc::{Retained, autoreleasepool};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGColorSpace, CGContext, CGImageAlphaInfo, CGPDFBox, CGPDFDocument, CGPDFPage,
    kCGColorSpaceSRGB,
};
use objc2_foundation::{NSRange, NSURL};
use objc2_pdf_kit::PDFDocument;

const BATCHES: [usize; 4] = [0, 16, 32, 256];

fn main() {
    let files: Vec<String> = std::env::args().skip(1).filter(|a| !a.starts_with("--")).collect();
    if let Ok(mode) = std::env::var("SPIKE_MODE") {
        child(&mode, &files[0]);
        return;
    }
    assert!(!files.is_empty(), "usage: cargo bench --bench search_spike -- a.pdf [...]");
    let only = std::env::var("SPIKE_ONLY").ok();
    let wants = |m: &str| only.as_deref().is_none_or(|o| o == m);
    for file in &files {
        let name = std::path::Path::new(file).file_name().unwrap().to_string_lossy();
        let pages = unsafe { open(file).pageCount() };
        println!("\n== {name} ({pages} pages)");
        if wants("extract") {
            for batch in BATCHES {
                run_child(file, "extract", &[("SPIKE_BATCH", batch.to_string())]);
            }
        }
        if wants("geometry") {
            run_child(file, "geometry", &[]);
        }
        if wants("contention") {
            run_child(file, "contention", &[]);
        }
    }
}

fn run_child(file: &str, mode: &str, env: &[(&str, String)]) {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.arg(file).env("SPIKE_MODE", mode).env("MallocSpaceEfficient", "1");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let status = cmd.status().expect("spawn child");
    if !status.success() {
        println!("  {mode}: child failed ({status})");
    }
}

fn child(mode: &str, file: &str) {
    match mode {
        "extract" => extract_mode(file, std::env::var("SPIKE_BATCH").unwrap().parse().unwrap()),
        "geometry" => geometry_mode(file),
        "contention" => contention_mode(file),
        _ => panic!("unknown mode {mode}"),
    }
}

// ---------------------------------------------------------------------------------------------
// Extraction

fn open(path: &str) -> Retained<PDFDocument> {
    let url = NSURL::from_file_path(path).expect("url");
    unsafe { PDFDocument::initWithURL(PDFDocument::alloc(), &url) }.expect("open")
}

struct PageText {
    text: String,
    utf16_len: usize,
    /// Since extraction began, when this page was done.
    at: Duration,
    took: Duration,
}

/// Mid-document first, wrapping, as a search from the current page would go.
fn scan_order(count: usize) -> Vec<usize> {
    let start = count / 2;
    (start..count).chain(0..start).collect()
}

/// Reads each page's text in `order`, reopening the document every `batch` pages (0: never).
/// Stops early if `stop` is set.
fn extract(path: &str, count: usize, order: &[usize], batch: usize, stop: &AtomicBool) -> Vec<Option<PageText>> {
    let mut texts: Vec<Option<PageText>> = (0..count).map(|_| None).collect();
    let mut doc: Option<Retained<PDFDocument>> = None;
    let mut in_doc = 0;
    let begin = Instant::now();
    for &i in order {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let t = Instant::now();
        let (text, utf16_len) = autoreleasepool(|_| {
            let d = doc.get_or_insert_with(|| open(path));
            let Some(page) = (unsafe { d.pageAtIndex(i) }) else { return (String::new(), 0) };
            match unsafe { page.string() } {
                Some(s) => (s.to_string(), s.length()),
                None => (String::new(), 0),
            }
        });
        in_doc += 1;
        if batch > 0 && in_doc >= batch {
            autoreleasepool(|_| drop(doc.take()));
            in_doc = 0;
        }
        texts[i] = Some(PageText { text, utf16_len, at: begin.elapsed(), took: t.elapsed() });
    }
    autoreleasepool(|_| drop(doc.take()));
    texts
}

fn extract_mode(path: &str, batch: usize) {
    let count = unsafe { open(path).pageCount() };
    let order = scan_order(count);
    let base = footprint_mb();
    let sampler = PeakSampler::start();
    let stop = AtomicBool::new(false);
    let texts = extract(path, count, &order, batch, &stop);
    let peak = sampler.finish();
    // `extract` has released its document by now.
    let with_cache = footprint_mb();

    let pages: Vec<&PageText> = texts.iter().flatten().collect();
    let total = pages.iter().map(|p| p.at).max().unwrap_or_default();
    let first = texts[order[0]].as_ref().map(|p| p.took).unwrap_or_default();
    let first_text = order.iter().filter_map(|&i| texts[i].as_ref()).find(|p| !p.text.trim().is_empty()).map(|p| p.at);
    let per_page: Vec<Duration> = pages.iter().map(|p| p.took).collect();
    let empty = pages.iter().filter(|p| p.text.trim().is_empty()).count();
    let label = if batch == 0 { "never".to_string() } else { batch.to_string() };
    println!(
        "  extract  reopen every {label:>5}: first page {:.1} ms, total {:.2} s ({:.2} ms/page), per page p50/p95/max {}, \
         peak +{:.1} MB, after release +{:.1} MB",
        ms(first),
        total.as_secs_f64(),
        total.as_secs_f64() * 1000.0 / count as f64,
        summary(per_page),
        peak - base,
        with_cache - base,
    );

    if batch == 32 {
        println!(
            "    empty pages: {empty}/{count}; first page with text after {}",
            first_text.map_or("never".into(), |d| format!("{:.1} ms", ms(d)))
        );
        analyze(&texts, &order);
    }
    drop(texts);
    println!("    footprint after dropping text too: +{:.1} MB", footprint_mb() - base);
}

// ---------------------------------------------------------------------------------------------
// Cache and matching

/// Normalized text, its offset runs, and how many entries a per-character map would need.
type Normalized = (String, Vec<(u32, u32)>, usize);

/// Lowercased, whitespace runs collapsed to one space. Returns the text and the mapping from
/// normalized byte offsets to UTF-16 offsets as runs of (normalized start, UTF-16 start):
/// within a run both advance together, one byte per unit.
fn normalize(text: &str) -> Normalized {
    let mut out = String::with_capacity(text.len());
    let mut runs: Vec<(u32, u32)> = Vec::new();
    let mut per_char_entries = 0;
    let mut u16_pos = 0u32;
    let mut prev_space = false;
    let push = |out: &mut String, s: &str, at16: u32, runs: &mut Vec<(u32, u32)>| {
        let n = out.len() as u32;
        let continues = runs.last().is_some_and(|&(ns, us)| n - ns == at16 - us);
        if !continues || s.len() != 1 {
            runs.push((n, at16));
        }
        out.push_str(s);
    };
    for c in text.chars() {
        let len16 = c.len_utf16() as u32;
        if c.is_whitespace() {
            if !prev_space {
                push(&mut out, " ", u16_pos, &mut runs);
                per_char_entries += 1;
            }
            prev_space = true;
        } else {
            prev_space = false;
            let mut buf = [0u8; 4];
            for l in c.to_lowercase() {
                push(&mut out, l.encode_utf8(&mut buf), u16_pos, &mut runs);
                per_char_entries += 1;
            }
        }
        u16_pos += len16;
    }
    (out, runs, per_char_entries)
}

fn analyze(texts: &[Option<PageText>], order: &[usize]) {
    let pages: Vec<&PageText> = texts.iter().flatten().collect();
    let utf8: usize = pages.iter().map(|p| p.text.len()).sum();
    let utf16: usize = pages.iter().map(|p| p.utf16_len * 2).sum();

    let t = Instant::now();
    let normalized: Vec<Normalized> =
        texts.iter().map(|p| p.as_ref().map_or_else(|| (String::new(), vec![], 0), |p| normalize(&p.text))).collect();
    let normalize_time = t.elapsed();
    let norm_bytes: usize = normalized.iter().map(|n| n.0.len()).sum();
    let run_bytes: usize = normalized.iter().map(|n| n.1.len() * 8).sum();
    let per_char_bytes: usize = normalized.iter().map(|n| n.2 * 4).sum();
    println!(
        "    cache: text {:.2} MB (UTF-16 {:.2} MB), normalized {:.2} MB (built in {:.0} ms); \
         offset map as u32 per char {:.2} MB, as runs {:.3} MB",
        mb(utf8),
        mb(utf16),
        mb(norm_bytes),
        ms(normalize_time),
        mb(per_char_bytes),
        mb(run_bytes),
    );

    // Words present on exactly 1 page and on ~20 pages, for rare and medium queries.
    let mut pages_with: HashMap<&str, (usize, usize)> = HashMap::new();
    for (i, (text, _, _)) in normalized.iter().enumerate() {
        let mut seen: Vec<&str> =
            text.split(|c: char| !c.is_alphabetic()).filter(|w| w.len() >= 7 && w.is_ascii()).collect();
        seen.sort_unstable();
        seen.dedup();
        for w in seen {
            let e = pages_with.entry(w).or_insert((0, i));
            e.0 += 1;
        }
    }
    let mut words: Vec<(&str, usize)> = pages_with.iter().map(|(w, &(n, _))| (*w, n)).collect();
    words.sort_unstable();
    let medium = words.iter().min_by_key(|(_, n)| n.abs_diff(20)).map(|(w, _)| w.to_string());
    let rare = words.iter().find(|(_, n)| *n == 1).map(|(w, _)| w.to_string());

    let mut queries = vec!["e".to_string(), "the".to_string()];
    queries.extend(medium.clone());
    queries.extend(rare);
    for q in &queries {
        let t = Instant::now();
        let mut matches = 0usize;
        let mut pages_hit = 0usize;
        for (text, _, _) in &normalized {
            let n = text.match_indices(q.as_str()).count();
            matches += n;
            pages_hit += (n > 0) as usize;
        }
        let scan = t.elapsed();
        let first = order
            .iter()
            .find(|&&i| normalized[i].0.contains(q.as_str()))
            .and_then(|&i| texts[i].as_ref())
            .map(|p| p.at);
        println!(
            "    query {q:>14}: {matches:>7} matches on {pages_hit:>4} pages, ranges {:.2} MB at 12 B each, \
             scan {:.2} ms, first match after {}",
            mb(matches * 12),
            ms(scan),
            first.map_or("none".into(), |d| format!("{:.1} ms", ms(d))),
        );
    }
    if let Some(word) = medium {
        let mut slowest = Duration::ZERO;
        for end in 1..=word.len() {
            let t = Instant::now();
            let n: usize = normalized.iter().map(|(text, _, _)| text.match_indices(&word[..end]).count()).sum();
            std::hint::black_box(n);
            slowest = slowest.max(t.elapsed());
        }
        println!("    typing {word:?} letter by letter: slowest full rescan {:.2} ms", ms(slowest));
    }
}

// ---------------------------------------------------------------------------------------------
// Highlight geometry

/// UTF-16 ranges of "the" (ASCII case-insensitive) in `text`.
fn ranges_of_the(text: &str) -> Vec<NSRange> {
    let units: Vec<u16> =
        text.encode_utf16().map(|u| if u < 128 { (u as u8).to_ascii_lowercase() as u16 } else { u }).collect();
    let needle: Vec<u16> = "the".encode_utf16().collect();
    units
        .windows(needle.len())
        .enumerate()
        .filter(|(_, w)| *w == needle)
        .map(|(i, _)| NSRange::new(i, needle.len()))
        .collect()
}

fn geometry_mode(path: &str) {
    let count = unsafe { open(path).pageCount() };
    let order = scan_order(count);
    let texts = extract(path, count, &order, 32, &AtomicBool::new(false));
    let mut candidates: Vec<(usize, Vec<NSRange>)> = texts
        .iter()
        .enumerate()
        .filter_map(|(i, p)| Some((i, ranges_of_the(&p.as_ref()?.text))))
        .filter(|(_, r)| !r.is_empty())
        .collect();
    // 90 pages spread over the document, dealt round-robin into three groups.
    let step = (candidates.len() / 90).max(1);
    candidates = candidates.into_iter().step_by(step).take(90).collect();
    drop(texts);

    #[derive(Clone, Copy)]
    enum Prep {
        Fresh,
        Rendered,
        TextRead,
    }
    let doc = open(path);
    let cg = unsafe { doc.documentRef() }.expect("documentRef");
    let mut results: [(Vec<Duration>, Vec<Duration>, usize, usize); 3] = Default::default();
    let mut prep_times: Vec<Duration> = vec![];
    for (n, (page, ranges)) in candidates.iter().enumerate() {
        let prep = [Prep::Fresh, Prep::Rendered, Prep::TextRead][n % 3];
        autoreleasepool(|_| {
            let t = Instant::now();
            match prep {
                Prep::Fresh => {}
                Prep::Rendered => {
                    for (tx, ty) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                        render_tile(&cg, *page, 1600.0, tx, ty);
                    }
                }
                Prep::TextRead => {
                    let p = unsafe { doc.pageAtIndex(*page) }.unwrap();
                    std::hint::black_box(unsafe { p.string() });
                }
            }
            if !matches!(prep, Prep::Fresh) {
                prep_times.push(t.elapsed());
            }
            let t = Instant::now();
            let p = unsafe { doc.pageAtIndex(*page) }.unwrap();
            let mut first = None;
            let mut rects = 0;
            for r in ranges {
                let Some(sel) = (unsafe { p.selectionForRange(*r) }) else { continue };
                let lines = unsafe { sel.selectionsByLine() };
                for l in 0..lines.count() {
                    let b = unsafe { lines.objectAtIndex(l).boundsForPage(&p) };
                    std::hint::black_box(b);
                    rects += 1;
                }
                first.get_or_insert(t.elapsed());
            }
            let slot = &mut results[n % 3];
            slot.0.push(first.unwrap_or_default());
            slot.1.push(t.elapsed());
            slot.2 += ranges.len();
            slot.3 += rects;
        });
    }
    for (name, (first, all, matches, rects)) in
        ["never touched", "already rendered", "text already read"].iter().zip(results)
    {
        println!(
            "  geometry {name:>17}: first rect {} | all matches on page {} ({:.0} matches, {:.0} rects per page avg)",
            summary(first),
            summary(all.clone()),
            matches as f64 / all.len() as f64,
            rects as f64 / all.len() as f64,
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Contention

/// Renders one 512 px tile of `page` at `width_px` across the page; (tx, ty) from the top-left.
fn render_tile(cg: &CGPDFDocument, index: usize, width_px: f64, tx: u32, ty: u32) {
    let Some(page) = CGPDFDocument::page(Some(cg), index + 1) else { return };
    let crop = CGPDFPage::box_rect(Some(&page), CGPDFBox::CropBox);
    let s = width_px / crop.size.width;
    let mut rgba = vec![0u8; 512 * 512 * 4];
    let srgb = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB })).unwrap();
    let ctx = unsafe {
        CGBitmapContextCreate(
            rgba.as_mut_ptr().cast(),
            512,
            512,
            8,
            2048,
            Some(&srgb),
            CGImageAlphaInfo::PremultipliedLast.0,
        )
    }
    .unwrap();
    let c = Some(&*ctx);
    let page_h = crop.size.height * s;
    CGContext::translate_ctm(c, -(tx as f64 * 512.0), -(page_h - 512.0 - ty as f64 * 512.0));
    CGContext::scale_ctm(c, s, s);
    CGContext::set_rgb_fill_color(c, 1.0, 1.0, 1.0, 1.0);
    CGContext::fill_rect(c, crop);
    CGContext::draw_pdf_page(c, Some(&page));
    drop(ctx);
    std::hint::black_box(&rgba);
}

/// Renders the top four tiles of each page in `pages`, reopening every 32 pages as the
/// balanced preset does; returns per-page latency.
fn render_pages(path: &str, pages: &[usize]) -> Vec<Duration> {
    let mut times = vec![];
    for chunk in pages.chunks(32) {
        autoreleasepool(|_| {
            let doc = open(path);
            let cg = unsafe { doc.documentRef() }.unwrap();
            for &p in chunk {
                let t = Instant::now();
                for (tx, ty) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                    render_tile(&cg, p, 2000.0, tx, ty);
                }
                times.push(t.elapsed());
            }
        });
    }
    times
}

fn contention_mode(path: &str) {
    let count = unsafe { open(path).pageCount() };
    // A stretch a reader would scroll through, well away from where extraction starts.
    let pages: Vec<usize> = (count / 5..count / 5 + 120).filter(|&p| p < count).collect();
    let alone = render_pages(path, &pages);
    println!("  render alone:            per page (4 tiles) p50/p95/max {}", summary(alone));

    let stop = Arc::new(AtomicBool::new(false));
    let extracted = Arc::new(AtomicU64::new(0));
    let worker = {
        let (path, stop, extracted) = (path.to_string(), stop.clone(), extracted.clone());
        std::thread::spawn(move || {
            let order = scan_order(count);
            while !stop.load(Ordering::Relaxed) {
                let texts = extract(&path, count, &order, 32, &stop);
                extracted.fetch_add(texts.iter().flatten().count() as u64, Ordering::Relaxed);
            }
        })
    };
    std::thread::sleep(Duration::from_millis(200));
    let t = Instant::now();
    let during = render_pages(path, &pages);
    let elapsed = t.elapsed();
    stop.store(true, Ordering::Relaxed);
    worker.join().unwrap();
    println!(
        "  render during extraction: per page (4 tiles) p50/p95/max {}  (extraction ran ~{:.0} pages/s)",
        summary(during),
        extracted.load(Ordering::Relaxed) as f64 / (elapsed.as_secs_f64() + 0.2),
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

/// "p50 / p95 / max" in milliseconds.
fn summary(mut samples: Vec<Duration>) -> String {
    if samples.is_empty() {
        return "-".into();
    }
    samples.sort_unstable();
    let at = |q: f64| ms(samples[((samples.len() - 1) as f64 * q).round() as usize]);
    format!("{:.1} / {:.1} / {:.1} ms", at(0.5), at(0.95), at(1.0))
}
