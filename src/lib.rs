//! A tabbed, memory-lean PDF viewer. The library holds the whole app so the benchmarks
//! can drive it headless; `main.rs` only opens the window.

pub mod app;
mod cache;
mod case_folding;
pub mod config;
pub mod finder;
mod find_bar;
pub mod history;
mod layout;
pub mod memory;
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
pub mod titlebar;
mod view;
mod window;
