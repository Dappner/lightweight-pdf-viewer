//! Chrome-style tab strip drawn in the (transparent) macOS title bar.
//!
//! Tabs shrink to [`MIN_TAB`] and then scroll horizontally (trackpad or wheel over the strip).
//! Dragging a tab reorders it; dragging it well below the strip, or out of the window,
//! detaches it: released over another window's strip it joins that window, and anywhere
//! else it gets a window of its own (a window's only tab just moves its window there).
//! Right-clicking a tab shows its file in Finder, copies its path, reverts its unsaved edits
//! or closes tabs. A tab with unsaved text boxes shows a dot in place of its close button until hovered.
//! Left of the tabs, a Preview-style button opens a menu of sidebar and page layout options;
//! at the right end, after the page and zoom status, buttons show the annotate bar and
//! rotate the pages.

use eframe::egui::{
    Align2, Button, CornerRadius, CursorIcon, FontId, LayerId, Order, Painter, Popup, Pos2, Rect, Response, Sense,
    Stroke, Ui, Vec2, ViewportCommand, pos2, text::LayoutJob, vec2,
};

use crate::app::Command;
use crate::layout::PageLayout;
use crate::renderer::DocId;
use crate::sidebar::Mode as SidebarMode;
use crate::tab::Tab;
use crate::theme::Palette;

pub const HEIGHT: f32 = 30.0;
/// Room for the traffic-light window buttons.
const LEADING: f32 = 78.0;
/// The view options button between the traffic lights and the tabs.
const VIEW_BUTTON: f32 = 44.0;
/// macOS draws the traffic lights centred ~16pt below the window's top edge; tabs are
/// centred on the same line so titles, close buttons and `+` align with them.
const TRAFFIC_LIGHT_CENTER_Y: f32 = 16.0;
const TAB_TOP: f32 = 2.0 * TRAFFIC_LIGHT_CENTER_Y - HEIGHT;
const MIN_TAB: f32 = 110.0;
const MAX_TAB: f32 = 220.0;
const PLUS_WIDTH: f32 = 36.0;
const STATUS_WIDTH: f32 = 150.0;
/// The annotate and rotate buttons at the right end of the strip.
const BUTTONS_WIDTH: f32 = 60.0;
/// How far below the strip a dragged tab must go to detach.
const DETACH_DISTANCE: f32 = 60.0;
/// Where a detached tab's new window goes relative to the pointer: the pointer ends up
/// over the start of the new window's first tab.
pub const NEW_WINDOW_GRAB: Vec2 = vec2(LEADING + VIEW_BUTTON + 40.0, TRAFFIC_LIGHT_CENTER_Y);

pub enum Action {
    Select(usize),
    Close(usize),
    /// Closes every tab but this one.
    CloseOthers(usize),
    /// Shows the tab's file in Finder.
    Reveal(usize),
    /// Drops the tab's edits not yet saved.
    Revert(usize),
    Move {
        from: usize,
        to: usize,
    },
    /// A tab was dragged out and dropped at `pointer` (window coordinates).
    Detach {
        index: usize,
        pointer: Pos2,
    },
    Open,
    Run(Command),
}

/// What the view options menu shows as checked.
pub struct ViewMenu {
    /// `None` while the sidebar is hidden.
    pub sidebar: Option<SidebarMode>,
    /// `None` until the active tab has loaded.
    pub layout: Option<PageLayout>,
    /// The annotate bar is showing, for its button.
    pub annotating: bool,
}

