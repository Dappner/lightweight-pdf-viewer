//! Headless session benchmark: drives the real app (tabs, render worker, CoreGraphics,
//! texture cache, snapshots) through egui without a window and reports latency and memory.
//!
//!   cargo bench --bench session -- a.pdf b.pdf [...]
//!
//! Environment: BENCH_PPP (pixels per point, default 1 = non-Retina), BENCH_SIZE
//! (window size in points, default 1000x1000), BENCH_BREAKDOWN=1 prints `footprint`'s
//! per-category breakdown after each phase.
//!
//! Memory is the process footprint without a window: GPU textures are not allocated, so
//! the "textures" column reports what they would occupy on the GPU. A windowed app adds a
//! roughly fixed ~35 MB (Metal, swapchain) on top.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Event, Key, Modifiers, RawInput, Rect, Vec2, ViewportId, ViewportInfo, pos2,
};
use lightweight_pdf_viewer::app::{App, Stats};
use lightweight_pdf_viewer::config::Config;
use lightweight_pdf_viewer::history::History;

const TIMEOUT: Duration = Duration::from_secs(20);

struct Harness {
    ctx: egui::Context,
    app: App,
    start: Instant,
    size: Vec2,
    ppp: f32,
    events: Vec<Event>,
}

impl Harness {
    fn frame(&mut self) {
        let mut raw = RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), self.size)),
            time: Some(self.start.elapsed().as_secs_f64()),
            events: std::mem::take(&mut self.events),
            ..Default::default()
        };
        raw.viewports.insert(
            ViewportId::ROOT,
            ViewportInfo { native_pixels_per_point: Some(self.ppp), ..Default::default() },
        );
        let app = &mut self.app;
        // The output (including texture uploads) is dropped: there is no GPU.
        let _ = self.ctx.run(raw, |ctx| app.ui(ctx));
    }

    fn stats(&self) -> Stats {
        self.app.stats()
    }

    fn press(&mut self, key: Key, modifiers: Modifiers) {
        self.events.push(Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers });
    }

    /// Runs frames until `done` holds; returns the elapsed time.
    fn wait(&mut self, done: impl Fn(&Stats) -> bool) -> Option<Duration> {
        let t = Instant::now();
        loop {
            self.frame();
            if done(&self.stats()) {
                return Some(t.elapsed());
            }
            if t.elapsed() > TIMEOUT {
                return None;
            }
            std::thread::sleep(Duration::from_micros(500));
        }
    }

    fn idle(&mut self, duration: Duration) {
        let t = Instant::now();
        while t.elapsed() < duration {
            self.frame();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Switches tab and returns (snapshot or tiles covered the first frame, time to sharp).
    fn switch(&mut self, key: Key, modifiers: Modifiers) -> (bool, Option<Duration>) {
        let before = self.stats().active;
        self.press(key, modifiers);
        let t = Instant::now();
        self.frame();
        let s = self.stats();
        assert_ne!(s.active, before, "tab switch did not happen");
        let covered = s.snapshot_shown || s.ready;
        let sharp = if s.ready { Some(t.elapsed()) } else { self.wait(|s| s.ready).map(|_| t.elapsed()) };
        (covered, sharp)
    }
}

fn footprint_mb() -> f64 {
    let mut info: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        libc::proc_pid_rusage(
            libc::getpid(),
            libc::RUSAGE_INFO_V2,
            &mut info as *mut _ as *mut libc::rusage_info_t,
        )
    };
    assert_eq!(ok, 0, "proc_pid_rusage failed");
    info.ri_phys_footprint as f64 / 1_048_576.0
}

/// The largest memory categories of this process, from macOS's `footprint` tool.
fn breakdown(phase: &str) {
    if std::env::var_os("BENCH_BREAKDOWN").is_none() {
        return;
    }
    let out = std::process::Command::new("footprint").arg(std::process::id().to_string()).output();
    let Ok(out) = out else { return };
    println!("  [{phase}]");
    for line in String::from_utf8_lossy(&out.stdout).lines().skip(5).take(6) {
        println!("    {line}");
    }
}

fn mb(bytes: usize) -> f64 {
    bytes as f64 / 1_048_576.0
}

