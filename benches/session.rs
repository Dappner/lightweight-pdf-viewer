//! Headless session benchmark: drives the real app (tabs, render worker, CoreGraphics,
//! texture cache, snapshots) through egui without a window and reports latency and memory.
//!
//!   cargo bench --bench session -- a.pdf b.pdf [...]
//!
//! Environment: BENCH_MEMORY (memory preset: low, balanced or fast; default balanced),
//! BENCH_PPP (pixels per point, default 1 = non-Retina), BENCH_SIZE (window size in points,
//! default 1000x1000), BENCH_BREAKDOWN=1 prints `footprint`'s per-category breakdown after
//! each phase, BENCH_FULL=1 runs the long version (see [`Rounds`]).
//!
//! With two or more files, a final phase moves one tab into a second window. It scrolls the
//! first window while the second draws nothing, as an unfocused window does (the texture cache
//! should stay within its budget), then scrolls the second while the first keeps drawing and
//! so takes in the second window's tiles (which must survive until the second draws them).
//!
//! Every wait for sharp tiles that times out is counted; the run fails if any did.
//!
//! Memory is the process footprint without a window: GPU textures are not allocated, so
//! the "textures" column reports what they would occupy on the GPU. A windowed app adds a
//! roughly fixed ~35 MB (Metal, swapchain) on top. Each phase ends with its CPU time (all
//! threads) and the peak footprint seen during it, sampled every frame.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{
    self, DeferredViewportUiCallback, Event, Key, Modifiers, RawInput, Rect, Vec2, ViewportId, ViewportInfo, pos2,
};
use lightweight_pdf_viewer::app::{App, Command, Stats};
use lightweight_pdf_viewer::config::Config;
use lightweight_pdf_viewer::history::History;
use lightweight_pdf_viewer::memory::{self, Memory};

const TIMEOUT: Duration = Duration::from_secs(20);

/// How many times each scenario runs. The quick default takes about 30 s per preset and
/// still catches memory that grows with pages read; the full run (BENCH_FULL=1, ~2.5 min)
/// gives steadier latency numbers and scrolls the whole last document.
struct Rounds {
    cycle: usize,
    flip: usize,
    /// Switches after a 4 s pause, once the previous tab's tiles have expired.
    dwell: usize,
    /// Switches afterwards, to show the footprint doesn't creep.
    more: usize,
    /// Page-down steps; the full run scrolls to the end.
    scroll: usize,
}

impl Rounds {
    fn from_env() -> Self {
        if std::env::var_os("BENCH_FULL").is_some() {
            Self { cycle: 40, flip: 40, dwell: 6, more: 100, scroll: usize::MAX }
        } else {
            Self { cycle: 20, flip: 10, dwell: 2, more: 40, scroll: 150 }
        }
    }
}

