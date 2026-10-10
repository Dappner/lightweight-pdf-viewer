//! Annotate mode (⌘⇧A): a slim bar over the pages with tools for marking them up. The
//! highlight tool, chosen on entering, highlights the text a drag, double or triple click
//! selects; the select tool selects text to copy, as outside the mode. With either,
//! double-clicking the page away from its text adds a text box. The bar also picks the
//! colour of new highlights and the size and colour of new text boxes (or of the one
//! selected), which the config keeps.

use eframe::egui::{
    Align, Color32, ComboBox, CornerRadius, CursorIcon, Frame, Layout, Margin, Rect, Response, RichText, Sense, Shape,
    Stroke, Ui, vec2,
};
use serde::{Deserialize, Serialize};

use crate::text_boxes;
use crate::theme::Palette;

pub const HEIGHT: f32 = 32.0;
/// Preview's highlight colours.
pub const HIGHLIGHT_COLORS: [(&str, [u8; 3]); 5] = [
    ("Yellow", text_boxes::HIGHLIGHT_YELLOW),
    ("Green", [110, 214, 96]),
    ("Blue", [96, 170, 255]),
    ("Pink", [255, 110, 170]),
    ("Purple", [186, 124, 255]),
];
const SWATCH: f32 = 16.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tool {
    /// Selects text to copy.
    Select,
    Highlight,
}

/// What new highlights and text boxes look like.
#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Style {
    pub highlight: [u8; 3],
    pub text_size: f32,
    pub text_color: [u8; 3],
}

impl Default for Style {
    fn default() -> Self {
        Self { highlight: text_boxes::HIGHLIGHT_YELLOW, text_size: text_boxes::DEFAULT_SIZE, text_color: [0, 0, 0] }
    }
}

pub enum Action {
    Tool(Tool),
    Style(Style),
    Done,
}

/// Draws the bar.
pub fn show(ui: &mut Ui, tool: Tool, style: &Style, palette: &Palette) -> Option<Action> {
    let mut action = None;
    let rect = ui.max_rect();
    ui.painter().rect_filled(rect, 0.0, palette.content);
    ui.painter().hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, palette.separator));
    let frame = Frame::NONE.inner_margin(Margin::symmetric(10, 0));
    frame.show(ui, |ui| {
        ui.set_height(HEIGHT);
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            for (t, hint) in [
                (Tool::Select, "Select: select text to copy"),
                (Tool::Highlight, "Highlight: drag over text to highlight it"),
            ] {
                if tool_button(ui, t, tool == t, style.highlight, palette).on_hover_text(hint).clicked() {
                    action = Some(Action::Tool(t));
                }
            }
            divider(ui, palette);
            for (name, color) in HIGHLIGHT_COLORS {
                if swatch(ui, color, style.highlight == color, palette).on_hover_text(name).clicked() {
                    action = Some(Action::Style(Style { highlight: color, ..*style }));
                }
            }
            divider(ui, palette);
            ui.label(RichText::new("Text").color(palette.text_weak))
                .on_hover_text("Double-click the page away from its text to add a text box");
            let mut size = style.text_size;
            ComboBox::from_id_salt("annotate_text_size").width(64.0).selected_text(format!("{size} pt")).show_ui(
                ui,
                |ui| {
                    for s in text_boxes::SIZES {
                        ui.selectable_value(&mut size, s, format!("{s} pt"));
                    }
                },
            );
            if size != style.text_size {
                action = Some(Action::Style(Style { text_size: size, ..*style }));
            }
            for (name, color) in text_boxes::COLORS {
                if swatch(ui, color, style.text_color == color, palette).on_hover_text(name).clicked() {
                    action = Some(Action::Style(Style { text_color: color, ..*style }));
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("Done").on_hover_text("Leave annotate mode (⌘⇧A)").clicked() {
                    action = Some(Action::Done);
                }
            });
        });
    });
    action
}

/// A tool's icon button, shaded while it's the tool in use: an arrow pointer for selecting,
/// a highlighter over a stroke of `highlight` for highlighting.
fn tool_button(ui: &mut Ui, tool: Tool, chosen: bool, highlight: [u8; 3], palette: &Palette) -> Response {
    let (rect, response) = ui.allocate_exact_size(vec2(28.0, 22.0), Sense::click());
    let response = response.on_hover_cursor(CursorIcon::PointingHand);
    let painter = ui.painter();
    if chosen || response.hovered() {
        painter.rect_filled(rect, CornerRadius::same(5), if chosen { palette.separator } else { palette.hover });
    }
    let color = if chosen { palette.text } else { palette.text_weak };
    let stroke = Stroke::new(1.3, color);
    let c = rect.center();
    match tool {
        Tool::Select => {
            let arrow = [(-3.5, -7.0), (-3.5, 5.0), (-0.5, 2.2), (1.8, 7.0), (3.6, 6.2), (1.3, 1.5), (5.0, 1.5)];
            painter.add(Shape::closed_line(arrow.iter().map(|&(x, y)| c + vec2(x, y)).collect(), stroke));
        }
        Tool::Highlight => {
            // Slanting up to the right, its chisel tip at the bottom left.
            let along = vec2(1.0, -1.0).normalized();
            let across = vec2(1.0, 1.0).normalized();
            let mid = c + vec2(1.5, -2.5);
            let at = |a: f32, b: f32| mid + along * a + across * b;
            let body = vec![at(4.5, -2.6), at(4.5, 2.6), at(-2.5, 2.6), at(-2.5, -2.6)];
            let tip = vec![at(-2.5, 2.6), at(-5.5, 1.2), at(-5.5, -1.2), at(-2.5, -2.6)];
            painter.add(Shape::closed_line(body, stroke));
            painter.add(Shape::line(tip, stroke));
            let [r, g, b] = highlight;
            let mark = Rect::from_center_size(c + vec2(0.0, 7.5), vec2(14.0, 2.5));
            painter.rect_filled(mark, CornerRadius::same(1), Color32::from_rgb(r, g, b));
        }
    }
    response
}

/// A round colour button, ringed when `chosen`.
fn swatch(ui: &mut Ui, color: [u8; 3], chosen: bool, palette: &Palette) -> Response {
    let (rect, response) = ui.allocate_exact_size(vec2(SWATCH, SWATCH), Sense::click());
    let response = response.on_hover_cursor(CursorIcon::PointingHand);
    let [r, g, b] = color;
    let painter = ui.painter();
    let center = rect.center();
    if chosen {
        painter.circle_stroke(center, SWATCH / 2.0, Stroke::new(1.5, palette.accent));
    }
    painter.circle_filled(center, SWATCH / 2.0 - 3.0, Color32::from_rgb(r, g, b));
    painter.circle_stroke(center, SWATCH / 2.0 - 3.0, Stroke::new(0.5, palette.separator));
    if response.hovered() && !chosen {
        painter.circle_stroke(center, SWATCH / 2.0, Stroke::new(1.0, palette.text_weak));
    }
    response
}

fn divider(ui: &mut Ui, palette: &Palette) {
    ui.add_space(4.0);
    let (rect, _) = ui.allocate_exact_size(vec2(1.0, 16.0), Sense::hover());
    ui.painter().vline(rect.center().x, rect.y_range(), Stroke::new(1.0, palette.separator));
    ui.add_space(4.0);
}
