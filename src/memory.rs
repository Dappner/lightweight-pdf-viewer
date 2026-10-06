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
    /// Distinct pages a render thread draws before reopening the document. CoreGraphics
    /// keeps every page it has drawn parsed (~0.3 MB each for a textbook) until the document
    /// is closed; reopening takes a millisecond or two.
    pub pages_per_open: usize,
    /// Render threads, each drawing a different page at once. Each keeps its own copy of the
    /// documents it draws, and so of CoreGraphics' caches for them: a second thread made
    /// scrolling sharp ~40% sooner for up to ~13 MB more peak footprint (`benches/session.rs`).
    pub render_threads: usize,
    /// Screen heights rendered ahead, above and below the visible area.
    pub prefetch: f32,
    /// Makes the allocator hand freed memory back to macOS promptly (see [`relaunch`]).
    pub space_efficient_malloc: bool,
    /// Seconds in the background (unfocused, not scrolled) after which the page tiles are
    /// dropped and a snapshot is shown instead; `None` keeps them.
    pub sleep_after: Option<f64>,
}

const MB: usize = 1024 * 1024;

impl Memory {
    pub fn tuning(self) -> Tuning {
        match self {
            Memory::Low => Tuning {
                texture_budget: 16 * MB,
                pages_per_open: 16,
                render_threads: 1,
                prefetch: 0.25,
                space_efficient_malloc: true,
                sleep_after: Some(2.0),
            },
            Memory::Balanced => Tuning {
                texture_budget: 48 * MB,
                pages_per_open: 32,
                render_threads: 1,
                prefetch: 0.5,
                space_efficient_malloc: true,
                sleep_after: Some(10.0),
            },
            Memory::Fast => Tuning {
                texture_budget: 128 * MB,
                pages_per_open: 256,
                render_threads: 3,
                prefetch: 1.0,
                space_efficient_malloc: false,
                sleep_after: None,
            },
        }
    }
}

/// Where the process's memory goes, in bytes, as macOS counts it.
#[derive(Clone, Copy, Default, Debug)]
pub struct Usage {
    /// The whole footprint, the "Memory" column in Activity Monitor. On Apple Silicon it
    /// includes GPU memory, which is system memory.
    pub footprint: u64,
    /// Memory allocated with `malloc`: CoreGraphics' parsed pages and fonts, text, egui.
    pub heap: u64,
    /// Memory the graphics driver allocated for the app: textures and buffers.
    pub graphics: u64,
}

#[cfg(target_os = "macos")]
pub fn usage() -> Option<Usage> {
    // SAFETY: each call fills a struct of the kind it is given, of the size it is told.
    unsafe {
        let mut vm: TaskVmInfo = std::mem::zeroed();
        let mut count = (size_of::<TaskVmInfo>() / size_of::<libc::natural_t>()) as libc::mach_msg_type_number_t;
        let ok = libc::task_info(mach_task_self_, TASK_VM_INFO, &mut vm as *mut _ as libc::task_info_t, &mut count);
        if ok != libc::KERN_SUCCESS {
            return None;
        }
        let mut heap: libc::malloc_statistics_t = std::mem::zeroed();
        // A null zone sums every zone.
        libc::malloc_zone_statistics(std::ptr::null_mut(), &mut heap);
        Some(Usage {
            footprint: vm.phys_footprint,
            heap: heap.size_in_use as u64,
            graphics: vm.ledger_tag_graphics_footprint.max(0) as u64,
        })
    }
}

#[cfg(not(target_os = "macos"))]
pub fn usage() -> Option<Usage> {
    None
}

#[cfg(target_os = "macos")]
const TASK_VM_INFO: libc::task_flavor_t = 22;

// The port for this process, which `mach_task_self()` returns in C. libc's binding is
// deprecated in favour of the `mach2` crate, which is a lot of crate for one static.
#[cfg(target_os = "macos")]
unsafe extern "C" {
    static mach_task_self_: libc::mach_port_t;
}

/// `task_vm_info` from `<mach/task_info.h>` up to its revision 3 fields; `task_info` fills
/// only as much as it is told is there.
#[cfg(target_os = "macos")]
#[repr(C)]
struct TaskVmInfo {
    virtual_size: u64,
    region_count: i32,
    page_size: i32,
    /// `resident_size` to `compressed_lifetime`.
    sizes: [u64; 16],
    phys_footprint: u64,
    min_address: u64,
    max_address: u64,
    /// `ledger_phys_footprint_peak` to `ledger_tag_media_nofootprint_compressed`.
    ledgers: [i64; 13],
    ledger_tag_graphics_footprint: i64,
    /// `ledger_tag_graphics_footprint_compressed` to `ledger_tag_neural_nofootprint_compressed`.
    more_ledgers: [i64; 7],
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
