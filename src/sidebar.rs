//! Page thumbnail sidebar (toggled with ⌘B).

use std::collections::HashSet;

use eframe::egui::{
    Align2, Color32, CornerRadius, CursorIcon, FontId, Rect, ScrollArea, Sense, Stroke,
    StrokeKind, Ui, pos2, vec2,
};

use crate::cache::TextureCache;
use crate::layout::DocLayout;
use crate::renderer::{DocId, Key};
use crate::tab::View;
use crate::theme::Palette;
use crate::view::Wants;

pub const WIDTH: f32 = 160.0;
const THUMB_WIDTH: f32 = 108.0;
const LABEL_HEIGHT: f32 = 22.0;
const SPACING: f32 = 12.0;
const PADDING: f32 = 12.0;

pub fn thumb_width_px(pixels_per_point: f32) -> u32 {
    (THUMB_WIDTH * pixels_per_point).round() as u32
}

fn thumb_height(size: eframe::egui::Vec2) -> f32 {
    (THUMB_WIDTH * size.y / size.x).min(THUMB_WIDTH * 2.0)
}

/// Returns the page the user clicked, if any.
#[allow(clippy::too_many_arguments)]
pub fn show(
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
