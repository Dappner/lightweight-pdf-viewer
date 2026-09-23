//! Memory presets: how much the app keeps around to make scrolling and going back fast.
//!
//! Chosen with `"memory"` in the config file. Measured with `benches/session.rs` on five
//! 700–1400 page textbooks at 2× (headless, so GPU textures are excluded): after scrolling a
//! whole book, the process used ~400 MB before these presets and ~30 MB with `balanced`.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Memory {
    /// Keeps only what is on screen; pages you return to are redrawn.
    Low,
    #[default]
    Balanced,
    /// Keeps more rendered and parsed pages, at several times the memory.
    Fast,
}

pub struct Tuning {
    /// GPU budget for rendered tiles and thumbnails, shared by every tab. Tiles on screen are
    /// never evicted, so a budget below what the window shows only stops older tiles being kept.
    pub texture_budget: usize,
    /// Distinct pages the render thread draws before reopening the document. CoreGraphics
    /// keeps every page it has drawn parsed (~0.3 MB each for a textbook) until the document
    /// is closed; reopening takes a millisecond or two.
    pub pages_per_open: usize,
    /// Screen heights rendered ahead, above and below the visible area.
    pub prefetch: f32,
    /// Makes the allocator hand freed memory back to macOS promptly (see [`relaunch`]).
    pub space_efficient_malloc: bool,
}

const MB: usize = 1024 * 1024;

impl Memory {
    pub fn tuning(self) -> Tuning {
        match self {
            Memory::Low => Tuning {
                texture_budget: 16 * MB,
                pages_per_open: 16,
                prefetch: 0.25,
                space_efficient_malloc: true,
            },
            Memory::Balanced => Tuning {
                texture_budget: 48 * MB,
                pages_per_open: 32,
                prefetch: 0.5,
                space_efficient_malloc: true,
            },
            Memory::Fast => Tuning {
                texture_budget: 128 * MB,
                pages_per_open: 256,
                prefetch: 1.0,
                space_efficient_malloc: false,
            },
        }
    }
}

const SPACE_EFFICIENT: &str = "MallocSpaceEfficient";

/// Restarts the process if `MallocSpaceEfficient=1` is not set as the preset wants. Without
/// it the system allocator keeps memory freed after rendering (half of it, after a long
/// session) counted against the app. It only reads the setting at launch, so this must run
/// first thing in `main`.
///
/// The app bundle's `LSEnvironment` sets it, so launching from Finder or the Dock with the
/// default presets needs no restart; `fast` restarts to clear it. Launching the binary
/// directly (`cargo run`) restarts to set it. A restart costs about 5 ms.
pub fn relaunch(memory: Memory) {
    use std::os::unix::process::CommandExt;

    let wanted = memory.tuning().space_efficient_malloc;
    if std::env::var_os(SPACE_EFFICIENT).is_some_and(|v| v == "1") == wanted {
        return;
    }
    let Ok(exe) = std::env::current_exe() else { return };
    let mut args = std::env::args_os();
    let mut command = std::process::Command::new(exe);
    if let Some(arg0) = args.next() {
        command.arg0(arg0);
    }
    if wanted {
        command.env(SPACE_EFFICIENT, "1");
    } else {
        command.env_remove(SPACE_EFFICIENT);
    }
    // `exec` only returns on failure, in which case the app runs on as it is.
    let _ = command.args(args).exec();
}
