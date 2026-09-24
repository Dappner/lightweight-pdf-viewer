//! Headless checks of state saved between launches: settings and file history (no window; egui only).

use std::path::{Path, PathBuf};

use eframe::egui::{self, Event, Modifiers, PointerButton, Pos2, RawInput, Rect, ViewportId, ViewportInfo, pos2, vec2};
use lightweight_pdf_viewer::app::App;
use lightweight_pdf_viewer::config::Config;
use lightweight_pdf_viewer::history::History;

struct Harness {
    ctx: egui::Context,
    app: App,
    time: f64,
}

impl Harness {
    fn new(config: Config, files: Vec<PathBuf>) -> Self {
        Self::with_history(config, History::default(), files)
    }

    fn with_history(config: Config, history: History, files: Vec<PathBuf>) -> Self {
        let ctx = egui::Context::default();
        let app = App::new(&ctx, config, history, files);
        let mut h = Self { ctx, app, time: 0.0 };
        for _ in 0..500 {
            let _ = h.frame(vec![]);
            if h.app.stats().page_count > 0 {
                let _ = h.frame(vec![]);
                return h;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("PDF never loaded");
    }

    /// Runs one frame and returns the mouse cursor the app asked for.
    fn frame(&mut self, events: Vec<Event>) -> egui::CursorIcon {
        self.time += 1.0 / 60.0;
        let mut raw = RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(1000.0, 800.0))),
            time: Some(self.time),
            events,
            ..Default::default()
        };
        raw.viewports
            .insert(ViewportId::ROOT, ViewportInfo { native_pixels_per_point: Some(1.0), ..Default::default() });
        let app = &mut self.app;
        self.ctx.run(raw, |ctx| app.ui(ctx)).platform_output.cursor_icon
    }

    fn button(&mut self, pos: Pos2, pressed: bool) {
        let _ = self.frame(vec![
            Event::PointerMoved(pos),
            Event::PointerButton { pos, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE },
        ]);
    }

    fn drag(&mut self, from: Pos2, to: Pos2) {
        let _ = self.frame(vec![Event::PointerMoved(from)]);
        self.button(from, true);
        for i in 1..=10 {
            let _ = self.frame(vec![Event::PointerMoved(from + (to - from) * i as f32 / 10.0)]);
        }
        self.button(to, false);
        let _ = self.frame(vec![]);
    }
}

