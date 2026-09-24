//! Headless check of the search worker's memory and latency on real documents.
//!
//!   cargo bench --bench search -- a.pdf b.pdf [...]
//!
//! Each file runs in fresh child processes (with `MallocSpaceEfficient=1`, as the default
//! preset), as if relaunching the app between them:
//!
//! - without the disk index, with the normal cache budget and with a 1 MB one that forces
//!   pages out of the memory cache;
//! - with an empty disk index (cold), which the run fills;
//! - with that index (warm), with both budgets.
//!
//! Each run searches for "the" from the middle of the document, steps through
//! matches and changes the query twice. Then it stops the worker (as a tab left in the
//! background does) and gets highlights again, stops it again partway through a new scan and
//! resumes that scan, and finally closes the search.
//!
//! It fails if the cache goes over its budget, a warm run's first scan extracts pages with
//! PDFKit instead of reading them from the index, the process footprint goes over the budget
//! plus [`PDFKIT_ALLOWANCE`], counts differ after the query changes back or after a resumed
//! scan, a stopped worker doesn't exit, or the footprint doesn't settle within
//! [`RESIDUAL_TOLERANCE`] of where it started.
//!
//! macOS only: it reads text with PDFKit, which the synthetic engine elsewhere has no stand-in for.

#[cfg(target_os = "macos")]
#[path = "search/macos.rs"]
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
