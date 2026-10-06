//! Headless checks of PNG images opened through the real app, as documents of one page.
//! macOS only: elsewhere the synthetic engine opens every file as a stack of letter pages.
#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui::{self, Event, Key, Modifiers, RawInput, Rect, ViewportId, ViewportInfo, pos2, vec2};
use lightweight_pdf_viewer::app::{App, Stats};
use lightweight_pdf_viewer::{config::Config, history::History};

/// Writes a `w`x`h` grey PNG and returns its path.
fn grey_png(name: &str, w: u32, h: u32) -> PathBuf {
    let path = std::env::temp_dir().join(format!("lightweight-pdf-viewer-{}-{name}.png", std::process::id()));
    let mut enc = png::Encoder::new(std::fs::File::create(&path).unwrap(), w, h);
    enc.set_color(png::ColorType::Grayscale);
    enc.write_header().unwrap().write_image_data(&vec![128; (w * h) as usize]).unwrap();
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

    fn key(&mut self, key: Key, modifiers: Modifiers) {
        self.frame(vec![Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers }]);
    }

    fn wait(&mut self, what: &str, done: impl Fn(&Stats) -> bool) {
        let start = Instant::now();
        while !done(&self.app.stats()) {
            assert!(start.elapsed() < Duration::from_secs(10), "timed out waiting for {what}");
            self.frame(vec![]);
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

/// A PNG opens as one page and renders sharp; searching it finds no text, without errors.
#[test]
fn png_opens_renders_and_has_nothing_to_find() {
    let path = grey_png("photo", 1200, 900);
    let mut h = Harness::new();
    h.app.open(&h.ctx, path.clone());
    h.wait("the image", |s| s.page_count == 1);
    h.wait("sharp tiles", |s| s.ready);

    h.key(Key::F, Modifiers::COMMAND);
    h.frame(vec![Event::Text("anything".into())]);
    h.wait("the search", |s| s.find_done);
    let find = h.app.stats().find;
    let _ = std::fs::remove_file(path);
    assert_eq!(find.as_deref(), Some("No searchable text"));
}
