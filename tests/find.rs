//! Headless checks of ⌘F search through the real app, on a generated PDF with text. macOS only:
//! elsewhere the synthetic engine has no text to find.
#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::time::Duration;

use eframe::egui::{Event, Key, Modifiers};
use lightweight_pdf_viewer::config::Config;

mod common;
use common::{Harness, placed_text_pdf};

/// Writes a PDF with one line of Helvetica text per page and returns its path.
fn text_pdf(name: &str, lines: &[&str]) -> PathBuf {
    let pages: Vec<Vec<(&str, i32)>> = lines.iter().map(|l| vec![(*l, 400)]).collect();
    placed_text_pdf(name, &pages)
}

const LINES: [&str; 8] = [
    "Chapter one",
    "filler text",
    "the needle is here",
    "more filler",
    "and more",
    "still nothing",
    "a second NEEDLE",
    "the end",
];

/// Opens a PDF of [`LINES`] and searches it for "needle", landing on the first match.
fn searched(h: &mut Harness, name: &str) -> PathBuf {
    let path = text_pdf(name, &LINES);
    h.app.open(&h.ctx, path.clone());
    h.wait("the document", |s| s.page_count == LINES.len() && s.find.is_none());
    h.frame(vec![]);
    h.key(Key::F, Modifiers::COMMAND);
    h.frame(vec![Event::Text("needle".into())]);
    h.wait("the first match", |s| s.find.as_deref() == Some("1 of 2") && s.current_page == 2);
    path
}

/// A tab switched away from pauses its search, and stops its worker once its grace period
/// ends; switching back keeps the results, and stepping still works.
#[test]
fn background_search_lets_go_of_its_worker() {
    let mut h = Harness::new();
    let first = searched(&mut h, "background-a");
    assert_eq!(h.app.stats().search_workers, 1);

    let second = text_pdf("background-b", &["nothing to see"]);
    h.app.open(&h.ctx, second.clone());
    h.wait("the second tab", |s| s.active == 1 && s.page_count == 1);
    assert_eq!(h.app.stats().search_workers, 1, "paused, not stopped, during the grace period");
    h.time += 4.0;
    h.wait("the worker to stop", |s| s.search_workers == 0);

    h.key(Key::Num1, Modifiers::COMMAND);
    h.frame(vec![]);
    assert_eq!(h.app.stats().find.as_deref(), Some("1 of 2"), "results survive");
    h.key(Key::G, Modifiers::COMMAND);
    h.wait("the second match", |s| s.find.as_deref() == Some("2 of 2") && s.current_page == 6);
    h.wait("a worker for the highlights", |s| s.search_workers == 1);
    let _ = std::fs::remove_file(first);
    let _ = std::fs::remove_file(second);
}

/// An unfocused window keeps its pages and search usable.
#[test]
fn unfocused_window_keeps_pages_and_search() {
    let mut h = Harness::new();
    let path = searched(&mut h, "unfocused");
    h.wait("sharp pages", |s| s.ready);
    h.focused = false;
    h.frame(vec![]);
    h.time += 11.0;
    h.frame(vec![]);
    assert!(h.app.stats().ready, "unfocused window lost its rendered pages");
    assert!(h.app.stats().texture_bytes > 0);
    assert_eq!(h.app.stats().find.as_deref(), Some("1 of 2"));

    h.focused = true;
    h.frame(vec![]);
    h.key(Key::G, Modifiers::COMMAND);
    h.wait("the second match", |s| s.find.as_deref() == Some("2 of 2") && s.current_page == 6);
    let _ = std::fs::remove_file(path);
}

/// Searching from halfway down a page picks the first match below that point, not one
/// above it on the same page.
#[test]
fn first_match_is_below_the_top_of_the_viewport() {
    let pages = vec![
        vec![("needle up top", 700), ("needle further down", 100)],
        vec![("one more needle", 400)],
        vec![("the end", 400)],
    ];
    let path = placed_text_pdf("within-page", &pages);
    let mut h = Harness::new();
    h.app.open(&h.ctx, path.clone());
    h.wait("the document", |s| s.page_count == 3);
    h.frame(vec![]);
    // Down to about halfway through the first page.
    for _ in 0..12 {
        h.key(Key::ArrowDown, Modifiers::NONE);
    }
    h.frame(vec![]);
    assert_eq!(h.app.stats().current_page, 0);

    h.key(Key::F, Modifiers::COMMAND);
    h.frame(vec![Event::Text("needle".into())]);
    h.wait("the match below", |s| s.find.as_deref() == Some("2 of 3") && s.current_page == 0);
    h.key(Key::Enter, Modifiers::SHIFT);
    h.wait("the one above", |s| s.find.as_deref() == Some("1 of 3"));
    let _ = std::fs::remove_file(path);
}