/// `scroll` is the strip's horizontal scroll offset, kept by the caller between frames.
pub fn show(
    ui: &mut Ui,
    tabs: &[Tab],
    active: usize,
    scroll: &mut f32,
    status: Option<String>,
    view_menu: ViewMenu,
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

    let n = tabs.len();
    let (area, tab_width) = strip(bar, n);
    let total = tab_width * n as f32;
    let max_scroll = (total - area.width()).max(0.0);

    if let Some(pointer) = ui.input(|i| i.pointer.hover_pos())
        && area.contains(pointer)
    {
        let delta = ui.input(|i| i.smooth_scroll_delta);
        *scroll -= delta.x + delta.y;
    }
    let active_id = tabs.get(active).map(|t| t.id);
    let last_active_key = ui.id().with("last_active");
    if ui.data(|d| d.get_temp::<Option<DocId>>(last_active_key)) != Some(active_id) {
        // Bring a newly active tab fully into view.
        let left = active as f32 * tab_width;
        *scroll = scroll.max(left + tab_width - area.width()).min(left);
        ui.data_mut(|d| d.insert_temp(last_active_key, active_id));
    }
    *scroll = scroll.clamp(0.0, max_scroll);
    let scroll = *scroll;

    let slot = |i: usize| {
        Rect::from_min_size(
            pos2(area.left() - scroll + i as f32 * tab_width, area.top()),
            vec2(tab_width, area.height()),
        )
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
        .map(|(i, tab)| {
            ui.interact(close_rect(slot(i)).intersect(area), ui.id().with(("close", tab.id)), Sense::click())
        })
        .collect();

    let grab_key = ui.id().with("grab");
    // The last known position: it goes stale rather than missing once the pointer leaves
    // the window mid-drag.
    let pointer = ui.input(|i| i.pointer.latest_pos());
    let pointer_x = pointer.map(|p| p.x);
    let window = ui.ctx().content_rect();
    let detaching = |p: Pos2| p.y > bar.bottom() + DETACH_DISTANCE || !window.contains(p);
    let mut dragged = None;
    let mut detached = None;
    for (i, response) in responses.iter().enumerate() {
        if response.drag_stopped()
            && let Some(p) = pointer.filter(|p| detaching(*p))
        {
            action = Some(Action::Detach { index: i, pointer: p });
            continue;
        }
        if response.dragged()
            && let Some(p) = pointer.filter(|p| detaching(*p))
        {
            detached = Some((i, p));
            continue;
        }
        if response.drag_started()
            && let Some(x) = pointer_x
        {
            let pressed_x = ui.input(|input| input.pointer.press_origin()).map_or(x, |p| p.x);
            ui.data_mut(|d| d.insert_temp(grab_key, pressed_x - slot(i).left()));
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

    let button =
        Rect::from_center_size(pos2(bar.left() + LEADING + VIEW_BUTTON / 2.0 - 4.0, area.center().y), vec2(36.0, 24.0));
    if let Some(command) = view_button(ui, button, &view_menu, palette) {
        action = Some(Action::Run(command));
    }

    let painter = ui.painter_at(area);
    for (i, tab) in tabs.iter().enumerate() {
        if dragged.is_some_and(|(d, _)| d == i) || detached.is_some_and(|(d, _)| d == i) {
            continue;
        }
        if !slot(i).intersect(area).is_positive() {
            continue;
        }
        let hovered = responses[i].hovered() || closes[i].hovered();
        let separator = i != active && !hovered && i + 1 != active && i + 1 < n;
        paint_tab(&painter, slot(i), tab, i == active, hovered, closes[i].hovered(), separator, palette);
    }
    if let Some((i, rect)) = dragged {
        paint_tab(&painter, rect, &tabs[i], true, true, false, false, palette);
    }
    if let Some((i, p)) = detached {
        paint_detached(ui, &tabs[i], p, tab_width, palette);
    }

    for (i, (response, close)) in responses.iter().zip(&closes).enumerate() {
        if close.clicked() || response.middle_clicked() {
            action = Some(Action::Close(i));
        } else if response.clicked() {
            action = Some(Action::Select(i));
        }
        response.context_menu(|ui| {
            if ui.button("Show in Finder").clicked() {
                action = Some(Action::Reveal(i));
            }
            if ui.button("Copy Path").clicked() {
                ui.ctx().copy_text(tabs[i].path.display().to_string());
            }
            if ui.add_enabled(tabs[i].text_boxes.unsaved(), Button::new("Revert to Saved")).clicked() {
                action = Some(Action::Revert(i));
            }
            ui.separator();
            if ui.button("Close Tab").clicked() {
                action = Some(Action::Close(i));
            }
            if ui.add_enabled(tabs.len() > 1, Button::new("Close Other Tabs")).clicked() {
                action = Some(Action::CloseOthers(i));
            }
        });
    }

    let plus_center = pos2((area.left() - scroll + total).min(area.right()) + PLUS_WIDTH / 2.0, area.center().y);
    let plus = Rect::from_center_size(plus_center, vec2(24.0, 24.0));
    let plus_response =
        ui.interact(plus, ui.id().with("open"), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
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
        let rotate = Rect::from_center_size(pos2(bar.right() - 8.0 - 12.0, plus_center.y), vec2(24.0, 24.0));
        if let Some(command) = rotate_button(ui, rotate, palette) {
            action = Some(Action::Run(command));
        }
        let annotate = rotate.translate(vec2(-28.0, 0.0));
        if annotate_button(ui, annotate, view_menu.annotating, palette) {
            action = Some(Action::Run(Command::ToggleAnnotate));
        }
        let painter = ui.painter();
        painter.text(
            pos2(annotate.left() - 6.0, plus_center.y),
            Align2::RIGHT_CENTER,
            status,
            FontId::proportional(12.0),
            palette.text_weak,
        );
    }

    action
}

/// The tab area of a strip in `bar` holding `n` tabs, and the width of each tab.
fn strip(bar: Rect, n: usize) -> (Rect, f32) {
    let leading = LEADING + VIEW_BUTTON;
    let area = Rect::from_min_max(
        pos2(bar.left() + leading, bar.top() + TAB_TOP),
        pos2((bar.right() - STATUS_WIDTH - BUTTONS_WIDTH - PLUS_WIDTH).max(bar.left() + leading), bar.bottom()),
    );
    let tab_width = if n == 0 { 0.0 } else { (area.width() / n as f32).clamp(MIN_TAB, MAX_TAB) };
    (area, tab_width)
}

/// Whether `pointer` (window coordinates) is over the strip of a window `width` wide.
pub fn contains(width: f32, pointer: Pos2) -> bool {
    pointer.x >= 0.0 && pointer.x <= width && pointer.y >= 0.0 && pointer.y <= HEIGHT
}

/// Where a tab dropped at `x` (window coordinates) goes among the `n` tabs of a strip in a
/// window `width` wide, scrolled by `scroll`.
pub fn drop_index(width: f32, n: usize, scroll: f32, x: f32) -> usize {
    let (area, tab_width) = strip(Rect::from_min_size(Pos2::ZERO, vec2(width, HEIGHT)), n.max(1));
    (((x - area.left() + scroll) / tab_width).round().max(0.0) as usize).min(n)
}

/// A dragged-out tab under the pointer, badged with a plus: releasing opens a new window.
fn paint_detached(ui: &Ui, tab: &Tab, pointer: Pos2, tab_width: f32, palette: &Palette) {
    let painter = ui.ctx().layer_painter(LayerId::new(Order::Tooltip, ui.id().with("detached")));
    let rect = Rect::from_min_size(pointer - vec2(24.0, 12.0), vec2(tab_width.min(MAX_TAB), 26.0));
    painter.rect_filled(rect.translate(vec2(0.0, 2.0)), CornerRadius::same(7), palette.separator);
    painter.rect(
        rect,
        CornerRadius::same(7),
        palette.content,
        Stroke::new(1.0, palette.separator),
        eframe::egui::StrokeKind::Inside,
    );

    let badge = pos2(rect.left() + 13.0, rect.center().y);
    painter.circle_filled(badge, 7.0, palette.accent);
    let stroke = Stroke::new(1.5, eframe::egui::Color32::WHITE);
    painter.line_segment([badge - vec2(3.5, 0.0), badge + vec2(3.5, 0.0)], stroke);
    painter.line_segment([badge - vec2(0.0, 3.5), badge + vec2(0.0, 3.5)], stroke);

    let mut job = LayoutJob::simple_singleline(tab.title.clone(), FontId::proportional(12.5), palette.text);
    job.wrap.max_width = rect.right() - 8.0 - (badge.x + 12.0);
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    let galley = painter.layout_job(job);
    painter.galley(pos2(badge.x + 12.0, rect.center().y - galley.size().y / 2.0), galley, palette.text);
    ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
}

/// A sidebar icon with a chevron, like Preview's, that opens the view options.
fn view_button(ui: &mut Ui, rect: Rect, menu: &ViewMenu, palette: &Palette) -> Option<Command> {
    let response =
        ui.interact(rect, ui.id().with("view_menu"), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
    let open = Popup::is_id_open(ui.ctx(), Popup::default_response_id(&response));
    let painter = ui.painter();
    if response.hovered() || open {
        painter.rect_filled(rect, CornerRadius::same(6), palette.hover);
    }
    let stroke = Stroke::new(1.3, palette.text_weak);
    let icon = Rect::from_center_size(rect.center() - vec2(5.0, 0.0), vec2(15.0, 12.0));
    painter.rect_stroke(icon, CornerRadius::same(2), stroke, eframe::egui::StrokeKind::Inside);
    painter.line_segment([pos2(icon.left() + 5.0, icon.top()), pos2(icon.left() + 5.0, icon.bottom())], stroke);
    let chevron = pos2(icon.right() + 7.0, rect.center().y);
    painter.line_segment([chevron + vec2(-3.0, -1.5), chevron + vec2(0.0, 1.5)], stroke);
    painter.line_segment([chevron + vec2(0.0, 1.5), chevron + vec2(3.0, -1.5)], stroke);

    let mut chosen = None;
    Popup::menu(&response).width(170.0).show(|ui| {
        let mut item = |ui: &mut Ui, label: &str, checked: bool, enabled: bool, command: Command| {
            if ui.add_enabled(enabled, Button::selectable(checked, label)).clicked() {
                chosen = Some(command);
            }
        };
        let sidebar = menu.sidebar;
        let hide = if sidebar.is_some() { "Hide Sidebar" } else { "Show Sidebar" };
        item(ui, hide, false, true, Command::ToggleSidebar);
        item(ui, "Pages", sidebar == Some(SidebarMode::Pages), true, Command::ShowSidebar(SidebarMode::Pages));
        item(ui, "Outline", sidebar == Some(SidebarMode::Outline), true, Command::ShowSidebar(SidebarMode::Outline));
        ui.separator();
        for (label, layout) in
            [("Single Page", PageLayout::Single), ("Two Pages", PageLayout::TwoPages), ("Book", PageLayout::Book)]
        {
            let loaded = menu.layout.is_some();
            item(ui, label, menu.layout == Some(layout), loaded, Command::SetLayout(layout));
        }
    });
    chosen
}

/// A pen that shows or hides the annotate bar, shaded while it's showing.
fn annotate_button(ui: &mut Ui, rect: Rect, on: bool, palette: &Palette) -> bool {
    let response = ui
        .interact(rect, ui.id().with("annotate"), Sense::click())
        .on_hover_cursor(CursorIcon::PointingHand)
        .on_hover_text(if on { "Hide Annotate Bar (⌘⇧A)" } else { "Annotate (⌘⇧A)" });
    let painter = ui.painter();
    if on || response.hovered() {
        painter.rect_filled(rect, CornerRadius::same(6), if on { palette.separator } else { palette.hover });
    }
    // A pen slanting up to the right, its tip at the bottom left.
    let stroke = Stroke::new(1.3, if on { palette.text } else { palette.text_weak });
    let c = rect.center();
    let (tip, base, end) = (c + vec2(-5.5, 5.5), c + vec2(-3.0, 3.0), c + vec2(4.5, -4.5));
    let side = vec2(1.6, 1.6);
    for [a, b] in [
        [base + side, end + side],
        [base - side, end - side],
        [end + side, end - side],
        [base + side, tip],
        [base - side, tip],
    ] {
        painter.line_segment([a, b], stroke);
    }
    response.clicked()
}

/// Rotates left, as Preview's toolbar button does; with ⌥ held, right.
fn rotate_button(ui: &mut Ui, rect: Rect, palette: &Palette) -> Option<Command> {
    let response = ui
        .interact(rect, ui.id().with("rotate"), Sense::click())
        .on_hover_cursor(CursorIcon::PointingHand)
        .on_hover_text("Rotate Left (⌘L), ⌥-click to Rotate Right (⌘R)");
    let painter = ui.painter();
    if response.hovered() {
        painter.rect_filled(rect, CornerRadius::same(6), palette.hover);
    }
    // A counterclockwise arrow: an arc from the top round the right to the left, with its
    // head at the top pointing left.
    let stroke = Stroke::new(1.3, palette.text_weak);
    let (center, radius) = (rect.center() + vec2(0.0, 0.5), 5.5);
    let at = |degrees: f32| {
        let a = degrees.to_radians();
        center + vec2(a.cos(), -a.sin()) * radius
    };
    let arc: Vec<Pos2> = (0..=24).map(|i| at(90.0 - i as f32 * 270.0 / 24.0)).collect();
    painter.add(eframe::egui::Shape::line(arc, stroke));
    let head = at(90.0);
    painter.line_segment([head, head + vec2(3.0, -3.0)], stroke);
    painter.line_segment([head, head + vec2(3.0, 3.0)], stroke);
    response.clicked().then(|| if ui.input(|i| i.modifiers.alt) { Command::RotateRight } else { Command::RotateLeft })
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

    // Unsaved text boxes: a dot where the close button is, which hovering turns back into it.
    if tab.text_boxes.unsaved() && !hovered {
        painter.circle_filled(close.center(), 3.5, palette.text_weak);
    } else if is_active || hovered {
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