/// A blank PDF with `pages` pages.
fn write_pdf(path: &Path, pages: usize) {
    let kids: Vec<String> = (0..pages).map(|i| format!("{} 0 R", i + 3)).collect();
    let mut objects = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        format!("<< /Type /Pages /Kids [{}] /Count {pages} >>", kids.join(" ")),
    ];
    objects.extend((0..pages).map(|_| "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>".to_string()));
    let mut pdf = String::from("%PDF-1.4\n");
    let mut offsets = Vec::new();
    for (i, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf += &format!("{} 0 obj\n{object}\nendobj\n", i + 1);
    }
    let xref = pdf.len();
    pdf += &format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1);
    for offset in offsets {
        pdf += &format!("{offset:010} 00000 n \n");
    }
    pdf += &format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objects.len() + 1);
    std::fs::write(path, pdf).unwrap();
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("lightweight-pdf-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn resizing_the_sidebar_is_saved_and_clamped() {
    let dir = temp_dir("sidebar");
    let pdf = dir.join("a.pdf");
    let config = dir.join("config.json");
    write_pdf(&pdf, 1);
    let _ = std::fs::remove_file(&config);

    let mut h = Harness::new(Config::load_from(config.clone()), vec![pdf.clone()]);
    let edge = 160.0;
    h.drag(pos2(edge, 400.0), pos2(edge + 100.0, 400.0));
    assert_eq!(Config::load_from(config.clone()).sidebar_width, 260.0);

    // A fresh app opens the sidebar at the saved width, and can't shrink it past the minimum.
    let mut h = Harness::new(Config::load_from(config.clone()), vec![pdf]);
    h.drag(pos2(260.0, 400.0), pos2(20.0, 400.0));
    assert_eq!(Config::load_from(config).sidebar_width, 150.0);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn hovering_the_sidebar_edge_does_not_flicker() {
    let dir = temp_dir("hover");
    let pdf = dir.join("a.pdf");
    write_pdf(&pdf, 30);
    let mut h = Harness::new(Config::default(), vec![pdf]);
    for x in (140..=170).map(|x| x as f32) {
        let pos = pos2(x, 400.0);
        let cursors: Vec<_> = (0..6).map(|_| h.frame(vec![Event::PointerMoved(pos)])).collect();
        assert!(cursors[1..].windows(2).all(|w| w[0] == w[1]), "cursor flickers at x = {x}: {cursors:?}");
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn open_tabs_are_restored_at_launch() {
    let dir = temp_dir("tabs");
    let [a, b, c] = ["a.pdf", "b.pdf", "c.pdf"].map(|name| dir.join(name));
    for pdf in [&a, &b, &c] {
        write_pdf(pdf, 1);
    }
    let config = dir.join("config.json");
    let _ = std::fs::remove_file(&config);

    let mut h = Harness::new(Config::load_from(config.clone()), vec![a.clone(), b.clone(), c.clone()]);
    let _ = h.frame(vec![Event::Key {
        key: egui::Key::Num1,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Modifiers::COMMAND,
    }]);
    let saved = Config::load_from(config.clone());
    assert_eq!(saved.windows.len(), 1);
    assert_eq!(saved.windows[0].tabs.len(), 3);
    assert_eq!(saved.windows[0].active, 0);

    // Files that have since been deleted are skipped; the active tab is kept.
    std::fs::remove_file(&b).unwrap();
    let h = Harness::new(Config::load_from(config.clone()), vec![]);
    let stats = h.app.stats();
    assert_eq!((stats.tabs, stats.active), (2, 0));

    // With restoring turned off, only the files asked for open.
    let mut off = Config::load_from(config);
    off.restore_tabs = false;
    let h = Harness::new(off, vec![c]);
    assert_eq!(h.app.stats().tabs, 1);

    let _ = std::fs::remove_dir_all(dir);
}

fn key(key: egui::Key, modifiers: Modifiers) -> Event {
    Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers }
}

#[test]
fn quick_open_reopens_a_recent_file() {
    let dir = temp_dir("quick-open");
    let [a, b] = ["alpha.pdf", "beta.pdf"].map(|name| dir.join(name));
    for pdf in [&a, &b] {
        write_pdf(pdf, 1);
    }
    let history = dir.join("history.json");
    let _ = std::fs::remove_file(&history);

    let mut h = Harness::with_history(Config::default(), History::load_from(history.clone()), vec![a.clone(), b]);
    assert_eq!(h.app.stats().active, 1);
    let _ = h.frame(vec![key(egui::Key::K, Modifiers::COMMAND)]);
    let _ = h.frame(vec![Event::Text("alp".into())]);
    let _ = h.frame(vec![key(egui::Key::Enter, Modifiers::NONE)]);
    assert_eq!(h.app.stats().active, 0);

    // The active tab (alpha) is left out, so ⌘1 picks beta: the palette's first result, not
    // the first tab.
    let _ = h.frame(vec![key(egui::Key::K, Modifiers::COMMAND)]);
    let _ = h.frame(vec![key(egui::Key::Num1, Modifiers::COMMAND)]);
    assert_eq!(h.app.stats().active, 1);
    let _ = h.frame(vec![key(egui::Key::Num1, Modifiers::COMMAND)]);
    assert_eq!(h.app.stats().active, 0, "⌘1 switches tabs again once the palette has closed");

    let ranked = History::load_from(history).ranked(u64::MAX / 2);
    assert_eq!(ranked.len(), 2);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn memory_preset_is_read_from_the_config_file() {
    use lightweight_pdf_viewer::memory::Memory;

    let dir = temp_dir("memory");
    let config = dir.join("config.json");
    assert_eq!(Config::load_from(config.clone()).memory, Memory::Balanced, "defaults to balanced");
    std::fs::write(&config, r#"{ "memory": "low" }"#).unwrap();
    assert_eq!(Config::load_from(config).memory, Memory::Low);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn texture_budget_holds_with_a_single_tab() {
    use lightweight_pdf_viewer::memory::Memory;

    let dir = temp_dir("budget");
    let pdf = dir.join("long.pdf");
    write_pdf(&pdf, 150);
    let mut config = Config::default();
    config.memory = Memory::Low;
    // One tab, so nothing is parked: the budget must still be enforced every frame.
    let mut h = Harness::new(config, vec![pdf]);
    let mut peak = 0;
    for _ in 0..150 {
        let _ = h.frame(vec![key(egui::Key::PageDown, Modifiers::NONE)]);
        for _ in 0..200 {
            if h.app.stats().ready {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
            let _ = h.frame(vec![]);
        }
        peak = peak.max(h.app.stats().texture_bytes);
    }
    assert!(h.app.stats().current_page > 100, "scrolled through the document");
    assert!(peak <= 16 << 20, "textures reached {} MB over a 16 MB budget", peak >> 20);
    let _ = std::fs::remove_dir_all(dir);
}