struct Harness {
    ctx: egui::Context,
    app: App,
    start: Instant,
    size: Vec2,
    ppp: f32,
    focused: bool,
    events: Vec<Event>,
    /// Highest footprint seen since the phase began.
    peak: f64,
    phase_cpu: f64,
    /// The other windows' frame callbacks, which run only when [`Self::window_frame`] asks,
    /// as a native window only draws when something changes.
    windows: HashMap<ViewportId, Arc<DeferredViewportUiCallback>>,
    /// Whether [`Self::wait`] draws the other windows too.
    draw_others: bool,
    others_focused: bool,
    other_events: Vec<Event>,
    /// Waits that gave up before their condition held.
    timeouts: usize,
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
            ViewportInfo { native_pixels_per_point: Some(self.ppp), focused: Some(self.focused), ..Default::default() },
        );
        let app = &mut self.app;
        // The output (including texture uploads) is dropped: there is no GPU.
        let output = self.ctx.run(raw, |ctx| app.ui(ctx));
        self.windows = output
            .viewport_output
            .into_iter()
            .filter_map(|(id, v)| Some((id, v.viewport_ui_cb?)))
            .collect();
        self.peak = self.peak.max(footprint_mb());
    }

    /// One frame of every window other than the main one.
    fn window_frame(&mut self) {
        for (id, ui) in self.windows.clone() {
            let mut raw = RawInput {
                viewport_id: id,
                screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), self.size)),
                time: Some(self.start.elapsed().as_secs_f64()),
                events: std::mem::take(&mut self.other_events),
                ..Default::default()
            };
            raw.viewports.insert(
                id,
                ViewportInfo {
                    native_pixels_per_point: Some(self.ppp),
                    focused: Some(self.others_focused),
                    ..Default::default()
                },
            );
            let _ = self.ctx.run(raw, |ctx| ui(ctx));
        }
    }

    fn begin_phase(&mut self) {
        self.peak = footprint_mb();
        self.phase_cpu = cpu_seconds();
    }

    fn end_phase(&self) {
        println!("  cpu {:.2} s   peak footprint {:.1} MB", cpu_seconds() - self.phase_cpu, self.peak);
    }

    fn stats(&self) -> Stats {
        self.app.stats()
    }

    fn press(&mut self, key: Key, modifiers: Modifiers) {
        self.events.push(Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers });
    }

    /// Presses a key in the other windows.
    fn press_other(&mut self, key: Key) {
        self.other_events.push(Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE });
    }

    /// Runs frames until `done` holds; returns the elapsed time, or `None` (counted in
    /// `timeouts`) if it gave up.
    fn wait(&mut self, done: impl Fn(&Stats) -> bool) -> Option<Duration> {
        let t = Instant::now();
        loop {
            self.frame();
            if self.draw_others {
                self.window_frame();
            }
            if done(&self.stats()) {
                return Some(t.elapsed());
            }
            if t.elapsed() > TIMEOUT {
                self.timeouts += 1;
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

/// User plus system CPU time of the whole process, in seconds.
fn cpu_seconds() -> f64 {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) }, 0, "getrusage failed");
    let secs = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    secs(usage.ru_utime) + secs(usage.ru_stime)
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
    let preset: Memory = std::env::var("BENCH_MEMORY")
        .map(|v| serde_json::from_value(serde_json::Value::String(v)).expect("BENCH_MEMORY: low, balanced or fast"))
        .unwrap_or_default();
    // Like the app, so the allocator behaves as it would there.
    memory::relaunch(preset);
    let files: Vec<PathBuf> =
        std::env::args().skip(1).filter(|a| !a.starts_with("--")).map(PathBuf::from).collect();
    if files.is_empty() {
        eprintln!("usage: cargo bench --bench session -- <pdf> [<pdf> ...]");
        std::process::exit(2);
    }
    let rounds = Rounds::from_env();
    let ppp = std::env::var("BENCH_PPP").ok().and_then(|v| v.parse().ok()).unwrap_or(1.0);
    let size = std::env::var("BENCH_SIZE")
        .ok()
        .and_then(|v| v.split_once('x').map(|(w, h)| (w.parse().ok(), h.parse().ok())))
        .and_then(|(w, h)| Some(Vec2::new(w?, h?)))
        .unwrap_or(Vec2::new(1000.0, 1000.0));

    let ctx = egui::Context::default();
    // Other windows draw only when the harness runs them, as with native windows.
    ctx.set_embed_viewports(false);
    let mut config = Config::default();
    config.memory = preset;
    let app = App::new(&ctx, config, History::default(), Vec::new());
    let mut h = Harness { ctx, app, start: Instant::now(), size, ppp, focused: true, events: Vec::new(), peak: 0.0, phase_cpu: 0.0,
        windows: HashMap::new(),
        draw_others: false,
        others_focused: false,
        other_events: Vec::new(),
        timeouts: 0,
    };
    h.frame();
    let malloc = std::env::var("MallocSpaceEfficient").map_or("default".into(), |v| format!("MallocSpaceEfficient={v}"));
    println!("memory preset {preset:?} ({malloc}), viewport {}x{} pt @ {ppp}x\n", size.x, size.y);
    println!("startup footprint: {:.1} MB (no window)\n", footprint_mb());

    println!("{:<34} {:>6} {:>14} {:>10} {:>10} {:>11}", "open", "pages", "to sharp ms", "footprint", "textures", "snapshots");
    println!("{:>101}", "(allocated)");
    h.begin_phase();
    for (i, file) in files.iter().enumerate() {
        let t = Instant::now();
        h.app.open(&h.ctx, file.clone());
        let sharp = h.wait(|s| s.active == i && s.ready).map(|_| t.elapsed());
        let s = h.stats();
        let name = file.file_name().unwrap_or_default().to_string_lossy();
        println!(
            "{:<34} {:>6} {:>14} {:>7.1} MB {:>7.1} MB {:>8} KB {:>8} KB",
            name.chars().take(34).collect::<String>(),
            s.page_count,
            ms(sharp),
            footprint_mb(),
            mb(s.texture_bytes),
            s.snapshot_bytes / 1024,
            s.snapshot_capacity / 1024
        );
    }
    h.idle(Duration::from_millis(300));
    h.end_phase();
    breakdown("after opening");

    if files.len() > 1 {
        println!("\ntab switching (ms: p50 / p95 / max; \"covered\" = something drawn on the first frame)");
        h.begin_phase();
        let mut rapid = Vec::new();
        let mut covered = 0;
        let n = rounds.cycle;
        for _ in 0..n {
            let (c, sharp) = h.switch(Key::Tab, Modifiers::CTRL);
            covered += c as usize;
            rapid.extend(sharp);
        }
        println!("  cycle all tabs ({n}x)        to sharp {:<22} covered {covered}/{n}", summary(rapid));

        let mut toggle = Vec::new();
        let mut covered = 0;
        let n = rounds.flip;
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
        let n = rounds.dwell;
        for _ in 0..n {
            h.idle(Duration::from_secs(4));
            let (c, sharp) = h.switch(Key::Tab, Modifiers::CTRL);
            covered += c as usize;
            dwell.extend(sharp);
        }
        println!("  after 4 s idle ({n}x)          to sharp {:<22} covered {covered}/{n}", summary(dwell));
        breakdown("after switching");

        let before = footprint_mb();
        for _ in 0..rounds.more {
            h.switch(Key::Tab, Modifiers::CTRL);
        }
        h.idle(Duration::from_millis(300));
        println!("  footprint over {} more switches: {before:.1} -> {:.1} MB", rounds.more, footprint_mb());
        let s = h.stats();
        println!(
            "  snapshots held: {} KB compressed, {} KB allocated",
            s.snapshot_bytes / 1024,
            s.snapshot_capacity / 1024
        );
        h.end_phase();
    }

    println!("\nscrolling the active document page by page");
    h.press(Key::Home, Modifiers::NONE);
    h.wait(|s| s.ready);
    h.begin_phase();
    let mut steps = Vec::new();
    let mut peak = 0;
    // Pages taller than the viewport take several steps; stop if the page stops advancing.
    let (mut last, mut stuck) = (usize::MAX, 0);
    for _ in 0..rounds.scroll.min(5000) {
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
    }
    println!(
        "  {} steps, to sharp {}   peak textures {:.1} MB   footprint {:.1} MB",
        steps.len(),
        summary(steps),
        mb(peak),
        footprint_mb()
    );
    h.end_phase();
    breakdown("after scrolling");

    println!("\nzoom (includes the 150 ms settle delay before re-rendering)");
    h.begin_phase();
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
    h.end_phase();

    println!("\nbackground (unfocused and left alone; the snapshot texture is not counted)");
    match preset.tuning().sleep_after {
        Some(after) => background(&mut h, after),
        None => println!("  never drops tiles with this preset"),
    }

    if files.len() > 1 {
        two_windows(&mut h, &rounds, preset);
    }

    if h.timeouts > 0 {
        eprintln!("\n{} waits for sharp tiles timed out; the results above are not valid", h.timeouts);
        std::process::exit(1);
    }
}

