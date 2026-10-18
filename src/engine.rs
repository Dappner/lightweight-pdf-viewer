//! The PDF engine: opens documents, reads their structure and rasterizes pages for the render
//! thread, and reads their text for search. CoreGraphics and PDFKit on macOS; elsewhere a
//! synthetic stand-in, so everything else can be built, tested and benchmarked on any platform.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveOutcome {
    Saved,
    PreviewMayResize,
}

#[cfg(target_os = "macos")]
mod coregraphics;
#[cfg(any(target_os = "macos", test))]
mod preview;
#[cfg(target_os = "macos")]
pub use coregraphics::{Bitmap, Document, Page, TextDocument, TextPage, autoreleasepool, print, save_annotations};

#[cfg(not(target_os = "macos"))]
mod synthetic;
#[cfg(not(target_os = "macos"))]
pub use synthetic::{Bitmap, Document, Page, TextDocument, TextPage, autoreleasepool, print, save_annotations};
