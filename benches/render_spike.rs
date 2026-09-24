//! Measurement spike for tile rendering with CoreGraphics: what the render thread's options
//! cost per page, headless. Not a regression benchmark.
//!
//!   cargo bench --bench render_spike -- a.pdf b.pdf [...]
//!
//! Each file renders up to RENDER_PAGES (default 12) pages spread through it at a Retina
//! fit-width scale (RENDER_WIDTH px wide, default 1800), all tiles of a page in a row, as the
//! render thread does. Variants: tile size, interpolation quality, an opaque context, one
//! draw per tile row or per page instead of per tile, thread QoS, and two workers. The gray
//! mode renders every page and counts the tiles without colour (see `gray`).
//!
//! RENDER_ONLY=variants|quality|qos|parallel|gray runs one mode.
//!
//! macOS only: it measures CoreGraphics itself.

#[cfg(target_os = "macos")]
#[path = "render_spike/macos.rs"]
mod macos;

#[cfg(target_os = "macos")]
fn main() {
    macos::main();
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("this benchmark measures CoreGraphics: run it on macOS");
    std::process::exit(1);
}
