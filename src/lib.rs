//! A tabbed, memory-lean PDF viewer. The library holds the whole app so the benchmarks
//! can drive it headless; `main.rs` only opens the window.

pub mod app;
mod cache;
mod case_folding;
pub mod config;
mod engine;
#[cfg(target_os = "macos")]
pub mod finder;
mod find_bar;
pub mod history;
mod layout;
pub mod memory;
#[cfg(target_os = "macos")]
pub mod menu;
mod quick_open;
mod renderer;
pub mod search;
pub mod search_index;
mod sidebar;
mod tab;
mod text;
mod tab_bar;
mod theme;
#[cfg(target_os = "macos")]
pub mod titlebar;
mod view;
mod window;

#[cfg(not(target_os = "macos"))]
mod unsupported;
#[cfg(not(target_os = "macos"))]
pub use unsupported::{finder, menu, titlebar};
