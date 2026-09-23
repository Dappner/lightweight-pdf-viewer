//! Sidebar (toggled with ⌘B): page thumbnails or the document outline.

use std::collections::HashSet;

use eframe::egui::{
    Align2, Color32, CornerRadius, CursorIcon, FontId, Rect, ScrollArea, Sense, Stroke,
    StrokeKind, Ui, pos2, text::LayoutJob, vec2,
};

use crate::cache::TextureCache;
use crate::layout::DocLayout;
use crate::renderer::{DocId, Key, OutlineEntry, Target};
use crate::tab::View;
use crate::theme::Palette;
use crate::view::Wants;

pub const WIDTH: f32 = 160.0;
const THUMB_WIDTH: f32 = 108.0;
const LABEL_HEIGHT: f32 = 22.0;
const SPACING: f32 = 12.0;
const PADDING: f32 = 12.0;
const HEADER: f32 = 36.0;
const OUTLINE_ROW: f32 = 24.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Pages,
    Outline,
}

pub enum Action {
    GoTo(Target),
    SetMode(Mode),
}

pub fn thumb_width_px(pixels_per_point: f32) -> u32 {
    (THUMB_WIDTH * pixels_per_point).round() as u32
}

fn thumb_height(size: eframe::egui::Vec2) -> f32 {
    (THUMB_WIDTH * size.y / size.x).min(THUMB_WIDTH * 2.0)
}

#[allow(clippy::too_many_arguments)]
pub fn show(
    ui: &mut Ui,
    mode: Mode,
    doc: DocId,
    layout: &DocLayout,
    view: &mut View,
    outline: Option<&[OutlineEntry]>,
    cache: &mut TextureCache,
    failed: &HashSet<Key>,
    wants: &mut Wants,
    palette: &Palette,
) -> Option<Action> {
    let switched = mode_switch(ui, mode, palette).map(Action::SetMode);
    let clicked = match mode {
        Mode::Pages => thumbnails(ui, doc, layout, view, cache, failed, wants, palette)
            .map(|page| Target::Page { page: page as u32, y: None }),
        Mode::Outline => outline_list(ui, doc, outline, view.current_page, palette),
    };
    switched.or(clicked.map(Action::GoTo))
}

/// "Pages | Outline" segmented control.
fn mode_switch(ui: &mut Ui, mode: Mode, palette: &Palette) -> Option<Mode> {
    let (header, _) = ui.allocate_exact_size(vec2(ui.available_width(), HEADER), Sense::hover());
    let track = Rect::from_center_size(header.center(), vec2(136.0, 24.0));
    ui.painter().rect_filled(track, CornerRadius::same(7), palette.chrome);
    let mut chosen = None;
    for (i, (label, option)) in [("Pages", Mode::Pages), ("Outline", Mode::Outline)].into_iter().enumerate() {
        let half = Rect::from_min_size(track.min + vec2(i as f32 * track.width() / 2.0, 0.0), vec2(track.width() / 2.0, track.height()));
        let response = ui.interact(half, ui.id().with(("sidebar_mode", i)), Sense::click());
        let selected = option == mode;
        if selected {
            ui.painter().rect_filled(half.shrink(2.0), CornerRadius::same(5), palette.separator);
        }
        let color = if selected || response.hovered() { palette.text } else { palette.text_weak };
        ui.painter().text(half.center(), Align2::CENTER_CENTER, label, FontId::proportional(12.0), color);
        if response.clicked() && !selected {
            chosen = Some(option);
        }
    }
    chosen
}

fn outline_list(
    ui: &mut Ui,
    doc: DocId,
    entries: Option<&[OutlineEntry]>,
    current_page: usize,
    palette: &Palette,
) -> Option<Target> {
    let message = match entries {
        None => Some("Loading outline…"),
        Some([]) => Some("This PDF has no outline"),
        Some(_) => None,
    };
    if let Some(message) = message {
        let rect = ui.available_rect_before_wrap();
        ui.painter().text(
            pos2(rect.center().x, rect.top() + 24.0),
            Align2::CENTER_CENTER,
            message,
            FontId::proportional(12.0),
            palette.text_weak,
        );
        return None;
    }
    let entries = entries.unwrap_or_default();
    // The section being read: the last entry that starts at or before the current page.
    let current = entries
        .iter()
        .rposition(|e| matches!(e.target, Some(Target::Page { page, .. }) if page as usize <= current_page));

    let mut clicked = None;
    ScrollArea::vertical().id_salt(("outline", doc)).auto_shrink(false).show_rows(
        ui,
        OUTLINE_ROW,
        entries.len(),
        |ui, rows| {
            for i in rows {
                let entry = &entries[i];
                let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), OUTLINE_ROW), Sense::click());
                let row = rect.shrink2(vec2(6.0, 1.0));
                let painter = ui.painter();
                if Some(i) == current {
                    painter.rect_filled(row, CornerRadius::same(5), palette.separator);
                } else if response.hovered() {
                    painter.rect_filled(row, CornerRadius::same(5), palette.hover);
                }
                let page = match entry.target {
                    Some(Target::Page { page, .. }) => Some(page),
                    _ => None,
                };
                let number = page.map(|p| (p + 1).to_string()).unwrap_or_default();
                let number_galley = painter.layout_no_wrap(number, FontId::proportional(11.0), palette.text_weak);
                let left = row.left() + 6.0 + entry.depth.min(6) as f32 * 12.0;
                let right = row.right() - 6.0 - number_galley.size().x - 6.0;
                let color = if entry.depth == 0 { palette.text } else { palette.text_weak };
                let mut job = LayoutJob::simple_singleline(entry.title.clone(), FontId::proportional(12.0), color);
                job.wrap.max_width = (right - left).max(10.0);
                job.wrap.max_rows = 1;
                job.wrap.break_anywhere = true;
                let title = painter.layout_job(job);
                painter.galley(pos2(left, row.center().y - title.size().y / 2.0), title, color);
                painter.galley(
                    pos2(row.right() - 6.0 - number_galley.size().x, row.center().y - number_galley.size().y / 2.0),
                    number_galley,
                    palette.text_weak,
                );
                if response.on_hover_cursor(CursorIcon::PointingHand).clicked() {
                    clicked = entry.target.clone();
                }
            }
        },
    );
    clicked
}

