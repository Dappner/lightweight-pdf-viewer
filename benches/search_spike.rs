//! Measurement spike for Cmd+F text search: what extracting, caching and highlighting text
//! would cost, headless. Not a regression benchmark.
//!
//!   cargo bench --bench search_spike -- a.pdf b.pdf [...]
//!
//! Every measurement runs in a fresh child process (with `MallocSpaceEfficient=1`, as the
//! default preset) so footprints don't bleed into each other. Modes, per file:
//!
//! - extract: reads every page's text through PDFKit starting mid-document and wrapping, as
//!   search would from the current page, reopening the document every N pages (never, 16,
//!   32, 256). Reports time to the first page, total, per-page latency, peak footprint, and
//!   footprint after releasing the document and then the text.
//! - analyze (with batch 32): cache sizes (text, UTF-16, normalized, offset mappings), and
//!   per query: matches, retained range bytes, scan time over the cache, time to first match.
//! - geometry: highlight rectangles (selectionForRange → selectionsByLine → boundsForPage)
//!   on pages never touched, already rendered, or whose text was already read.
//! - contention: tile render latency on one thread, alone vs. with extraction running.
//!
//! SPIKE_ONLY=extract|geometry|contention runs one mode.
//!
//! macOS only: it reads text with PDFKit, which the synthetic engine elsewhere has no stand-in for.

#[cfg(target_os = "macos")]
mod common;
#[cfg(target_os = "macos")]
#[path = "search_spike/macos.rs"]
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