/// Leaves the app unfocused until it drops its tiles, then focuses it again.
fn background(h: &mut Harness, after: f64) {
    h.begin_phase();
    let textures = h.stats().texture_bytes;
    h.focused = false;
    let t = Instant::now();
    // An idle app draws no frames; check a few times a second instead of spinning.
    while !h.stats().asleep && t.elapsed() < Duration::from_secs_f64(after + 5.0) {
        h.frame();
        std::thread::sleep(Duration::from_millis(50));
    }
    h.idle(Duration::from_millis(500));
    let s = h.stats();
    println!(
        "  asleep {} after {:.1} s   textures {:.1} -> {:.1} MB   footprint {:.1} MB",
        s.asleep,
        t.elapsed().as_secs_f64(),
        mb(textures),
        mb(s.texture_bytes),
        footprint_mb()
    );
    h.end_phase();

    h.focused = true;
    let t = Instant::now();
    h.frame();
    let covered = h.stats().snapshot_shown || h.stats().ready;
    let sharp = h.wait(|s| s.ready).map(|_| t.elapsed());
    println!("  refocus: covered {covered}, to sharp {} ms", ms(sharp));
}

/// Moves the active tab into a second window and waits until both are sharp. Then scrolls the
/// first window while the second draws nothing, and the second while the first keeps drawing.
fn two_windows(h: &mut Harness, rounds: &Rounds, preset: Memory) {
    println!("\ntwo windows");
    h.app.run(&h.ctx.clone(), 0, Command::MoveTabToNewWindow);
    h.draw_others = true;
    let sharp = h.wait(|s| s.windows_ready);
    let s = h.stats();
    assert_eq!(s.windows, 2, "tab was not moved to a second window");
    println!("  both windows sharp: {} ms", ms(sharp));
    let budget = preset.tuning().texture_budget * s.windows;

    println!("  the first window scrolls, the second draws nothing");
    h.draw_others = false;
    h.press(Key::Home, Modifiers::NONE);
    h.wait(|s| s.ready);
    scroll_both_windows(h, rounds, budget, |h| h.press(Key::PageDown, Modifiers::NONE));

    println!("  the second window scrolls, the first keeps drawing");
    h.draw_others = true;
    h.focused = false;
    h.others_focused = true;
    h.press_other(Key::Home);
    h.wait(|s| s.ready);
    scroll_both_windows(h, rounds, budget, |h| h.press_other(Key::PageDown));
    breakdown("two windows");
}

