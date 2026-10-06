//! Headless checks of keyboard navigation through a real (generated) PDF.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui::{self, Event, Key, Modifiers, RawInput, Rect, ViewportId, ViewportInfo, pos2, vec2};
use lightweight_pdf_viewer::app::{App, Command, Stats};
use lightweight_pdf_viewer::{config::Config, history::History};

/// Writes a PDF of `pages` blank US Letter pages and returns its path.
fn blank_pdf(name: &str, pages: usize) -> PathBuf {
    let mut objects = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        format!(
            "<< /Type /Pages /Count {pages} /Kids [{}] >>",
            (0..pages).map(|i| format!("{} 0 R", i + 4)).collect::<Vec<_>>().join(" ")
        ),
        "<< /Length 0 >>\nstream\n\nendstream".to_string(),
    ];
    objects.extend(
        (0..pages).map(|_| "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 3 0 R >>".to_string()),
    );
    let mut pdf = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend(format!("{} 0 obj\n{object}\nendobj\n", i + 1).bytes());
    }
    let xref = pdf.len();
    pdf.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).bytes());
    for offset in offsets {
        pdf.extend(format!("{offset:010} 00000 n \n").bytes());
    }
    pdf.extend(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objects.len() + 1).bytes());
    let path = std::env::temp_dir().join(format!("lightweight-pdf-viewer-{}-{name}.pdf", std::process::id()));
    std::fs::write(&path, pdf).unwrap();
    path
}

struct Harness {
    ctx: egui::Context,
    app: App,
    time: f64,
}

impl Harness {
    fn new() -> Self {
        let ctx = egui::Context::default();
        let app = App::new(&ctx, Config::default(), History::default(), Vec::new());
        Self { ctx, app, time: 0.0 }
    }

    fn frame(&mut self, events: Vec<Event>) {
        self.time += 1.0 / 60.0;
        let mut raw = RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(1000.0, 800.0))),
            time: Some(self.time),
            events,
            ..Default::default()
        };
        raw.viewports.insert(
            ViewportId::ROOT,
            ViewportInfo { native_pixels_per_point: Some(1.0), focused: Some(true), ..Default::default() },
        );
        let app = &mut self.app;
        let _ = self.ctx.run(raw, |ctx| app.ui(ctx));
    }

    fn press(&mut self, key: Key) {
        self.press_with(key, Modifiers::NONE);
    }

    fn press_with(&mut self, key: Key, modifiers: Modifiers) {
        self.frame(vec![Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers }]);
        // The scroll lands on the next frame.
        self.frame(vec![]);
    }

    fn wait(&mut self, done: impl Fn(&Stats) -> bool) {
        let start = Instant::now();
        while !done(&self.app.stats()) {
            let s = self.app.stats();
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "timed out: rotation {} pages {} ready {} tabs {}",
                s.rotation,
                s.page_count,
                s.ready,
                s.tabs
            );
            self.frame(vec![]);
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

/// Home lands on the first page; paging down from there must move the current page on.
#[test]
fn paging_down_after_home_updates_the_current_page() {
    let path = blank_pdf("paging", 6);
    let mut h = Harness::new();
    h.app.open(&h.ctx, path.clone());
    h.wait(|s| s.page_count == 6);
    h.frame(vec![]);

    h.press(Key::Home);
    assert_eq!(h.app.stats().current_page, 0);
    for _ in 0..4 {
        h.press(Key::PageDown);
    }
    let page = h.app.stats().current_page;
    let _ = std::fs::remove_file(path);
    assert!(page > 0, "still on page {page} after four page-downs");
}

/// Rotating keeps the page at the top of the window, as reopening does, also when turned
/// again before the last turn loaded; the file reopens turned the way it was left.
#[test]
fn rotation_keeps_the_page_and_is_remembered() {
    let path = blank_pdf("rotation", 6);
    let mut h = Harness::new();
    h.app.open(&h.ctx, path.clone());
    h.wait(|s| s.page_count == 6);
    h.frame(vec![]);
    // To the top of page 3, which is then both the current page and the one at the top.
    for _ in 0..3 {
        h.press(Key::ArrowRight);
    }
    let page = h.app.stats().current_page;
    assert_eq!(page, 3);

    h.press_with(Key::R, Modifiers::COMMAND);
    h.wait(|s| s.rotation == 90 && s.page_count == 6 && s.ready);
    assert_eq!(h.app.stats().current_page, page);
    h.press_with(Key::L, Modifiers::COMMAND);
    h.press_with(Key::L, Modifiers::COMMAND);
    h.wait(|s| s.rotation == 270 && s.page_count == 6 && s.ready);
    assert_eq!(h.app.stats().current_page, page);

    h.press_with(Key::W, Modifiers::COMMAND);
    h.wait(|s| s.tabs == 0);
    h.app.open(&h.ctx, path.clone());
    h.wait(|s| s.page_count == 6 && s.ready);
    let stats = h.app.stats();
    let _ = std::fs::remove_file(path);
    assert_eq!(stats.rotation, 270);
    assert_eq!(stats.current_page, page);
}

/// The right-click menu's "Rotate This Page" turns one page; rotating all turns the rest
/// alike and keeps that page a quarter ahead. Both are remembered.
#[test]
fn single_pages_rotate_alone() {
    let path = blank_pdf("page-rotation", 6);
    let mut h = Harness::new();
    h.app.open(&h.ctx, path.clone());
    h.wait(|s| s.page_count == 6);
    h.frame(vec![]);
    for _ in 0..3 {
        h.press(Key::ArrowRight);
    }
    assert_eq!(h.app.stats().current_page, 3);

    h.app.run(&h.ctx, 0, Command::RotatePage { page: 3, clockwise: true });
    h.wait(|s| s.page_count == 6 && s.ready);
    assert_eq!(h.app.stats().rotation, 90);
    h.press(Key::ArrowLeft);
    assert_eq!((h.app.stats().current_page, h.app.stats().rotation), (2, 0));

    h.press_with(Key::R, Modifiers::COMMAND);
    h.wait(|s| s.page_count == 6 && s.ready && s.rotation == 90);
    h.press(Key::ArrowRight);
    assert_eq!((h.app.stats().current_page, h.app.stats().rotation), (3, 180));

    h.press_with(Key::W, Modifiers::COMMAND);
    h.wait(|s| s.tabs == 0);
    h.app.open(&h.ctx, path.clone());
    h.wait(|s| s.page_count == 6 && s.ready);
    let stats = h.app.stats();
    let _ = std::fs::remove_file(path);
    assert_eq!((stats.current_page, stats.rotation), (3, 180));
}
