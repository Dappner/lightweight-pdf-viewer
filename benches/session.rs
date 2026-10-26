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
//! With two or more files, a search phase runs ⌘F for "the" in every tab, saving page text to
//! a fresh cache folder, as the app does. Once the 3-second grace period after leaving a tab
//! has passed, only the active tab may keep a search worker (and its text); the run fails if
//! more do, if the footprint stays more than one search's text budget ([`CACHE_BUDGET`]) plus
//! [`SEARCH_SLACK_MB`] above where it was before searching, or if closing every find bar
//! leaves a worker. It then searches every tab again, which reads the saved text. In the
//! two-window phase, each window searches its tab: two workers may stay, one per window.
//!
//! Every wait that times out is counted; the run fails if any did.
//!
//! Memory is the process footprint without a window: GPU textures are not allocated, so
//! the "textures" column reports what they would occupy on the GPU. A windowed app adds a
//! roughly fixed ~35 MB (Metal, swapchain) on top. Each phase ends with its CPU time (all
//! threads) and the peak footprint seen during it, sampled every frame.

mod common;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{
    self, DeferredViewportUiCallback, Event, Key, Modifiers, RawInput, Rect, Vec2, ViewportId, ViewportInfo, pos2,
};
use common::footprint_mb;
use lightweight_pdf_viewer::app::{App, Command, Stats};
use lightweight_pdf_viewer::config::Config;
use lightweight_pdf_viewer::history::History;
use lightweight_pdf_viewer::memory::{self, Memory};
use lightweight_pdf_viewer::search::CACHE_BUDGET;

