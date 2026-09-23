mod app;
mod cache;
mod layout;
mod renderer;
mod sidebar;
mod tab;
mod tab_bar;
mod theme;
mod view;

use std::path::PathBuf;

use eframe::egui;

fn main() -> eframe::Result {
    let files: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("PDF Viewer")
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
        "PDF Viewer",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc, files)))),
    )
}
