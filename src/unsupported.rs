//! No-op stand-ins for the macOS integrations (menu bar, Finder, title bar, file dialog) on
//! other platforms, where the crate builds only so its portable core can be tested and
//! benchmarked.

pub mod finder {
    use std::path::PathBuf;

    use eframe::egui;

    pub fn install() {}

    pub fn set_context(_ctx: &egui::Context) {}

    pub fn take() -> Vec<PathBuf> {
        Vec::new()
    }
}

pub mod menu {
    use eframe::egui;

    use crate::app::Command;
    use crate::layout::PageLayout;

    pub fn set_layout(_layout: Option<PageLayout>) {}

    pub fn take() -> Vec<Command> {
        Vec::new()
    }

    pub fn install(_ctx: &egui::Context) {}
}

pub mod titlebar {
    pub fn install() {}
}

/// Files picked in an Open dialog.
pub fn pick_pdfs() -> Vec<std::path::PathBuf> {
    Vec::new()
}
