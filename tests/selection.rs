//! Headless checks of selecting and copying text through the real app, on a generated PDF
//! with text. macOS only: elsewhere the synthetic engine has no text to select.
#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui::{Event, Key, Modifiers, PointerButton, Pos2, pos2, vec2};

mod common;
use common::{Harness, placed_text_pdf};

impl Harness {
    /// Moves the pointer over `at` for a while, so the page's text is read.
    fn hover(&mut self, at: Pos2) {
        let until = Instant::now() + Duration::from_millis(300);
        let mut x = 0.0;
        while Instant::now() < until {
            x = 1.0 - x;
            self.frame(vec![Event::PointerMoved(at + vec2(x, 0.0))]);
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn button(&mut self, at: Pos2, pressed: bool) {
        self.frame(vec![Event::PointerButton {
            pos: at,
            button: PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        }]);
    }

    /// Drags from `from` to `to` in steps, then releases.
    fn drag(&mut self, from: Pos2, to: Pos2) {
        self.frame(vec![Event::PointerMoved(from)]);
        self.button(from, true);
        for step in 1..=10 {
            self.frame(vec![Event::PointerMoved(from + (to - from) * step as f32 / 10.0)]);
        }
        self.button(to, false);
    }

    /// Copies (⌘C), and waits for the copied text.
    fn copy(&mut self) -> String {
        let before = self.copied.len();
        self.frame(vec![Event::Copy]);
        self.wait_until("copied text", |h| h.copied.len() > before);
        self.copied[0].clone()
    }

    /// Copies (⌘C) every frame until something is copied: once a selection still waiting
    /// for its text has resolved.
    fn copy_once_selected(&mut self) -> String {
        let before = self.copied.len();
        let start = Instant::now();
        while self.copied.len() == before {
            assert!(start.elapsed() < Duration::from_secs(10), "timed out waiting for a selection to copy");
            self.frame(vec![Event::Copy]);
            std::thread::sleep(Duration::from_millis(2));
        }
        self.copied[0].clone()
    }

    /// `count` quick clicks at `at`.
    fn clicks(&mut self, at: Pos2, count: usize) {
        self.frame(vec![Event::PointerMoved(at)]);
        for _ in 0..count {
            self.button(at, true);
            self.button(at, false);
        }
    }

    /// Runs frames for `ms` milliseconds of real time.
    fn idle(&mut self, ms: u64) {
        let until = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < until {
            self.frame(vec![]);
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

/// Opens a PDF with one line of text a page, its first page filling the window, with the
/// sidebar hidden so the page spans its width.
fn opened(h: &mut Harness, name: &str, lines: &[&str]) -> PathBuf {
    let pages: Vec<Vec<(&str, i32)>> = lines.iter().map(|l| vec![(*l, 400)]).collect();
    let path = placed_text_pdf(name, &pages);
    h.app.open(&h.ctx, path.clone());
    h.wait("the document", |s| s.page_count == lines.len());
    h.key(Key::B, Modifiers::COMMAND);
    h.frame(vec![]);
    path
}

/// Dragging from above the first page's text to below it selects all of it, and ⌘C copies it.
#[test]
fn drag_selects_and_copies() {
    let mut h = Harness::new();
    let path = opened(&mut h, "drag", &["Selected words here", "Next page"]);
    let (top, bottom) = (pos2(5.0, 100.0), pos2(995.0, 795.0));
    h.hover(top);
    h.drag(top, bottom);
    assert_eq!(h.copy().trim(), "Selected words here");

    // A click clears the selection: nothing more is copied.
    h.button(pos2(500.0, 500.0), true);
    h.button(pos2(500.0, 500.0), false);
    h.frame(vec![Event::Copy]);
    h.frame(vec![]);
    assert_eq!(h.copied.len(), 1);
    let _ = std::fs::remove_file(path);
}

/// ⌘A selects every page's text; copying it reads the pages not shown.
#[test]
fn select_all_copies_every_page() {
    let mut h = Harness::new();
    let lines = ["First page", "Second page", "Third page"];
    let path = opened(&mut h, "all", &lines);
    h.key(Key::A, Modifiers::COMMAND);
    let copied = h.copy();
    let got: Vec<&str> = copied.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    assert_eq!(got, lines);
    let _ = std::fs::remove_file(path);
}

/// A drag over a page whose text hasn't been read yet (no hover first) still selects once the
/// text arrives, though the press is over by then.
#[test]
fn a_drag_over_unread_text_selects_once_it_arrives() {
    // The text is read on another thread, so the drag may end before or after it arrives. The
    // selection must come out the same either way, so nothing here depends on winning the race.
    let mut h = Harness::new();
    let path = opened(&mut h, "cold-drag", &["Selected words here", "Next page"]);
    let (from, to) = (pos2(5.0, 100.0), pos2(995.0, 795.0));
    h.frame(vec![Event::PointerMoved(from)]);
    h.button(from, true);
    h.frame(vec![Event::PointerMoved(to)]);
    h.button(to, false);
    assert_eq!(h.copy_once_selected().trim(), "Selected words here");
    let _ = std::fs::remove_file(path);
}

/// Double and triple clicks on unread text select the word and line nearest the click.
#[test]
fn clicks_on_unread_text_select_words_and_lines() {
    let mut h = Harness::new();
    let path = opened(&mut h, "cold-clicks", &["Selected words here", "Next page"]);
    // Right of the line: its last word.
    h.clicks(pos2(500.0, 500.0), 2);
    assert_eq!(h.copy_once_selected(), "here");

    h.idle(200);
    h.clicks(pos2(500.0, 500.0), 3);
    assert_eq!(h.copy_once_selected().trim(), "Selected words here");
    let _ = std::fs::remove_file(path);
}

fn many_pages(count: usize) -> Vec<String> {
    (0..count).map(|i| format!("Page number {i}")).collect()
}

/// A copy the worker is still reading never overwrites a newer one taken from loaded text.
#[test]
fn an_older_copy_never_overwrites_a_newer_one() {
    let mut h = Harness::new();
    let lines = many_pages(1500);
    let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
    let path = opened(&mut h, "copy-race", &lines);
    h.key(Key::A, Modifiers::COMMAND);
    h.frame(vec![Event::Copy]);
    assert!(h.copied.is_empty(), "the worker reads the pages not shown");

    h.clicks(pos2(500.0, 500.0), 3);
    assert_eq!(h.copy_once_selected().trim(), "Page number 0");
    h.idle(1000);
    assert_eq!(h.copied.len(), 1, "the older copy was cancelled: {:?}", h.copied.first());
    let _ = std::fs::remove_file(path);
}

/// Switching tabs during a large copy stops the worker; the copy never lands.
#[test]
fn switching_tabs_cancels_a_copy() {
    let mut h = Harness::new();
    let lines = many_pages(1500);
    let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
    let first = opened(&mut h, "copy-switch-a", &lines);
    h.key(Key::A, Modifiers::COMMAND);
    h.frame(vec![Event::Copy]);
    assert_eq!(h.app.stats().text_workers, 1);

    let second = placed_text_pdf("copy-switch-b", &[vec![("Other", 400)]]);
    h.app.open(&h.ctx, second.clone());
    h.wait("the second tab", |s| s.active == 1 && s.page_count == 1);
    assert_eq!(h.app.stats().text_workers, 0);
    h.idle(500);
    assert!(h.copied.is_empty(), "the cancelled copy landed");
    let _ = std::fs::remove_file(first);
    let _ = std::fs::remove_file(second);
}

/// With everything selected and the pages zoomed far out, only the ends of the selection have
/// their text read: the pages between are tinted whole.
#[test]
fn zoomed_out_select_all_reads_only_its_ends() {
    let mut h = Harness::new();
    let lines = many_pages(300);
    let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
    let path = opened(&mut h, "zoomed-out", &lines);
    for _ in 0..12 {
        h.key(Key::Minus, Modifiers::COMMAND);
    }
    h.key(Key::A, Modifiers::COMMAND);
    h.idle(500);
    let one_page = h.app.stats().text_bytes;
    assert!(one_page > 0, "the first page, an end of the selection, is read");
    for _ in 0..20 {
        h.key(Key::PageDown, Modifiers::NONE);
    }
    h.idle(500);
    assert_eq!(h.app.stats().text_bytes, one_page, "no page between the ends was read");
    let _ = std::fs::remove_file(path);
}
