//! Headless check of the search worker's memory and latency on real documents.
//!
//!   cargo bench --bench search -- a.pdf b.pdf [...]
//!
//! Each file runs in fresh child processes (with `MallocSpaceEfficient=1`, as the default
//! preset), as if relaunching the app between them:
//!
//! - without the disk index, with the normal cache budget and with a 1 MB one that forces
//!   pages out of the memory cache;
//! - with an empty disk index (cold), which the run fills;
//! - with that index (warm), with both budgets.
//!
//! Each run searches for "the" from the middle of the document, steps through
//! matches and changes the query twice. Then it stops the worker (as a tab left in the
//! background does) and gets highlights again, stops it again partway through a new scan and
//! resumes that scan, and finally closes the search.
//!
//! It fails if the cache goes over its budget, a warm run's first scan extracts pages with
//! PDFKit instead of reading them from the index, the process footprint goes over the budget
//! plus [`PDFKIT_ALLOWANCE`], counts differ after the query changes back or after a resumed
//! scan, a stopped worker doesn't exit, or the footprint doesn't settle within
//! [`RESIDUAL_TOLERANCE`] of where it started.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use eframe::egui;
use lightweight_pdf_viewer::search::{CACHE_BUDGET, Search};
use lightweight_pdf_viewer::search_index::{Limits, Store};
use objc2::AnyThread;
use objc2_foundation::NSURL;
use objc2_pdf_kit::PDFDocument;

/// PDFKit's own memory while reading, on top of the cache (measured at 5–20 MB).
const PDFKIT_ALLOWANCE: f64 = 32.0;
/// How far above the starting footprint the process may settle after the search closes.
const RESIDUAL_TOLERANCE: f64 = 8.0;
const SMALL_BUDGET: usize = 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(120);

fn main() {
    let files: Vec<String> = std::env::args().skip(1).filter(|a| !a.starts_with("--")).collect();
    if let Ok(budget) = std::env::var("SEARCH_BUDGET") {
        let index = std::env::var("SEARCH_INDEX").ok().filter(|s| !s.is_empty()).map(PathBuf::from);
        let warm = std::env::var("SEARCH_WARM").is_ok();
        let ok = run(&files[0], budget.parse().unwrap(), index, warm);
        std::process::exit(if ok { 0 } else { 1 });
    }
    assert!(!files.is_empty(), "usage: cargo bench --bench search -- a.pdf [...]");
    let index = std::env::temp_dir().join(format!("lightweight-pdf-bench-index-{}", std::process::id()));
    let mut failed = 0;
    for file in &files {
        println!("\n== {}", std::path::Path::new(file).file_name().unwrap().to_string_lossy());
        let _ = std::fs::remove_dir_all(&index);
        let runs = [
            (CACHE_BUDGET, None, false),
            (SMALL_BUDGET, None, false),
            (CACHE_BUDGET, Some(&index), false),
            (CACHE_BUDGET, Some(&index), true),
            (SMALL_BUDGET, Some(&index), true),
        ];
        for (budget, index, warm) in runs {
            let mut child = Command::new(std::env::current_exe().unwrap());
            child
                .arg(file)
                .env("SEARCH_BUDGET", budget.to_string())
                .env("SEARCH_INDEX", index.map_or(PathBuf::new(), |p| p.clone()))
                .env("MallocSpaceEfficient", "1");
            if warm {
                child.env("SEARCH_WARM", "1");
            }
            failed += !child.status().expect("spawn child").success() as usize;
        }
    }
    let _ = std::fs::remove_dir_all(&index);
    if failed > 0 {
        println!("\n{failed} run(s) failed");
        std::process::exit(1);
    }
}

fn page_count(path: &str) -> usize {
    let url = NSURL::from_file_path(path).expect("url");
    let doc = unsafe { PDFDocument::initWithURL(PDFDocument::alloc(), &url) }.expect("open");
    unsafe { doc.pageCount() }
}

