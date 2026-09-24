//! The find bar: a field floating at the top right of the page canvas, with the match
//! count, previous and next buttons, and a close button.

use eframe::egui::{
    Align2, Context, CornerRadius, FontId, Frame, Id, Key, Label, Margin, Modifiers, Order, Rect, RichText, Sense,
    Stroke, TextEdit, Ui, pos2, text::CCursor, text::CCursorRange, vec2,
};

use crate::search::Search;
use crate::theme::Palette;

const INSET: f32 = 10.0;
const FIELD_WIDTH: f32 = 180.0;
const STATUS_WIDTH: f32 = 96.0;
const BUTTON: f32 = 22.0;

pub enum Action {
    /// The query was edited.
    Edited,
    Next,
    Previous,
    Close,
}

pub fn field_id(serial: u64) -> Id {
    Id::new(("find", serial))
}

/// Whether the find field has keyboard focus.
pub fn focused(ctx: &Context, serial: u64) -> bool {
    ctx.memory(|m| m.has_focus(field_id(serial)))
}

/// Draws the bar below `top` (the tab strip's height). Enter and ⇧Enter in the field step
/// through matches.
pub fn show(ctx: &Context, serial: u64, top: f32, search: &mut Search, palette: &Palette) -> Option<Action> {
    let id = field_id(serial);
    let mut action = None;
    // Taken before the field sees them: Enter would otherwise end editing.
    if focused(ctx, serial) {
        ctx.input_mut(|i| {
            if i.consume_key(Modifiers::SHIFT, Key::Enter) {
                action = Some(Action::Previous);
            } else if i.consume_key(Modifiers::NONE, Key::Enter) {
                action = Some(Action::Next);
            }
        });
    }
    let frame = Frame::new()
        .fill(palette.content)
        .stroke(Stroke::new(1.0, palette.separator))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(8, 4));
    eframe::egui::Area::new(Id::new(("find-bar", serial)))
        .order(Order::Foreground)
        .anchor(Align2::RIGHT_TOP, vec2(-INSET, top + INSET))
        .show(ctx, |ui| {
            frame.show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    let mut output = TextEdit::singleline(&mut search.query)
                        .id(id)
                        .hint_text("Find")
                        .font(FontId::proportional(13.0))
                        .text_color(palette.text)
                        .frame(false)
                        .desired_width(FIELD_WIDTH)
                        .show(ui);
                    if std::mem::take(&mut search.focus) {
                        output.response.request_focus();
                        let all = CCursorRange::two(CCursor::new(0), CCursor::new(search.query.chars().count()));
                        output.state.cursor.set_char_range(Some(all));
                        output.state.store(ui.ctx(), id);
                    }
                    if output.response.changed() {
                        action = Some(Action::Edited);
                    }
                    let status = search.status().unwrap_or_default();
                    ui.add_sized(
                        [STATUS_WIDTH, BUTTON],
                        Label::new(RichText::new(status).size(12.0).color(palette.text_weak)).truncate(),
                    );
                    if icon_button(ui, palette, Icon::Up).clicked() {
                        action = Some(Action::Previous);
                    }
                    if icon_button(ui, palette, Icon::Down).clicked() {
                        action = Some(Action::Next);
                    }
                    if icon_button(ui, palette, Icon::Close).clicked() {
                        action = Some(Action::Close);
                    }
                });
            });
        });
    action
}

enum Icon {
    Up,
    Down,
    Close,
}

fn icon_button(ui: &mut Ui, palette: &Palette, icon: Icon) -> eframe::egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(BUTTON, BUTTON), Sense::click());
    let painter = ui.painter();
    if response.hovered() {
        painter.rect_filled(rect, CornerRadius::same(5), palette.hover);
    }
    let stroke = Stroke::new(1.4, palette.text);
    let c = rect.center();
    let lines: &[[(f32, f32); 2]] = match icon {
        Icon::Up => &[[(-4.0, 2.0), (0.0, -2.0)], [(0.0, -2.0), (4.0, 2.0)]],
        Icon::Down => &[[(-4.0, -2.0), (0.0, 2.0)], [(0.0, 2.0), (4.0, -2.0)]],
        Icon::Close => &[[(-3.5, -3.5), (3.5, 3.5)], [(3.5, -3.5), (-3.5, 3.5)]],
    };
    for [a, b] in lines {
        painter.line_segment([c + vec2(a.0, a.1), c + vec2(b.0, b.1)], stroke);
    }
    response
}

/// A rect on a page, given as fractions of it, placed on the page's screen rect.
pub fn on_page(page: Rect, fraction: Rect) -> Rect {
    Rect::from_min_max(
        pos2(page.min.x + fraction.min.x * page.width(), page.min.y + fraction.min.y * page.height()),
        pos2(page.min.x + fraction.max.x * page.width(), page.min.y + fraction.max.y * page.height()),
    )
}
