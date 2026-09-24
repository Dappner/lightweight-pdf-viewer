//! The PDF engine the render thread drives: opens documents, reads their structure and
//! rasterizes pages. CoreGraphics on macOS; elsewhere a synthetic stand-in, so everything
//! else can be built, tested and benchmarked on any platform.

#[cfg(target_os = "macos")]
mod coregraphics;
/// Text search reads page text through PDFKit, which needs the same page geometry.
#[cfg(target_os = "macos")]
pub(crate) use coregraphics::PageGeometry;
#[cfg(target_os = "macos")]
pub use coregraphics::{Bitmap, Document, Page, autoreleasepool};

#[cfg(not(target_os = "macos"))]
mod synthetic;
#[cfg(not(target_os = "macos"))]
pub use synthetic::{Bitmap, Document, Page, autoreleasepool};