/// Runs the scenario in this process, saving page text in `index` if given; returns whether
/// every check passed. `warm`: the index should already hold every page.
fn run(path: &str, budget: usize, index: Option<PathBuf>, warm: bool) -> bool {
    let store = index.map(|dir| Store { dir, limits: Limits::default() });
    let pages = page_count(path);
    let start = pages / 2;
    let ctx = egui::Context::default();
    let base = footprint_mb();
    let sampler = PeakSampler::start();
    let mut max_cache = 0;
    let mut errors: Vec<String> = Vec::new();

    let mut search = Search::with_budget(&ctx, PathBuf::from(path), pages, start, budget, store.clone());
    let t = Instant::now();
    search.set_query("the", (start, 0.0));
    let mut first_hit = None;
    let mut geometry = None;
    let full = pump(&ctx, &mut search, start, &mut max_cache, |s, elapsed| {
        if first_hit.is_none() && s.current().is_some() {
            first_hit = Some(elapsed);
        }
        if let Some(hit) = s.current() {
            s.want_geometry(&[hit.page]);
            if geometry.is_none() && s.take_reveal().is_some() {
                geometry = Some(elapsed - first_hit.unwrap());
            }
        }
        s.stats().counted == pages && geometry.is_some()
    });
    let scan = t.elapsed();
    let matches = search.stats().matches;
    let first_reads = (search.stats().loaded, search.stats().extracted);
    if warm && first_reads.1 > 0 {
        errors.push(format!("warm first scan extracted {} pages with PDFKit", first_reads.1));
    }
    if !full {
        errors.push("first scan timed out".into());
    }

    // Stepping through matches once everything is counted takes no waiting.
    let mut steps = Vec::new();
    for _ in 0..50 {
        let before = search.current();
        let t = Instant::now();
        search.step(true, (start, 0.0));
        steps.push(t.elapsed());
        if search.current() == before {
            errors.push("step didn't move".into());
            break;
        }
    }

    let t = Instant::now();
    search.set_query("qzxj", (start, 0.0));
    pump(&ctx, &mut search, start, &mut max_cache, |s, _| s.stats().counted == pages);
    let requery = t.elapsed();
    let t = Instant::now();
    search.set_query("the", (start, 0.0));
    pump(&ctx, &mut search, start, &mut max_cache, |s, _| s.stats().counted == pages);
    let back = t.elapsed();
    if search.stats().matches != matches {
        errors.push(format!("{} matches after changing the query back, {matches} before", search.stats().matches));
    }
    let stats = search.stats();

    // Stopped in the background: only the UI's results stay.
    let probe = search.probe();
    search.suspend();
    if !wait_exit(&probe) {
        errors.push("suspended worker still running after 5 s".into());
    }
    std::thread::sleep(Duration::from_millis(300));
    let suspended = footprint_mb();
    // Highlights for a page not yet covered start a new worker, which reads just that page.
    let far = search.current().map_or(0, |h| h.page);
    search.step(false, (start, 0.0));
    let t = Instant::now();
    let page = search.current().map_or(0, |h| h.page);
    let highlighted = pump(&ctx, &mut search, start, &mut max_cache, |s, _| {
        s.want_geometry(&[page, far]);
        s.highlights(page).next().is_some()
    });
    let rehighlight = t.elapsed();
    if !highlighted {
        errors.push("no highlights after resuming".into());
    }
    let resumed_cache = search.stats().cache_bytes;

    // Stopped partway through a scan, then back in front: the scan carries on from there.
    search.suspend();
    search.set_query("and", (start, 0.0));
    pump(&ctx, &mut search, start, &mut max_cache, |s, _| s.stats().counted >= pages / 3);
    let partial = search.stats().counted;
    search.suspend();
    search.pause(true);
    let t = Instant::now();
    search.pause(false);
    pump(&ctx, &mut search, start, &mut max_cache, |s, _| s.stats().counted == pages);
    let rest = t.elapsed();
    let and_resumed = search.stats().matches;
    search.set_query("the", (start, 0.0));
    search.set_query("and", (start, 0.0));
    pump(&ctx, &mut search, start, &mut max_cache, |s, _| s.stats().counted == pages);
    if search.stats().matches != and_resumed {
        errors.push(format!("{and_resumed} matches of \"and\" after resuming, {} in one go", search.stats().matches));
    }
    let peak = sampler.finish();
    let reads = (search.stats().loaded, search.stats().extracted);

    let probe = search.probe();
    let t = Instant::now();
    drop(search);
    if !wait_exit(&probe) {
        errors.push("worker still running 5 s after close".into());
    }
    let exit = t.elapsed();
    std::thread::sleep(Duration::from_millis(300));
    let after = footprint_mb();

    if max_cache > budget {
        errors.push(format!("cache reached {:.2} MB, over its {:.2} MB budget", mb(max_cache), mb(budget)));
    }
    if peak - base > mb(budget) + PDFKIT_ALLOWANCE {
        errors.push(format!("peak +{:.1} MB, over budget + {PDFKIT_ALLOWANCE} MB", peak - base));
    }
    if after - base > RESIDUAL_TOLERANCE {
        errors.push(format!("footprint settled +{:.1} MB above the start", after - base));
    }

    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    steps.sort_unstable();
    let index_mb = store.as_ref().map_or(0.0, |s| dir_mb(&s.dir));
    let label = match (&store, warm) {
        (None, _) => "no index",
        (Some(_), false) => "cold index",
        (Some(_), true) => "warm index",
    };
    println!(
        "  {label}, budget {:>4.1} MB: {pages} pages, {matches} matches of \"the\" (text on {} pages)\n    \
         first match {} ms, its highlight +{} ms, full scan {:.2} s; step p50/max {:.1}/{:.1} µs\n    \
         new query rescanned in {:.0} ms, back to \"the\" in {:.0} ms\n    \
         stopped: +{:.1} MB; highlights again in {:.1} ms (cache {:.0} KB); \
         scan stopped at {partial} pages resumed in {:.2} s\n    \
         cache max {:.2} MB, peak +{:.1} MB; closed in {:.1} ms, settled +{:.1} MB\n    \
         page reads: first scan {} from index / {} extracted, whole run {} / {}; index {:.2} MB on disk",
        mb(budget),
        stats.with_text,
        first_hit.map_or("-".into(), |d| format!("{:.1}", ms(d))),
        geometry.map_or("-".into(), |d| format!("{:.1}", ms(d))),
        scan.as_secs_f64(),
        ms(steps[steps.len() / 2]) * 1000.0,
        ms(*steps.last().unwrap()) * 1000.0,
        ms(requery),
        ms(back),
        suspended - base,
        ms(rehighlight),
        resumed_cache as f64 / 1024.0,
        rest.as_secs_f64(),
        mb(max_cache),
        peak - base,
        ms(exit),
        after - base,
        first_reads.0,
        first_reads.1,
        reads.0,
        reads.1,
        index_mb,
    );
    for e in &errors {
        println!("    FAIL: {e}");
    }
    errors.is_empty()
}