const TIMEOUT: Duration = Duration::from_secs(20);
/// Scrolling steps slower than this to sharpen are reported one by one.
const SLOW_STEP: Duration = Duration::from_millis(250);
/// Seconds a tab left behind keeps its tiles and search (the app's `PARK_SECONDS`), plus a
/// margin.
const GRACE: Duration = Duration::from_millis(3500);
/// Footprint the search phase may leave on top of one search's text budget: rendering
/// differences between the moments measured, and the allocator's slack.
const SEARCH_SLACK_MB: f64 = 8.0;

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
    /// How long each main-window frame took on the UI thread since the phase began: what
    /// decides whether scrolling stays smooth (8.3 ms a frame at 120 Hz).
    frame_times: Vec<Duration>,
    /// Highest footprint seen since the phase began.
    peak: f64,
    phase_cpu: f64,
    /// Rendered and discarded images when the phase began (see [`Stats::rendered`]).
    phase_renders: (u64, u64),
    /// The other windows' frame callbacks, which run only when [`Self::window_frame`] asks,
    /// as a native window only draws when something changes.
    windows: HashMap<ViewportId, Arc<DeferredViewportUiCallback>>,
    /// Whether [`Self::wait`] draws the other windows too.
    draw_others: bool,
    others_focused: bool,
    other_events: Vec<Event>,
    /// Waits that gave up before their condition held.
    timeouts: usize,
    /// Checks that failed, reported at the end.
    failures: Vec<String>,
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
        let t = Instant::now();
        let output = self.ctx.run(raw, |ctx| app.ui(ctx));
        self.frame_times.push(t.elapsed());
        self.windows = output.viewport_output.into_iter().filter_map(|(id, v)| Some((id, v.viewport_ui_cb?))).collect();
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
        self.frame_times.clear();
        self.peak = footprint_mb();
        self.phase_cpu = cpu_seconds();
        let s = self.stats();
        self.phase_renders = (s.rendered, s.discarded);
    }

    fn end_phase(&self) {
        let s = self.stats();
        println!(
            "  cpu {:.2} s   peak footprint {:.1} MB   UI frame {} ms over {} frames   rendered {} ({} wasted)",
            cpu_seconds() - self.phase_cpu,
            self.peak,
            summary(self.frame_times.clone()),
            self.frame_times.len(),
            s.rendered - self.phase_renders.0,
            s.discarded - self.phase_renders.1,
        );
    }

    fn stats(&self) -> Stats {
        self.app.stats()
    }

    fn press(&mut self, key: Key, modifiers: Modifiers) {
        self.events.push(Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers });
    }

    /// Presses a key in the other windows.
    fn press_other(&mut self, key: Key) {
        self.other_events.push(Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        });
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
            if self.draw_others {
                self.window_frame();
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Opens (or focuses) the find bar of the focused window's active tab and searches it for
    /// `query`, in the main window or, with `other`, the others.
    fn find(&mut self, query: &str, other: bool) {
        let cmd_f =
            Event::Key { key: Key::F, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::COMMAND };
        let text = Event::Text(query.into());
        if other {
            self.other_events.push(cmd_f);
            self.window_frame();
            self.other_events.push(text);
            self.window_frame();
        } else {
            self.events.push(cmd_f);
            self.frame();
            self.events.push(text);
            self.frame();
        }
    }

    /// Records a failed check.
    fn check(&mut self, ok: bool, failure: impl FnOnce() -> String) {
        if !ok {
            let failure = failure();
            println!("    FAIL: {failure}");
            self.failures.push(failure);
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
    let files: Vec<PathBuf> = std::env::args().skip(1).filter(|a| !a.starts_with("--")).map(PathBuf::from).collect();
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
    // Searches save page text as in the app, to a folder of this run's own.
    let search_cache = std::env::temp_dir().join(format!("lightweight-pdf-session-search-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&search_cache);
    config.search_cache_dir = Some(search_cache.clone());
    let app = App::new(&ctx, config, History::default(), Vec::new());
    let mut h = Harness {
        ctx,
        app,
        start: Instant::now(),
        size,
        ppp,
        focused: true,
        events: Vec::new(),
        frame_times: Vec::new(),
        peak: 0.0,
        phase_cpu: 0.0,
        phase_renders: (0, 0),
        windows: HashMap::new(),
        draw_others: false,
        others_focused: false,
        other_events: Vec::new(),
        timeouts: 0,
        failures: Vec::new(),
    };
    h.frame();
    let malloc =
        std::env::var("MallocSpaceEfficient").map_or("default".into(), |v| format!("MallocSpaceEfficient={v}"));
    println!("memory preset {preset:?} ({malloc}), viewport {}x{} pt @ {ppp}x\n", size.x, size.y);
    println!("startup footprint: {:.1} MB (no window)\n", footprint_mb());

    println!(
        "{:<34} {:>6} {:>14} {:>10} {:>10} {:>11}",
        "open", "pages", "to sharp ms", "footprint", "textures", "snapshots"
    );
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
            let (key, modifiers) =
                if i % 2 == 0 { (Key::Tab, Modifiers::CTRL) } else { (Key::Tab, Modifiers::CTRL | Modifiers::SHIFT) };
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
    scroll_through(&mut h, &rounds);
    breakdown("after scrolling");

    // Every visible page's text is read to highlight it: the cost of a selection on screen.
    println!("\nscrolling page by page with everything selected (⌘A)");
    h.press(Key::A, Modifiers::COMMAND);
    scroll_through(&mut h, &rounds);
    h.press(Key::Escape, Modifiers::NONE);
    h.frame();

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

    if files.len() > 1 {
        search(&mut h, &files);
    }

    println!("\nbackground (unfocused windows retain their pages)");
    background(&mut h);

    if files.len() > 1 {
        two_windows(&mut h, &rounds, preset);
    }

    let _ = std::fs::remove_dir_all(&search_cache);
    if h.timeouts > 0 {
        eprintln!("\n{} waits timed out; the results above are not valid", h.timeouts);
    }
    if !h.failures.is_empty() {
        eprintln!("\n{} check(s) failed", h.failures.len());
    }
    if h.timeouts > 0 || !h.failures.is_empty() {
        std::process::exit(1);
    }
}

/// Scrolls the active document from the top, a page at a time, waiting for each step to be
/// sharp, as one phase.
fn scroll_through(h: &mut Harness, rounds: &Rounds) {
    h.press(Key::Home, Modifiers::NONE);
    h.wait(|s| s.ready);
    h.begin_phase();
    let phase = Instant::now();
    let mut steps = Vec::new();
    let (mut peak, mut peak_text) = (0, 0);
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
        let took = t.elapsed();
        if took > SLOW_STEP {
            println!(
                "    slow step {}: page {last} -> {}, {} ms, {:.1} s into the phase, {} rendered so far",
                steps.len(),
                h.stats().current_page,
                ms(Some(took)),
                phase.elapsed().as_secs_f64(),
                h.stats().rendered - h.phase_renders.0,
            );
        }
        steps.push(took);
        let s = h.stats();
        peak = peak.max(s.texture_bytes);
        peak_text = peak_text.max(s.text_bytes);
    }
    println!(
        "  {} steps, to sharp {}   peak textures {:.1} MB   peak text layers {:.2} MB   footprint {:.1} MB",
        steps.len(),
        summary(steps),
        mb(peak),
        mb(peak_text),
        footprint_mb()
    );
    h.end_phase();
}

/// Searches every tab for "the", then checks that only the active tab's search keeps a worker
/// once the others' grace period is over, and what the footprint settles at. Closes every
/// find bar, then searches every tab again from the saved text.
fn search(h: &mut Harness, files: &[PathBuf]) {
    println!("\nsearch (⌘F \"the\" in each tab, text saved to a fresh cache folder)");
    let n = files.len();
    let select = |h: &mut Harness, i: usize| {
        h.app.run(&h.ctx.clone(), 0, Command::SelectTab(i));
        h.wait(|s| s.active == i && s.ready);
    };
    // The same tabs visited in the same order, then the grace period, as below: what the
    // footprint is compared with.
    for i in 0..n {
        select(h, i);
    }
    h.idle(GRACE);
    let before = footprint_mb();
    breakdown("before searching");
    h.begin_phase();

    let mut cold = Vec::new();
    for (i, file) in files.iter().enumerate() {
        select(h, i);
        let t = Instant::now();
        h.find("the", false);
        let done = h.wait(|s| s.find_done).map(|_| t.elapsed());
        cold.extend(done);
        let s = h.stats();
        println!(
            "  {:<34} counted in {:>8} ms ({} pages extracted, {} from disk)   workers {}   footprint {:.1} MB",
            file.file_name().unwrap_or_default().to_string_lossy().chars().take(34).collect::<String>(),
            ms(done),
            s.find_reads.1,
            s.find_reads.0,
            s.search_workers,
            footprint_mb()
        );
    }
    let workers = h.stats().search_workers;
    println!(
        "  right after the last: {workers} workers (the active tab and the one just left)   footprint {:.1} MB",
        footprint_mb()
    );
    h.check(workers <= 2, || format!("{workers} search workers right after searching, at most 2 expected"));

    h.idle(GRACE);
    let workers = h.stats().search_workers;
    let settled = footprint_mb();
    let allowed = mb(CACHE_BUDGET) + SEARCH_SLACK_MB;
    println!(
        "  after the grace period: {workers} worker   footprint {before:.1} -> {settled:.1} MB (+{:.1}, allowed +{allowed:.0})",
        settled - before
    );
    h.check(workers == 1, || {
        format!("{workers} search workers after the grace period, only the active tab's expected")
    });
    h.check(settled - before <= allowed, || format!("footprint +{:.1} MB after searching every tab", settled - before));

    // ⌘F focuses the bar, Esc closes it.
    for i in (0..n).rev() {
        select(h, i);
        h.press(Key::F, Modifiers::COMMAND);
        h.frame();
        h.press(Key::Escape, Modifiers::NONE);
        h.frame();
    }
    h.idle(Duration::from_millis(300));
    let workers = h.stats().search_workers;
    println!("  every find bar closed: {workers} workers   footprint {:.1} MB", footprint_mb());
    breakdown("every find bar closed");
    h.check(workers == 0, || format!("{workers} search workers with every find bar closed"));

    let mut warm = Vec::new();
    let mut extracted = 0;
    for i in 0..n {
        select(h, i);
        let t = Instant::now();
        h.find("the", false);
        warm.extend(h.wait(|s| s.find_done).map(|_| t.elapsed()));
        extracted += h.stats().find_reads.1;
    }
    println!(
        "  counted, ms (p50 / p95 / max): first search {}   again, from saved text {} ({extracted} pages extracted)",
        summary(cold),
        summary(warm)
    );
    h.check(extracted == 0, || format!("searching again extracted {extracted} pages instead of reading saved text"));
    h.idle(GRACE);
    let workers = h.stats().search_workers;
    println!("  after the grace period: {workers} worker   footprint {:.1} MB", footprint_mb());
    h.check(workers == 1, || format!("{workers} search workers after searching again, 1 expected"));

    // Searching and closing again and again: the footprint may level off where the allocator
    // keeps freed text, but must not climb with every round.
    let mut closed = Vec::new();
    let queries = ["and", "of", "is", "to"];
    for query in queries {
        for i in 0..n {
            select(h, i);
            h.find(query, false);
            h.wait(|s| s.find_done);
        }
        for i in (0..n).rev() {
            select(h, i);
            h.press(Key::F, Modifiers::COMMAND);
            h.frame();
            h.press(Key::Escape, Modifiers::NONE);
            h.frame();
        }
        h.idle(GRACE);
        closed.push(footprint_mb());
    }
    let rounds: Vec<String> = closed.iter().map(|f| format!("{f:.1}")).collect();
    println!(
        "  {} more rounds of searching every tab and closing: footprint {} MB",
        queries.len(),
        rounds.join(" -> ")
    );
    let climb = closed[closed.len() - 1] - closed[1];
    h.check(climb <= SEARCH_SLACK_MB / 2.0, || format!("footprint climbed {climb:.1} MB over the later search rounds"));
    breakdown("after the search rounds");
    h.find("the", false);
    h.wait(|s| s.find_done);
    // The last tab's search stays open for the background phase.
    h.end_phase();
}

/// Checks that an unfocused window retains its rendered pages.
fn background(h: &mut Harness) {
    h.begin_phase();
    let textures = h.stats().texture_bytes;
    h.focused = false;
    h.idle(Duration::from_secs(12));
    let s = h.stats();
    println!("  textures {:.1} -> {:.1} MB   footprint {:.1} MB", mb(textures), mb(s.texture_bytes), footprint_mb());
    h.check(s.ready && s.texture_bytes >= textures, || "unfocused window lost its pages".into());
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

    // Each window searches its active tab for a new query (their searches for "the" are
    // done): one worker each stays.
    println!("  each window searches its tab for \"and\"");
    h.begin_phase();
    let before = footprint_mb();
    h.find("and", true);
    let second = h.wait(|s| s.find_done);
    h.focused = true;
    h.others_focused = false;
    h.find("and", false);
    let first = h.wait(|s| s.find_done);
    h.idle(GRACE);
    let workers = h.stats().search_workers;
    let settled = footprint_mb();
    let allowed = 2.0 * mb(CACHE_BUDGET) + SEARCH_SLACK_MB;
    println!(
        "    counted in {} / {} ms   {workers} workers   footprint {before:.1} -> {settled:.1} MB (+{:.1}, allowed +{allowed:.0})",
        ms(first),
        ms(second),
        settled - before
    );
    h.check(workers == 2, || format!("{workers} search workers with two windows, one each expected"));
    h.check(settled - before <= allowed, || {
        format!("footprint +{:.1} MB after searching in two windows", settled - before)
    });
    h.end_phase();
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
