use std::path::PathBuf;

use eframe::egui;
use lightweight_pdf_viewer::app::{self, App};
use lightweight_pdf_viewer::{config::Config, finder, history::History, memory, menu, titlebar};

fn main() -> eframe::Result {
    let config = Config::load();
    memory::relaunch(config.memory);
    let files: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    finder::install();
    // The main window reopens where the first saved window was.
    let main_window = match config.windows.first().and_then(|w| w.frame).filter(|_| config.restore_tabs) {
        Some([x, y, w, h]) => app::window_at(egui::pos2(x, y), egui::vec2(w, h)),
        None => app::window_builder(),
    };
    let options = eframe::NativeOptions {
        viewport: main_window,
        renderer: eframe::Renderer::Wgpu,
        wgpu_options: egui_wgpu::WgpuConfiguration {
            // Two drawables instead of three. Each is a full-window buffer (28 MB for a
            // near-full-screen window on a 14" MacBook Pro), and frames here are cheap to draw.
            desired_maximum_frame_latency: Some(1),
            ..Default::default()
        },
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
