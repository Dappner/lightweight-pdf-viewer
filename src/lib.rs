//! A tabbed, memory-lean PDF viewer. The library holds the whole app so the benchmarks
//! can drive it headless; `main.rs` only opens the window.

pub mod app;
mod cache;
pub mod config;
pub mod finder;
pub mod history;
mod layout;
pub mod memory;
pub mod menu;
mod quick_open;
mod renderer;
mod sidebar;
mod tab;
mod tab_bar;
mod theme;
pub mod titlebar;
mod view;
