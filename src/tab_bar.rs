//! Chrome-style tab strip drawn in the (transparent) macOS title bar.
//!
//! Tabs shrink to [`MIN_TAB`] and then scroll horizontally (trackpad or wheel over the strip).
//! Dragging a tab reorders it.

use eframe::egui::{
    Align2, CornerRadius, CursorIcon, FontId, Id, Painter, Rect, Response, Sense, Stroke, Ui,
    ViewportCommand, pos2, text::LayoutJob, vec2,
};

use crate::renderer::DocId;
use crate::tab::Tab;
use crate::theme::Palette;

pub const HEIGHT: f32 = 30.0;
/// Room for the traffic-light window buttons.
const LEADING: f32 = 78.0;
/// macOS draws the traffic lights centred ~16pt below the window's top edge; tabs are
/// centred on the same line so titles, close buttons and `+` align with them.
const TRAFFIC_LIGHT_CENTER_Y: f32 = 16.0;
const TAB_TOP: f32 = 2.0 * TRAFFIC_LIGHT_CENTER_Y - HEIGHT;
const MIN_TAB: f32 = 110.0;
const MAX_TAB: f32 = 220.0;
const PLUS_WIDTH: f32 = 36.0;
const STATUS_WIDTH: f32 = 130.0;

pub enum Action {
    Select(usize),
    Close(usize),
    Move { from: usize, to: usize },
    Open,
}

/// `scroll` is the strip's horizontal scroll offset, kept by the caller between frames.
pub fn show(
    ui: &mut Ui,
    tabs: &[Tab],
    active: usize,
    scroll: &mut f32,
    status: Option<String>,
    palette: &Palette,
) -> Option<Action> {
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
    ui.painter().rect_filled(bar, 0.0, palette.chrome);

    let area = Rect::from_min_max(
        pos2(bar.left() + LEADING, bar.top() + TAB_TOP),
        pos2((bar.right() - STATUS_WIDTH - PLUS_WIDTH).max(bar.left() + LEADING), bar.bottom()),
    );
    let n = tabs.len();
    let tab_width = if n == 0 { 0.0 } else { (area.width() / n as f32).clamp(MIN_TAB, MAX_TAB) };
    let total = tab_width * n as f32;
    let max_scroll = (total - area.width()).max(0.0);

    if let Some(pointer) = ui.input(|i| i.pointer.hover_pos())
        && area.contains(pointer)
    {
        let delta = ui.input(|i| i.smooth_scroll_delta);
        *scroll -= delta.x + delta.y;
    }
    let active_id = tabs.get(active).map(|t| t.id);
    let last_active_key = Id::new("tab_bar_last_active");
    if ui.data(|d| d.get_temp::<Option<DocId>>(last_active_key)) != Some(active_id) {
        // Bring a newly active tab fully into view.
        let left = active as f32 * tab_width;
        *scroll = scroll.max(left + tab_width - area.width()).min(left);
        ui.data_mut(|d| d.insert_temp(last_active_key, active_id));
    }
    *scroll = scroll.clamp(0.0, max_scroll);
    let scroll = *scroll;

    let slot = |i: usize| {
        Rect::from_min_size(pos2(area.left() - scroll + i as f32 * tab_width, area.top()), vec2(tab_width, area.height()))
    };

    // Register all interactions before painting; close buttons last so they win hit-tests.
    let responses: Vec<Response> = tabs
        .iter()
        .enumerate()
        .map(|(i, tab)| ui.interact(slot(i).intersect(area), ui.id().with(("tab", tab.id)), Sense::click_and_drag()))
        .collect();
    let closes: Vec<Response> = tabs
        .iter()
        .enumerate()
        .map(|(i, tab)| ui.interact(close_rect(slot(i)).intersect(area), ui.id().with(("close", tab.id)), Sense::click()))
        .collect();

    let grab_key = Id::new("tab_bar_grab");
    let pointer_x = ui.input(|i| i.pointer.interact_pos()).map(|p| p.x);
    let mut dragged = None;
    for (i, response) in responses.iter().enumerate() {
        if response.drag_started()
            && let Some(x) = pointer_x
        {
            ui.data_mut(|d| d.insert_temp(grab_key, x - slot(i).left()));
            // Like browsers, dragging a tab also selects it.
            if i != active {
                action = Some(Action::Select(i));
            }
        }
        if response.dragged()
            && let Some(x) = pointer_x
        {
            let grab = ui.data(|d| d.get_temp::<f32>(grab_key)).unwrap_or(tab_width / 2.0);
            let left = (x - grab).clamp(area.left(), (area.right() - tab_width).max(area.left()));
            let target = (((left + tab_width / 2.0 - (area.left() - scroll)) / tab_width).floor() as usize).min(n - 1);
            if target != i {
                action = Some(Action::Move { from: i, to: target });
            }
            dragged = Some((i, Rect::from_min_size(pos2(left, area.top()), vec2(tab_width, area.height()))));
        }
    }

    let painter = ui.painter_at(area);
    for (i, tab) in tabs.iter().enumerate() {
        if dragged.is_some_and(|(d, _)| d == i) {
            continue;
        }
        let hovered = responses[i].hovered() || closes[i].hovered();
        let separator = i != active && !hovered && i + 1 != active && i + 1 < n;
        paint_tab(&painter, slot(i), tab, i == active, hovered, closes[i].hovered(), separator, palette);
    }
    if let Some((i, rect)) = dragged {
        paint_tab(&painter, rect, &tabs[i], true, true, false, false, palette);
    }

    for (i, (response, close)) in responses.iter().zip(&closes).enumerate() {
        if close.clicked() || response.middle_clicked() {
            action = Some(Action::Close(i));
        } else if response.clicked() {
            action = Some(Action::Select(i));
        }
    }

    let plus_center = pos2((area.left() - scroll + total).min(area.right()) + PLUS_WIDTH / 2.0, area.center().y);
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

fn close_rect(tab: Rect) -> Rect {
    Rect::from_center_size(pos2(tab.right() - 16.0, tab.center().y), vec2(16.0, 16.0))
}

#[allow(clippy::too_many_arguments)]
fn paint_tab(
    painter: &Painter,
    rect: Rect,
    tab: &Tab,
    is_active: bool,
    hovered: bool,
    close_hovered: bool,
    separator: bool,
    palette: &Palette,
) {
    if is_active {
        let radius = CornerRadius { nw: 8, ne: 8, sw: 0, se: 0 };
        painter.rect_filled(rect, radius, palette.content);
    } else if hovered {
        painter.rect_filled(rect.shrink2(vec2(2.0, 3.0)), CornerRadius::same(6), palette.hover);
    }
    if separator {
        let x = rect.right();
        painter.line_segment(
            [pos2(x, rect.top() + 8.0), pos2(x, rect.bottom() - 8.0)],
            Stroke::new(1.0, palette.separator),
        );
    }

    let close = close_rect(rect);
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

    if is_active || hovered {
        if close_hovered {
            painter.rect_filled(close, CornerRadius::same(4), palette.separator);
        }
        let c = close.center();
        let d = 3.5;
        let stroke = Stroke::new(1.3, palette.text_weak);
        painter.line_segment([c + vec2(-d, -d), c + vec2(d, d)], stroke);
        painter.line_segment([c + vec2(d, -d), c + vec2(-d, d)], stroke);
    }
}