/// Returns the page the user clicked, if any.
#[allow(clippy::too_many_arguments)]
fn thumbnails(
    ui: &mut Ui,
    doc: DocId,
    layout: &DocLayout,
    view: &mut View,
    cache: &mut TextureCache,
    failed: &HashSet<Key>,
    wants: &mut Wants,
    palette: &Palette,
) -> Option<usize> {
    let ppp = ui.ctx().pixels_per_point();
    let width_px = thumb_width_px(ppp);
    let mut tops = Vec::with_capacity(layout.page_count() + 1);
    let mut y = PADDING;
    for page in 0..layout.page_count() {
        tops.push(y);
        y += thumb_height(layout.page_size(page)) + LABEL_HEIGHT + SPACING;
    }
    let content_height = y + PADDING;
    let follow = view.sidebar_page != Some(view.current_page);
    view.sidebar_page = Some(view.current_page);

    let mut clicked = None;
    ScrollArea::vertical().id_salt(("thumbs", doc)).auto_shrink(false).show_viewport(ui, |ui, viewport| {
        ui.set_min_size(vec2(ui.available_width(), content_height));
        let origin = ui.max_rect().min;
        let center_x = ui.max_rect().center().x;
        let first = tops.partition_point(|t| *t <= viewport.top()).saturating_sub(1);

        if follow && let Some(&top) = tops.get(view.current_page) {
            let height = thumb_height(layout.page_size(view.current_page)) + LABEL_HEIGHT;
            let rect = Rect::from_min_size(pos2(origin.x, origin.y + top), vec2(1.0, height));
            ui.scroll_to_rect(rect, None);
        }

        for (page, &top) in tops.iter().enumerate().skip(first) {
            if top > viewport.bottom() {
                break;
            }
            let th = thumb_height(layout.page_size(page));
            let thumb = Rect::from_min_size(pos2(center_x - THUMB_WIDTH / 2.0, origin.y + top), vec2(THUMB_WIDTH, th));
            let item = Rect::from_min_max(pos2(ui.max_rect().left(), thumb.top()), pos2(ui.max_rect().right(), thumb.bottom() + LABEL_HEIGHT));
            let selected = page == view.current_page;
            let painter = ui.painter();

            if selected {
                painter.rect_filled(thumb.expand(4.0), CornerRadius::same(5), palette.separator);
            }
            painter.rect_filled(thumb, 0.0, Color32::WHITE);
            let key = Key::Thumb { doc, page: page as u32, width: width_px };
            match cache.get(&key) {
                Some(id) => {
                    let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
                    painter.image(id, thumb, uv, Color32::WHITE);
                }
                None if !failed.contains(&key) => wants.thumbs.push(key),
                None => {}
            }
            painter.rect_stroke(thumb, 0.0, Stroke::new(1.0, palette.page_border), StrokeKind::Outside);

            let label_center = pos2(center_x, thumb.bottom() + LABEL_HEIGHT / 2.0 + 2.0);
            let font = FontId::proportional(11.0);
            if selected {
                let galley = painter.layout_no_wrap((page + 1).to_string(), font.clone(), Color32::WHITE);
                let pill = Rect::from_center_size(label_center, galley.size() + vec2(12.0, 4.0));
                painter.rect_filled(pill, CornerRadius::same(8), palette.accent);
                painter.galley(pill.center() - galley.size() / 2.0, galley, Color32::WHITE);
            } else {
                painter.text(label_center, Align2::CENTER_CENTER, page + 1, font, palette.text_weak);
            }

            let response = ui.interact(item, ui.id().with(("thumb", page)), Sense::click());
            if response.clicked() {
                clicked = Some(page);
            }
            response.on_hover_cursor(CursorIcon::PointingHand);
        }
    });
    clicked
}
