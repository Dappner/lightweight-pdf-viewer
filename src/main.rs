use std::path::PathBuf;

use eframe::egui;
use lightweight_pdf_viewer::{app::App, config::Config, finder, history::History, memory, menu, titlebar};

fn main() -> eframe::Result {
    let config = Config::load();
    memory::relaunch(config.memory);
    let files: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    finder::install();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Lightweight PDF")
            // An empty icon keeps the bundle's icon; otherwise eframe swaps in egui's logo.
            .with_icon(egui::IconData::default())
            .with_inner_size([960.0, 1080.0])
            .with_min_inner_size([480.0, 360.0])
            .with_fullsize_content_view(true)
            .with_titlebar_shown(false)
            .with_title_shown(false)
            .with_drag_and_drop(true),
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };
    eframe::run_native(
        "Lightweight PDF",
        options,
        Box::new(|cc| {
            menu::install(&cc.egui_ctx);
            titlebar::install();
            Ok(Box::new(App::new(&cc.egui_ctx, config, History::load(), files)))
        }),
    )
}
