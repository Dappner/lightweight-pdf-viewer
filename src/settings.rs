//! ⌘, settings: the app's memory use, and the choices in the config file. Each change is
//! applied and saved as it is made.

use eframe::egui::{
    Align, Align2, Button, Color32, ComboBox, Context, CornerRadius, Frame, Id, Layout, Margin, Modal, RichText,
    Stroke, Ui, vec2,
};

use crate::app::Command;
use crate::config::Config;
use crate::memory::{self, Memory, Usage};
use crate::theme::Palette;

const WIDTH: f32 = 420.0;
const TOP: f32 = 80.0;
/// Seconds between readings of the footprint.
const SAMPLE_SECONDS: f64 = 1.0;
const MB: u64 = 1024 * 1024;
const CACHE_SIZES: [u64; 4] = [0, 64, 256, 1024];
const CACHE_DAYS: [u64; 3] = [7, 30, 90];

/// What the settings show besides the config.
pub struct Status<'a> {
    pub config: &'a Config,
    /// The preset the app launched with: the render threads and the allocator keep it.
    pub launched: Memory,
    pub texture_bytes: usize,
    pub texture_budget: usize,
    /// See [`crate::window::Shared::window_buffers`].
    pub window_buffers: u64,
}

#[derive(Default)]
pub struct Settings {
    usage: Option<Usage>,
    /// When `usage` was read; `None` before the first reading.
    sampled_at: Option<f64>,
}

pub enum Outcome {
    Run(Command),
    Dismiss,
}

impl Settings {
    pub fn show(&mut self, ctx: &Context, palette: &Palette, status: &Status) -> Option<Outcome> {
        let now = ctx.input(|i| i.time);
        if self.sampled_at.is_none_or(|t| now - t >= SAMPLE_SECONDS) {
            self.usage = memory::usage();
            self.sampled_at = Some(now);
        }
        ctx.request_repaint_after(std::time::Duration::from_secs_f64(SAMPLE_SECONDS));

        let width = WIDTH.min(ctx.content_rect().width() - 32.0);
        let area = Modal::default_area(Id::new("settings")).anchor(Align2::CENTER_TOP, vec2(0.0, TOP));
        let frame = Frame::new()
            .fill(palette.content)
            .stroke(Stroke::new(1.0_f32, palette.separator))
            .corner_radius(CornerRadius::same(10))
            .inner_margin(Margin::same(16));
        let modal = Modal::new(Id::new("settings"))
            .area(area)
            .frame(frame)
            .backdrop_color(Color32::from_black_alpha(60))
            .show(ctx, |ui| self.contents(ui, width, palette, status));
        if modal.should_close() {
            return Some(Outcome::Dismiss);
        }
        modal.inner.map(Outcome::Run)
    }

    fn contents(&self, ui: &mut Ui, width: f32, palette: &Palette, status: &Status) -> Option<Command> {
        ui.set_width(width);
        let config = status.config;
        let mut command = None;
        let weak = |text: &str| RichText::new(text).color(palette.text_weak).size(12.0);

        ui.label(RichText::new("Settings").color(palette.text).size(15.0).strong());
        ui.add_space(10.0);

        self.usage_rows(ui, palette, status);
        ui.add_space(8.0);
        ui.separator();
        ui.add_space(8.0);

        ui.horizontal(|ui| {
            ui.label(RichText::new("Memory").color(palette.text));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                // Right to left, so the presets go in reverse.
                for (label, preset) in [("Fast", Memory::Fast), ("Balanced", Memory::Balanced), ("Low", Memory::Low)] {
                    if ui.add(Button::selectable(config.memory == preset, label)).clicked() && config.memory != preset {
                        command = Some(Command::SetMemory(preset));
                    }
                }
            });
        });
        ui.label(weak(match config.memory {
            Memory::Low => "Keeps only what is on screen; pages you return to are redrawn.",
            Memory::Balanced => "Keeps nearby pages ready; inactive tabs release their pages.",
            Memory::Fast => "Keeps more pages drawn and ready, at several times the memory.",
        }));
        if config.memory != status.launched {
            ui.label(weak("Some of this takes effect when the app next opens."));
        }

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(8.0);

        let mut restore = config.restore_tabs;
        if ui.checkbox(&mut restore, RichText::new("Reopen windows and tabs at launch").color(palette.text)).changed() {
            command = Some(Command::SetRestoreTabs(restore));
        }

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(8.0);

        let limits = config.search_cache;
        let mut chosen = limits;
        ui.horizontal(|ui| {
            ui.label(RichText::new("Search cache").color(palette.text));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_enabled_ui(limits.max_mb > 0, |ui| {
                    ComboBox::from_id_salt("cache_days").selected_text(days(limits.max_days)).show_ui(ui, |ui| {
                        for d in CACHE_DAYS {
                            ui.selectable_value(&mut chosen.max_days, d, days(d));
                        }
                    });
                });
                ComboBox::from_id_salt("cache_size").selected_text(size(limits.max_mb)).show_ui(ui, |ui| {
                    for mb in CACHE_SIZES {
                        ui.selectable_value(&mut chosen.max_mb, mb, size(mb));
                    }
                });
            });
        });
        ui.label(weak("Text of searched documents, kept on disk (not in memory) so searching them again is fast."));
        if chosen != limits {
            command = Some(Command::SetSearchCache(chosen));
        }
        command
    }

    /// The footprint, and what it is made of. The window buffers are an estimate and the
    /// rest is what the other rows leave, so the rows add up to the total.
    fn usage_rows(&self, ui: &mut Ui, palette: &Palette, status: &Status) {
        let row = |ui: &mut Ui, label: &str, value: String, main: bool| {
            ui.horizontal(|ui| {
                let color = if main { palette.text } else { palette.text_weak };
                let size = if main { 14.0 } else { 12.5 };
                if !main {
                    ui.add_space(12.0);
                }
                ui.label(RichText::new(label).color(color).size(size));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(RichText::new(value).color(color).size(size).monospace());
                });
            });
        };
        let Some(usage) = self.usage else {
            row(ui, "Memory in use", "—".to_string(), true);
            return;
        };
        let pages = status.texture_bytes as u64;
        let graphics = usage.graphics.saturating_sub(pages);
        let windows = status.window_buffers;
        let rest = usage.footprint.saturating_sub(windows + pages + graphics + usage.heap);
        row(ui, "Memory in use", mb(usage.footprint), true);
        row(ui, "Window buffers", mb(windows), false);
        row(ui, "Page images", format!("{} of {}", mb(pages), mb(status.texture_budget as u64)), false);
        row(ui, "Other graphics (fonts, buffers)", mb(graphics), false);
        row(ui, "Heap (parsed pages, text, …)", mb(usage.heap), false);
        row(ui, "System and other", mb(rest), false);
    }
}

fn mb(bytes: u64) -> String {
    format!("{:.0} MB", bytes as f64 / MB as f64)
}

fn size(mb: u64) -> String {
    match mb {
        0 => "Off".to_string(),
        mb if mb >= 1024 && mb % 1024 == 0 => format!("{} GB", mb / 1024),
        mb => format!("{mb} MB"),
    }
}

fn days(d: u64) -> String {
    format!("{d} days")
}
