//! Shared by the benches: process memory footprint and a sampler of its peak.
#![allow(dead_code)] // each bench uses only part of this

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

#[cfg(target_os = "macos")]
pub fn footprint_mb() -> f64 {
    let mut info: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        libc::proc_pid_rusage(libc::getpid(), libc::RUSAGE_INFO_V2, &mut info as *mut _ as *mut libc::rusage_info_t)
    };
    assert_eq!(ok, 0, "proc_pid_rusage failed");
    info.ri_phys_footprint as f64 / 1_048_576.0
}

/// Resident memory (the closest Linux has to macOS's footprint); with the synthetic engine
/// this measures the app's own memory, without CoreGraphics' caches.
#[cfg(not(target_os = "macos"))]
pub fn footprint_mb() -> f64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let kb = status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.trim().trim_end_matches(" kB").parse::<f64>().ok());
    kb.unwrap_or(0.0) / 1024.0
}

pub struct PeakSampler {
    stop: Arc<AtomicBool>,
    peak: Arc<AtomicU64>,
    thread: std::thread::JoinHandle<()>,
}

impl PeakSampler {
    pub fn start() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let peak = Arc::new(AtomicU64::new(footprint_mb().to_bits()));
        let thread = {
            let (stop, peak) = (stop.clone(), peak.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let now = footprint_mb();
                    if now > f64::from_bits(peak.load(Ordering::Relaxed)) {
                        peak.store(now.to_bits(), Ordering::Relaxed);
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
            })
        };
        Self { stop, peak, thread }
    }

    pub fn finish(self) -> f64 {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.join().unwrap();
        f64::from_bits(self.peak.load(Ordering::Relaxed)).max(footprint_mb())
    }
}