/// Pages down in the focused window with `page_down` and reports the texture peak and any
/// steps that never became sharp.
fn scroll_both_windows(h: &mut Harness, rounds: &Rounds, budget: usize, page_down: impl Fn(&mut Harness)) {
    h.begin_phase();
    let timeouts = h.timeouts;
    let start = h.stats().texture_bytes;
    let mut peak = start;
    let mut steps = Vec::new();
    let (mut last, mut stuck) = (usize::MAX, 0);
    for _ in 0..rounds.scroll.min(5000) {
        let s = h.stats();
        stuck = if s.current_page == last { stuck + 1 } else { 0 };
        // Each timeout takes 20 s; a few are enough to show the window isn't getting its tiles.
        if s.current_page + 1 >= s.page_count || stuck > 8 || h.timeouts - timeouts >= 3 {
            break;
        }
        last = s.current_page;
        page_down(h);
        let t = Instant::now();
        if h.wait(|s| s.ready).is_some() {
            steps.push(t.elapsed());
        }
        peak = peak.max(h.stats().texture_bytes);
    }
    println!(
        "    {} steps sharp, {} timed out, to sharp {}   textures {:.1} -> peak {:.1} MB (budget {:.1} MB)   footprint {:.1} MB",
        steps.len(),
        h.timeouts - timeouts,
        summary(steps),
        mb(start),
        mb(peak),
        mb(budget),
        footprint_mb()
    );
    h.end_phase();
}