#[test]
fn finds_steps_through_and_closes() {
    let path = text_pdf("find", &LINES);
    let mut h = Harness::new();
    h.app.open(&h.ctx, path.clone());
    h.wait("the document", |s| s.page_count == LINES.len());
    h.frame(vec![]);

    h.key(Key::F, Modifiers::COMMAND);
    assert_eq!(h.app.stats().find.as_deref(), Some(""), "⌘F opens an empty find bar");
    h.frame(vec![Event::Text("needle".into())]);
    h.wait("the first match", |s| s.find.as_deref() == Some("1 of 2") && s.current_page == 2);

    h.key(Key::Enter, Modifiers::NONE);
    h.wait("the second match", |s| s.find.as_deref() == Some("2 of 2") && s.current_page == 6);
    h.key(Key::Enter, Modifiers::NONE);
    h.wait("wrapping to the first", |s| s.find.as_deref() == Some("1 of 2") && s.current_page == 2);
    h.key(Key::Enter, Modifiers::SHIFT);
    h.wait("back to the second", |s| s.find.as_deref() == Some("2 of 2") && s.current_page == 6);
    h.key(Key::G, Modifiers::COMMAND);
    h.wait("⌘G to the first", |s| s.find.as_deref() == Some("1 of 2") && s.current_page == 2);

    // Selecting everything and typing replaces the query.
    h.key(Key::F, Modifiers::COMMAND);
    h.frame(vec![]);
    h.frame(vec![Event::Text("haystack".into())]);
    h.wait("no matches", |s| s.find.as_deref() == Some("No matches"));

    h.key(Key::Escape, Modifiers::NONE);
    let find = h.app.stats().find;
    let _ = std::fs::remove_file(path);
    assert_eq!(find, None, "Esc closes the find bar");
}

/// A config that saves searched text in `dir`.
fn saving_to(dir: &std::path::Path) -> Config {
    let mut config = Config::default();
    config.search_cache_dir = Some(dir.to_owned());
    config
}

/// A document searched once is searched from its saved text after a relaunch, without
/// PDFKit; an edited one is read again, and clearing the cache deletes the text.
#[test]
fn searched_text_is_saved_between_launches() {
    let dir = std::env::temp_dir().join(format!("lightweight-pdf-viewer-{}-index", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let pages = LINES.len() as u64;

    let mut h = Harness::with_config(saving_to(&dir));
    let path = searched(&mut h, "saved");
    h.wait("every page read", |s| s.find_reads.0 + s.find_reads.1 >= pages);
    assert_eq!(h.app.stats().find_reads, (0, pages), "the first search extracts every page");
    h.key(Key::Escape, Modifiers::NONE);
    drop(h);
    let saved = std::fs::read_dir(&dir).unwrap().count();
    assert_eq!(saved, 1, "one file for the document");

    let mut h = Harness::with_config(saving_to(&dir));
    h.app.open(&h.ctx, path.clone());
    h.wait("the document", |s| s.page_count == LINES.len());
    h.frame(vec![]);
    h.key(Key::F, Modifiers::COMMAND);
    h.frame(vec![Event::Text("needle".into())]);
    h.wait("the first match", |s| s.find.as_deref() == Some("1 of 2") && s.current_page == 2);
    h.wait("every page read", |s| s.find_reads.0 >= pages);
    assert_eq!(h.app.stats().find_reads, (pages, 0), "after a relaunch, every page comes from disk");
    h.key(Key::Escape, Modifiers::NONE);
    drop(h);

    // The same file edited: its saved text no longer applies.
    let lines: Vec<&str> = LINES.iter().map(|l| if l.contains("NEEDLE") { "no longer here" } else { l }).collect();
    std::thread::sleep(Duration::from_millis(10));
    let edited = text_pdf("saved", &lines);
    assert_eq!(edited, path);
    let mut h = Harness::with_config(saving_to(&dir));
    h.app.open(&h.ctx, path.clone());
    h.wait("the document", |s| s.page_count == LINES.len());
    h.frame(vec![]);
    h.key(Key::F, Modifiers::COMMAND);
    h.frame(vec![Event::Text("needle".into())]);
    h.wait("the only match", |s| s.find.as_deref() == Some("1 of 1"));
    h.wait("every page read", |s| s.find_reads.1 >= pages);
    assert_eq!(h.app.stats().find_reads, (0, pages), "an edited PDF is extracted again");

    h.app.run(&h.ctx, 0, lightweight_pdf_viewer::app::Command::ClearSearchCache);
    let left = std::fs::read_dir(&dir).unwrap().count();
    drop(h);
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(left, 0, "clearing the cache deletes the saved text");
}
