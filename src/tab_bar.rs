//! Chrome-style tab strip drawn in the (transparent) macOS title bar.

use eframe::egui::{
    Align2, CornerRadius, CursorIcon, FontId, Rect, Sense, Stroke, Ui, ViewportCommand, pos2,
    text::LayoutJob, vec2,
};

use crate::tab::Tab;
use crate::theme::Palette;

pub const HEIGHT: f32 = 30.0;
/// Room for the traffic-light window buttons.
const LEADING: f32 = 78.0;
/// macOS draws the traffic lights centred ~16pt below the window's top edge; tabs are
/// centred on the same line so titles, close buttons and `+` align with them.
const TRAFFIC_LIGHT_CENTER_Y: f32 = 16.0;
const TAB_TOP: f32 = 2.0 * TRAFFIC_LIGHT_CENTER_Y - HEIGHT;
const MIN_TAB: f32 = 90.0;
const MAX_TAB: f32 = 220.0;
const STATUS_WIDTH: f32 = 130.0;

pub enum Action {
    Select(usize),
    Close(usize),
    Open,
}

pub fn show(ui: &mut Ui, tabs: &[Tab], active: usize, status: Option<String>, palette: &Palette) -> Option<Action> {
    let bar = ui.max_rect();
    let mut action = None;

    // Empty areas of the strip move the window, like a native title bar.
    let background = ui.interact(bar, ui.id().with("drag"), Sense::click_and_drag());
    if background.drag_started() {
        ui.ctx().send_viewport_cmd(ViewportCommand::StartDrag);
    }
    if background.double_clicked() {
        let maximized = ui.input(|i| i.viewport().maximized.unwrap_or(false));
        ui.ctx().send_viewport_cmd(ViewportCommand::Maximized(!maximized));
    }

    let painter = ui.painter();
    painter.rect_filled(bar, 0.0, palette.chrome);

    let tabs_left = bar.left() + LEADING;
    let tabs_right = bar.right() - STATUS_WIDTH - 36.0;
    let tab_width = if tabs.is_empty() {
        0.0
    } else {
        ((tabs_right - tabs_left) / tabs.len() as f32).clamp(MIN_TAB, MAX_TAB)
    };

    for (i, tab) in tabs.iter().enumerate() {
        let rect = Rect::from_min_size(
            pos2(tabs_left + i as f32 * tab_width, bar.top() + TAB_TOP),
            vec2(tab_width, bar.height() - TAB_TOP),
        );
        let response = ui.interact(rect, ui.id().with(("tab", tab.id)), Sense::click());
        let is_active = i == active;
        let painter = ui.painter();

        if is_active {
            let radius = CornerRadius { nw: 8, ne: 8, sw: 0, se: 0 };
            painter.rect_filled(rect, radius, palette.content);
        } else if response.hovered() {
            painter.rect_filled(rect.shrink2(vec2(2.0, 3.0)), CornerRadius::same(6), palette.hover);
        } else if i + 1 != active && i + 1 < tabs.len() {
            let x = rect.right();
            painter.line_segment(
                [pos2(x, rect.top() + 8.0), pos2(x, rect.bottom() - 8.0)],
                Stroke::new(1.0, palette.separator),
            );
        }

        let close = Rect::from_center_size(pos2(rect.right() - 16.0, rect.center().y), vec2(16.0, 16.0));
        let title_rect = Rect::from_min_max(pos2(rect.left() + 12.0, rect.top()), pos2(close.left() - 4.0, rect.bottom()));
        let mut job = LayoutJob::simple_singleline(
            tab.title.clone(),
            FontId::proportional(12.5),
            if is_active { palette.text } else { palette.text_weak },
        );
        job.wrap.max_width = title_rect.width();
        job.wrap.max_rows = 1;
        job.wrap.break_anywhere = true;
        let galley = painter.layout_job(job);
        painter.galley(pos2(title_rect.left(), rect.center().y - galley.size().y / 2.0), galley, palette.text);

        let close_response = ui.interact(close, ui.id().with(("close", tab.id)), Sense::click());
        if is_active || response.hovered() || close_response.hovered() {
            let painter = ui.painter();
            if close_response.hovered() {
                painter.rect_filled(close, CornerRadius::same(4), palette.separator);
            }
            let c = close.center();
            let d = 3.5;
            let stroke = Stroke::new(1.3, palette.text_weak);
            painter.line_segment([c + vec2(-d, -d), c + vec2(d, d)], stroke);
            painter.line_segment([c + vec2(d, -d), c + vec2(-d, d)], stroke);
        }

        if close_response.clicked() || response.middle_clicked() {
            action = Some(Action::Close(i));
        } else if response.clicked() {
            action = Some(Action::Select(i));
        }
    }

    let plus_center = pos2(tabs_left + tabs.len() as f32 * tab_width + 18.0, bar.top() + TAB_TOP + (bar.height() - TAB_TOP) / 2.0);
    let plus = Rect::from_center_size(plus_center, vec2(24.0, 24.0));
    let plus_response = ui.interact(plus, ui.id().with("open"), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
    let painter = ui.painter();
    if plus_response.hovered() {
        painter.rect_filled(plus, CornerRadius::same(6), palette.hover);
    }
    let stroke = Stroke::new(1.4, palette.text_weak);
    painter.line_segment([plus_center - vec2(5.0, 0.0), plus_center + vec2(5.0, 0.0)], stroke);
    painter.line_segment([plus_center - vec2(0.0, 5.0), plus_center + vec2(0.0, 5.0)], stroke);
    if plus_response.clicked() {
        action = Some(Action::Open);
    }

    if let Some(status) = status {
        painter.text(
            pos2(bar.right() - 14.0, plus_center.y),
            Align2::RIGHT_CENTER,
            status,
            FontId::proportional(12.0),
            palette.text_weak,
        );
    }

    action
}
