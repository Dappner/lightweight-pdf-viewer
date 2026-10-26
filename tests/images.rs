//! Headless checks of PNG images opened through the real app, as documents of one page.
//! macOS only: elsewhere the synthetic engine opens every file as a stack of letter pages.
#![cfg(target_os = "macos")]

use std::path::PathBuf;

use eframe::egui::{Event, Key, Modifiers};

mod common;
use common::Harness;

/// Writes a `w`x`h` grey PNG and returns its path.
fn grey_png(name: &str, w: u32, h: u32) -> PathBuf {
    let path = std::env::temp_dir().join(format!("lightweight-pdf-viewer-{}-{name}.png", std::process::id()));
    let mut enc = png::Encoder::new(std::fs::File::create(&path).unwrap(), w, h);
    enc.set_color(png::ColorType::Grayscale);
    enc.write_header().unwrap().write_image_data(&vec![128; (w * h) as usize]).unwrap();
    path
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