fn ms(d: Option<Duration>) -> String {
    d.map_or("timeout".into(), |d| format!("{:.1}", d.as_secs_f64() * 1000.0))
}

/// "p50 / p95 / max" in milliseconds.
fn summary(mut samples: Vec<Duration>) -> String {
    if samples.is_empty() {
        return "-".into();
    }
    samples.sort();
    let at = |q: f64| samples[((samples.len() - 1) as f64 * q).round() as usize].as_secs_f64() * 1000.0;
    format!("{:.1} / {:.1} / {:.1}", at(0.5), at(0.95), at(1.0))
}

fn main() {
    let files: Vec<PathBuf> =
        std::env::args().skip(1).filter(|a| !a.starts_with("--")).map(PathBuf::from).collect();
    if files.is_empty() {
        eprintln!("usage: cargo bench --bench session -- <pdf> [<pdf> ...]");
        std::process::exit(2);
    }
    let ppp = std::env::var("BENCH_PPP").ok().and_then(|v| v.parse().ok()).unwrap_or(1.0);
    let size = std::env::var("BENCH_SIZE")
        .ok()
        .and_then(|v| v.split_once('x').map(|(w, h)| (w.parse().ok(), h.parse().ok())))
        .and_then(|(w, h)| Some(Vec2::new(w?, h?)))
        .unwrap_or(Vec2::new(1000.0, 1000.0));

    let ctx = egui::Context::default();
    let app = App::new(&ctx, Config::default(), History::default(), Vec::new());
    let mut h = Harness { ctx, app, start: Instant::now(), size, ppp, events: Vec::new() };
    h.frame();
    println!("viewport {}x{} pt @ {ppp}x\n", size.x, size.y);
    println!("startup footprint: {:.1} MB (no window)\n", footprint_mb());

    println!("{:<34} {:>6} {:>14} {:>10} {:>10} {:>11}", "open", "pages", "to sharp ms", "footprint", "textures", "snapshots");
    for (i, file) in files.iter().enumerate() {
        let t = Instant::now();
        h.app.open(&h.ctx, file.clone());
        let sharp = h.wait(|s| s.active == i && s.ready).map(|_| t.elapsed());
        let s = h.stats();
        let name = file.file_name().unwrap_or_default().to_string_lossy();
        println!(
            "{:<34} {:>6} {:>14} {:>7.1} MB {:>7.1} MB {:>8} KB",
            name.chars().take(34).collect::<String>(),
            s.page_count,
            ms(sharp),
            footprint_mb(),
            mb(s.texture_bytes),
            s.snapshot_bytes / 1024
        );
    }
    h.idle(Duration::from_millis(300));
    breakdown("after opening");

    if files.len() > 1 {
        println!("\ntab switching (ms: p50 / p95 / max; \"covered\" = something drawn on the first frame)");
        let mut rapid = Vec::new();
        let mut covered = 0;
        let n = 40;
        for _ in 0..n {
            let (c, sharp) = h.switch(Key::Tab, Modifiers::CTRL);
            covered += c as usize;
            rapid.extend(sharp);
        }
        println!("  cycle all tabs ({n}x)        to sharp {:<22} covered {covered}/{n}", summary(rapid));

        let mut toggle = Vec::new();
        let mut covered = 0;
        for i in 0..n {
            let (key, modifiers) = if i % 2 == 0 { (Key::Tab, Modifiers::CTRL) } else { (Key::Tab, Modifiers::CTRL | Modifiers::SHIFT) };
            h.idle(Duration::from_millis(300));
            let (c, sharp) = h.switch(key, modifiers);
            covered += c as usize;
            toggle.extend(sharp);
        }
        println!("  flip between two ({n}x)      to sharp {:<22} covered {covered}/{n}", summary(toggle));

        let mut dwell = Vec::new();
        let mut covered = 0;
        let n = 6;
        for _ in 0..n {
            h.idle(Duration::from_secs(4));
            let (c, sharp) = h.switch(Key::Tab, Modifiers::CTRL);
            covered += c as usize;
            dwell.extend(sharp);
        }
        println!("  after 4 s idle ({n}x)          to sharp {:<22} covered {covered}/{n}", summary(dwell));
        breakdown("after switching");

        let before = footprint_mb();
        for _ in 0..100 {
            h.switch(Key::Tab, Modifiers::CTRL);
        }
        h.idle(Duration::from_millis(300));
        println!("  footprint over 100 more switches: {before:.1} -> {:.1} MB", footprint_mb());
        if std::env::var_os("DIAG_SWITCHHEAP").is_some() {
            let out = std::process::Command::new("heap").args(["-s", &std::process::id().to_string()]).output().unwrap();
            for l in String::from_utf8_lossy(&out.stdout).lines().filter(|l| l.contains("zones:") || l.contains("footprint") ).take(4) { eprintln!("SH {l}"); }
            for l in String::from_utf8_lossy(&out.stdout).lines().skip_while(|l| !l.contains("COUNT")).take(25) { eprintln!("SH {l}"); }
            let out = std::process::Command::new("vmmap").args(["--summary", &std::process::id().to_string()]).output().unwrap();
            for l in String::from_utf8_lossy(&out.stdout).lines().filter(|l| l.contains("Malloc") || l.contains("DefaultMalloc") || l.contains("TOTAL")) { eprintln!("SV {l}"); }
        }
    }

    println!("\nscrolling the active document page by page");
    h.press(Key::Home, Modifiers::NONE);
    h.wait(|s| s.ready);
    let mut steps = Vec::new();
    let mut peak = 0;
    // Pages taller than the viewport take several steps; stop if the page stops advancing.
    let (mut last, mut stuck) = (usize::MAX, 0);
    for _ in 0..5000 {
        let s = h.stats();
        stuck = if s.current_page == last { stuck + 1 } else { 0 };
        if s.current_page + 1 >= s.page_count || stuck > 8 {
            break;
        }
        last = s.current_page;
        h.press(Key::PageDown, Modifiers::NONE);
        let t = Instant::now();
        h.wait(|s| s.ready);
        steps.push(t.elapsed());
        peak = peak.max(h.stats().texture_bytes);
        if steps.len() % 100 == 0 { eprintln!("DIAG step {} page {} footprint {:.1}", steps.len(), h.stats().current_page, footprint_mb()); if steps.len() % 300 == 0 { let out = std::process::Command::new("footprint").arg(std::process::id().to_string()).output().unwrap(); for l in String::from_utf8_lossy(&out.stdout).lines().take(14) { eprintln!("FP {l}"); } } }
    }
    {
        unsafe extern "C" { fn malloc_zone_pressure_relief(zone: *mut std::ffi::c_void, goal: usize) -> usize; }
        let before = footprint_mb();
        let freed = unsafe { malloc_zone_pressure_relief(std::ptr::null_mut(), 0) };
        eprintln!("DIAG relief: {before:.1} -> {:.1} MB (released {:.1} MB)", footprint_mb(), freed as f64 / 1048576.0);
    }
    {
        let out = std::process::Command::new("vmmap").args(["--summary", &std::process::id().to_string()]).output().unwrap();
        let t = String::from_utf8_lossy(&out.stdout);
        let mut on = false;
        for l in t.lines() { if l.contains("MALLOC ZONE") { on = true; } if on { eprintln!("VM {l}"); } }
    }
    if std::env::var_os("DIAG_HEAP").is_some() {
        let out = std::process::Command::new("malloc_history").args([&std::process::id().to_string(), "-allBySize"]).output().unwrap();
        for l in String::from_utf8_lossy(&out.stdout).lines() { eprintln!("HEAP {l}"); }
    }
    println!(
        "  {} steps, to sharp {}   peak textures {:.1} MB   footprint {:.1} MB",
        steps.len(),
        summary(steps),
        mb(peak),
        footprint_mb()
    );
    breakdown("after scrolling");

    println!("\nzoom (includes the 150 ms settle delay before re-rendering)");
    let mut zooms = Vec::new();
    for _ in 0..3 {
        h.press(Key::Equals, Modifiers::COMMAND);
        let t = Instant::now();
        h.frame();
        h.wait(|s| s.ready);
        zooms.push(t.elapsed());
    }
    println!(
        "  3x zoom in, to sharp {}   textures {:.1} MB   footprint {:.1} MB",
        summary(zooms),
        mb(h.stats().texture_bytes),
        footprint_mb()
    );
}