/// Waits up to 5 s for a stopped worker's thread to exit; returns whether it did.
fn wait_exit(probe: &lightweight_pdf_viewer::search::WorkerProbe) -> bool {
    let t = Instant::now();
    while probe.running() && t.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(1));
    }
    !probe.running()
}

/// Feeds the search results as a frame would until `done` or the timeout; returns whether
/// `done` held. `done` gets the time since the pump began.
fn pump(
    ctx: &egui::Context,
    search: &mut Search,
    anchor: usize,
    max_cache: &mut usize,
    mut done: impl FnMut(&mut Search, Duration) -> bool,
) -> bool {
    let t = Instant::now();
    loop {
        search.receive();
        search.update(ctx, (anchor, 0.0), anchor);
        *max_cache = (*max_cache).max(search.stats().cache_bytes);
        if done(search, t.elapsed()) {
            return true;
        }
        if t.elapsed() > TIMEOUT {
            return false;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

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

fn dir_mb(dir: &std::path::Path) -> f64 {
    let files = std::fs::read_dir(dir).into_iter().flatten().flatten();
    files.filter_map(|e| e.metadata().ok()).map(|m| m.len()).sum::<u64>() as f64 / 1_048_576.0
}

fn mb(bytes: usize) -> f64 {
    bytes as f64 / 1_048_576.0
}
