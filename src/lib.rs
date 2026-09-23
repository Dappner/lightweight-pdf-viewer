//! A tabbed, memory-lean PDF viewer. The library holds the whole app so the benchmarks
//! can drive it headless; `main.rs` only opens the window.

pub mod app;
mod cache;
pub mod finder;
mod layout;
pub mod menu;
mod renderer;
mod sidebar;
mod tab;
mod tab_bar;
mod theme;
pub mod titlebar;
mod view;
