//! Shared by the headless app tests: generated PDFs and a frame-by-frame harness around the app.
#![allow(dead_code)] // each test crate uses only part of this

use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui::{self, Event, Key, Modifiers, OutputCommand, RawInput, Rect, ViewportId, ViewportInfo, pos2, vec2};
use lightweight_pdf_viewer::app::{App, Stats};
use lightweight_pdf_viewer::{config::Config, history::History};

/// Writes a PDF of US Letter pages, each with lines of text at the given heights (points
/// from the bottom), and returns its path.
pub fn placed_text_pdf(name: &str, page_lines: &[Vec<(&str, i32)>]) -> PathBuf {
    let pages = page_lines.len();
    let mut objects = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        format!(
            "<< /Type /Pages /Count {pages} /Kids [{}] >>",
            (0..pages).map(|i| format!("{} 0 R", 4 + i * 2)).collect::<Vec<_>>().join(" ")
        ),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
    ];
    for (i, lines) in page_lines.iter().enumerate() {
        let content: String =
            lines.iter().map(|(line, y)| format!("BT /F1 18 Tf 72 {y} Td ({line}) Tj ET\n")).collect();
        objects.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 3 0 R >> >> /Contents {} 0 R >>",
            5 + i * 2
        ));
        objects.push(format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()));
    }
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

/// An app driven one egui frame at a time in a 1000x800 window.
pub struct Harness {
    pub ctx: egui::Context,
    pub app: App,
    pub time: f64,
    pub focused: bool,
    /// Text the app put on the clipboard, last first.
    pub copied: Vec<String>,
}

impl Harness {
    pub fn new() -> Self {
        Self::with_config(Config::default())
    }

    pub fn with_config(config: Config) -> Self {
        let ctx = egui::Context::default();
        let app = App::new(&ctx, config, History::default(), Vec::new());
        Self { ctx, app, time: 0.0, focused: true, copied: Vec::new() }
    }

    pub fn frame(&mut self, events: Vec<Event>) {
        self.time += 1.0 / 60.0;
        let mut raw = RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(1000.0, 800.0))),
            time: Some(self.time),
            events,
            ..Default::default()
        };
        raw.viewports.insert(
            ViewportId::ROOT,
            ViewportInfo { native_pixels_per_point: Some(1.0), focused: Some(self.focused), ..Default::default() },
        );
        let app = &mut self.app;
        let output = self.ctx.run(raw, |ctx| app.ui(ctx));
        for command in output.platform_output.commands {
            if let OutputCommand::CopyText(text) = command {
                self.copied.insert(0, text);
            }
        }
    }

    pub fn key(&mut self, key: Key, modifiers: Modifiers) {
        self.frame(vec![Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers }]);
    }

    /// Runs frames, letting worker threads catch up, until `done` or a timeout.
    pub fn wait_until(&mut self, what: &str, done: impl Fn(&Self) -> bool) {
        let start = Instant::now();
        while !done(self) {
            assert!(start.elapsed() < Duration::from_secs(10), "timed out waiting for {what}");
            self.frame(vec![]);
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Like [`Self::wait_until`], on the app's stats.
    pub fn wait(&mut self, what: &str, done: impl Fn(&Stats) -> bool) {
        self.wait_until(what, |h| done(&h.app.stats()));
    }
}
