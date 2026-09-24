//! Headless checks of keyboard navigation through a real (generated) PDF.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui::{self, Event, Key, Modifiers, RawInput, Rect, ViewportId, ViewportInfo, pos2, vec2};
use lightweight_pdf_viewer::app::{App, Stats};
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
        self.frame(vec![Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }]);
        // The scroll lands on the next frame.
        self.frame(vec![]);
    }

    fn wait(&mut self, done: impl Fn(&Stats) -> bool) {
        let start = Instant::now();
        while !done(&self.app.stats()) {
            assert!(start.elapsed() < Duration::from_secs(10), "timed out");
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
