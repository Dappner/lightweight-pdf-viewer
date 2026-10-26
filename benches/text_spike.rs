//! Measurement spike for text selection: what a per-page text layer (the page string plus a
//! rectangle per character) would cost to build with PDFKit, against asking PDFKit to
//! hit-test each pointer move. Not a regression benchmark.
//!
//!   cargo bench --bench text_spike -- a.pdf b.pdf [...]
//!
//! Each file runs in a fresh child process (with `MallocSpaceEfficient=1`, as the default
//! preset). Up to 60 pages spread over the document, each never touched before:
//!
//! - string: the first `page.string()`, which parses the page.
//! - bounds: a one-unit selection's box for every UTF-16 unit, after the string.
//! - lines: `selectionForRange(whole page)` → `selectionsByLine` → `boundsForPage`.
//! - hit: `characterIndexAtPoint` at 200 points on a page already parsed, per call.
//!
//! Then the whole document's text layer, reopening every 16 pages and draining the
//! autorelease pool after each, as the app does: total time, peak
//! footprint, and what the layer would take in memory at 16 bytes a character.
//!
//! macOS only: it reads text with PDFKit, which the synthetic engine elsewhere has no stand-in for.

#[cfg(target_os = "macos")]
mod common;
#[cfg(target_os = "macos")]
#[path = "text_spike/macos.rs"]
mod macos;

#[cfg(target_os = "macos")]
fn main() {
    macos::main();
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("this benchmark reads text with PDFKit: run it on macOS");
    std::process::exit(1);
}
