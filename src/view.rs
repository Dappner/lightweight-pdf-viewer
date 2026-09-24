//! The scrolling page canvas of the active tab.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use eframe::egui::{
    Color32, CursorIcon, Pos2, Rect, ScrollArea, Sense, Stroke, StrokeKind, TextureHandle, Ui, Vec2,
    pos2, vec2,
};

use crate::cache::TextureCache;
use crate::find_bar;
use crate::history::Place;
use crate::layout::{DocLayout, MARGIN, PageLayout};
use crate::renderer::{DocId, Key, Link, SnapshotJob, SnapshotPage, TILE, Target, page_pixels, scale_key};
use crate::search::Search;
use crate::tab::View;

pub const MIN_ZOOM: f32 = 0.1;
pub const MAX_ZOOM: f32 = 8.0;
/// Seconds the zoom must stay unchanged before tiles are re-rendered at the new scale.
const ZOOM_SETTLE: f64 = 0.15;

const UV: Rect = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));

/// Render requests collected during a frame, in priority order.
#[derive(Default)]
pub struct Wants {
    pub visible: Vec<Key>,
    pub thumbs: Vec<Key>,
    pub prefetch: Vec<Key>,
}

impl Wants {
    pub fn into_queue(self) -> Vec<Key> {
        let mut queue = self.visible;
        queue.extend(self.thumbs);
        queue.extend(self.prefetch);
        queue
    }
}

/// Snapshot pixels per point: half of Retina resolution, never below 1×.
pub fn snapshot_scale(pixels_per_point: f32) -> f32 {
    (pixels_per_point * 0.5).max(1.0)
}

pub fn fit_width_zoom(layout: &DocLayout, viewport_width: f32) -> f32 {
    layout.fit_width(viewport_width).clamp(MIN_ZOOM, MAX_ZOOM)
}

impl View {
    fn content_width(&self, layout: &DocLayout, zoom: f32) -> f32 {
        layout.content_size(zoom, self.viewport.width()).x
    }

    /// Changes the zoom while keeping the document point under `anchor` (relative to the
    /// viewport's top-left) fixed on screen.
    pub fn zoom_about(&mut self, layout: &DocLayout, zoom: f32, anchor: Vec2, now: f64) {
        let zoom = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        if zoom == self.zoom || layout.page_count() == 0 {
            return;
        }
        let base = self.pending_offset.unwrap_or(self.offset);
        let point = base + anchor;
        let page = layout.page_near(point.to_pos2(), self.zoom, self.content_width(layout, self.zoom));
        let old_rect = layout.page_rect(page, self.zoom, self.content_width(layout, self.zoom));
        let in_page = (point - old_rect.min.to_vec2()) / self.zoom;
        let new_rect = layout.page_rect(page, zoom, self.content_width(layout, zoom));
        let new_point = new_rect.min + in_page * zoom;

        self.zoom = zoom;
        self.zoom_changed_at = now;
        self.pending_offset = Some((new_point.to_vec2() - anchor).max(Vec2::ZERO));
    }

    pub fn scroll_by(&mut self, delta: Vec2) {
        let base = self.pending_offset.unwrap_or(self.offset);
        self.pending_offset = Some((base + delta).max(Vec2::ZERO));
        // Scrolling leaves the page navigated to; the most visible one becomes current.
        self.landed_page = None;
    }

    pub fn go_to_page(&mut self, layout: &DocLayout, page: usize) {
        if layout.page_count() == 0 {
            return;
        }
        let page = page.min(layout.page_count() - 1);
        let rect = layout.page_rect(page, self.zoom, self.content_width(layout, self.zoom));
        let x = self.pending_offset.unwrap_or(self.offset).x;
        self.pending_offset = Some(vec2(x, (rect.top() - MARGIN).max(0.0)));
        self.landed_page = Some(page);
    }

    /// Scrolls so `y` (0 = top of the page, 1 = bottom) sits at the top of the viewport.
    pub fn go_to_point(&mut self, layout: &DocLayout, page: usize, y: Option<f32>) {
        self.go_to_page(layout, page);
        if let (Some(y), Some(offset)) = (y, self.pending_offset.as_mut()) {
            offset.y += MARGIN + y * layout.page_size(page.min(layout.page_count() - 1)).y * self.zoom;
        }
    }

    /// Where the reader is: the first page of the top row, and how far down it the top of the
    /// viewport is. A row scrolled to with [`Self::go_to_page`] counts as the top one even
    /// though the margin above it is showing. Only meaningful once the zoom is set.
    pub fn place(&self, layout: &DocLayout) -> Place {
        let y = self.pending_offset.unwrap_or(self.offset).y + MARGIN;
        let page = layout.page_at(y, self.zoom);
        let rect = layout.page_rect(page, self.zoom, self.content_width(layout, self.zoom));
        Place { layout: layout.mode(), page, y: ((y - rect.top()) / rect.height()).clamp(0.0, 1.0), zoom: self.zoom }
    }

    /// Scrolls back to a [`Self::place`].
    pub fn go_to_place(&mut self, layout: &DocLayout, page: usize, y: f32) {
        self.go_to_point(layout, page, Some(y));
        if let Some(offset) = self.pending_offset.as_mut() {
            offset.y = (offset.y - MARGIN).max(0.0);
        }
    }

    /// Scrolls `area` of `page` (fractions of the page) into view if it isn't already,
    /// placing it a third of the way down the viewport.
    pub fn reveal(&mut self, layout: &DocLayout, page: usize, area: Rect) {
        if page >= layout.page_count() {
            return;
        }
        let page_rect = layout.page_rect(page, self.zoom, self.content_width(layout, self.zoom));
        let target = find_bar::on_page(page_rect, area);
        let offset = self.pending_offset.unwrap_or(self.offset);
        let visible = Rect::from_min_size(offset.to_pos2(), self.viewport.size());
        if visible.contains_rect(target) {
            return;
        }
        let mut offset = offset;
        if target.top() < visible.top() || target.bottom() > visible.bottom() {
            offset.y = target.center().y - visible.height() / 3.0;
        }
        if target.left() < visible.left() || target.right() > visible.right() {
            offset.x = target.center().x - visible.width() / 2.0;
        }
        self.pending_offset = Some(offset.max(Vec2::ZERO));
        self.landed_page = None;
    }

    /// Rearranges the pages, fitting the new rows to the window and keeping the page at the
    /// top of the viewport in place.
    pub fn set_layout(&mut self, layout: &mut DocLayout, mode: PageLayout, now: f64) {
        if mode == layout.mode() || layout.page_count() == 0 {
            return;
        }
        let place = self.place(layout);
        layout.set_mode(mode);
        self.zoom = fit_width_zoom(layout, self.viewport.width());
        self.zoom_changed_at = now;
        self.go_to_place(layout, place.page, place.y);
    }

    /// Describes the current viewport so the worker can render it into a snapshot.
    pub fn snapshot_job(&self, layout: &DocLayout, doc: DocId, pixels_per_point: f32) -> SnapshotJob {
        let s = snapshot_scale(pixels_per_point);
        let size = self.viewport.size();
        let content_width = self.content_width(layout, self.zoom);
        let pages = layout
            .pages_in(self.offset.y, self.offset.y + size.y, self.zoom)
            .map(|page| {
                let r = layout.page_rect(page, self.zoom, content_width).translate(-self.offset);
                SnapshotPage {
                    page: page as u32,
                    x: (r.min.x * s).round() as i32,
                    y: (r.min.y * s).round() as i32,
                    width: (r.width() * s).round().max(1.0) as i32,
                    height: (r.height() * s).round().max(1.0) as i32,
                }
            })
            .collect();
        SnapshotJob {
            doc,
            size: [(size.x * s).round().max(1.0) as u32, (size.y * s).round().max(1.0) as u32],
            pages,
        }
    }
}

pub struct Canvas<'a> {
    pub doc: DocId,
    pub layout: &'a DocLayout,
    pub view: &'a mut View,
    pub snapshot: &'a mut Option<TextureHandle>,
    pub cache: &'a mut TextureCache,
    pub failed: &'a HashSet<Key>,
    /// Link annotations of the pages loaded so far.
    pub links: &'a HashMap<u32, Vec<Link>>,
    /// The find bar's search, whose matches are highlighted.
    pub search: Option<&'a Search>,
    /// Width of the sidebar's thumbnails, drawn as placeholders until tiles arrive.
    pub thumb_px: u32,
    /// Screen heights rendered ahead, above and below the visible area.
    pub prefetch: f32,
    pub page_border: Color32,
}

pub struct CanvasOutput {
    /// Every visible tile was drawn at full sharpness.
    pub ready: bool,
    pub clicked_link: Option<Target>,
    /// Pages at least partly on screen.
    pub visible_pages: Vec<u32>,
}

impl Canvas<'_> {
    pub fn show(self, ui: &mut Ui, wants: &mut Wants) -> CanvasOutput {
        let Canvas { doc, layout, view, snapshot, cache, failed, links, search, thumb_px, prefetch, page_border } = self;
        let ppp = ui.ctx().pixels_per_point();
        let now = ui.input(|i| i.time);
        let area = ui.available_rect_before_wrap();
        view.viewport = area;

        if view.zoom == 0.0 {
            view.zoom = fit_width_zoom(layout, area.width());
            view.render_zoom = view.zoom;
        }

        // Pinch or ⌘-scroll zooms around the pointer.
        let (zoom_delta, hover) = ui.input(|i| (i.zoom_delta(), i.pointer.hover_pos()));
        if zoom_delta != 1.0
            && let Some(pointer) = hover.filter(|p| area.contains(*p))
        {
            view.zoom_about(layout, view.zoom * zoom_delta, pointer - area.min, now);
        }

        if view.zoom != view.render_zoom {
            if now - view.zoom_changed_at >= ZOOM_SETTLE {
                view.prev_render_zoom.get_or_insert(view.render_zoom);
                view.render_zoom = view.zoom;
            } else {
                ui.ctx().request_repaint_after(Duration::from_secs_f64(ZOOM_SETTLE));
            }
        }

        let content = layout.content_size(view.zoom, area.width());
        let mut scroll = ScrollArea::both().id_salt(("doc", doc)).auto_shrink(false);
        let jumped = view.pending_offset.is_some();
        if let Some(offset) = view.pending_offset.take() {
            scroll = scroll.scroll_offset(offset);
        }

        let scale = scale_key(view.render_zoom * ppp);
        let prev_scale = view.prev_render_zoom.map(|z| scale_key(z * ppp)).filter(|s| *s != scale);

        let output = scroll.show_viewport(ui, |ui, viewport| {
            ui.set_min_size(content);
            let origin = ui.max_rect().min.to_vec2();
            let painter = ui.painter();
            let screen_viewport = viewport.translate(origin);
            let prefetch = screen_viewport.expand2(vec2(0.0, viewport.height() * prefetch));

            if let Some(texture) = snapshot.as_ref() {
                let size = Vec2::from(texture.size().map(|v| v as f32)) / snapshot_scale(ppp);
                painter.image(texture.id(), Rect::from_min_size(screen_viewport.min, size), UV, Color32::WHITE);
            }

            let mut all_visible_ready = true;
            let mut clicked_link = None;
            let mut visible_pages: Vec<(u32, Rect)> = Vec::new();
            // The page taking up the most of the viewport; the first one on a tie.
            let mut most_visible = (0.0, layout.page_at(viewport.top(), view.zoom));
            let pages = layout.pages_in(prefetch.top() - origin.y, prefetch.bottom() - origin.y, view.zoom);
            for page in pages {
                let size = layout.page_size(page);
                // Size the page from its rendered pixel dimensions so tiles map 1:1 onto
                // physical pixels once the zoom settles (otherwise they get resampled).
                let [pw, ph] = page_pixels(size, scale);
                let points_per_pixel = view.zoom / (view.render_zoom * ppp);
                let min = snap_pos(layout.page_rect(page, view.zoom, content.x).min + origin, ppp);
                let rect = Rect::from_min_size(min, vec2(pw as f32, ph as f32) * points_per_pixel);
                let visible = rect.intersects(screen_viewport);

                if visible {
                    visible_pages.push((page as u32, rect));
                    let shown = rect.intersect(screen_viewport).area();
                    if shown > most_visible.0 + 0.5 {
                        most_visible = (shown, page);
                    }
                    if snapshot.is_none() {
                        painter.rect_filled(rect, 0.0, Color32::WHITE);
                        let thumb = Key::Thumb { doc, page: page as u32, width: thumb_px };
                        if let Some(id) = cache.peek(&thumb) {
                            painter.image(id, rect, UV, Color32::WHITE);
                        }
                    }
                    painter.rect_stroke(rect, 0.0, Stroke::new(1.0, page_border), StrokeKind::Outside);
                    match links.get(&(page as u32)) {
                        None => wants.visible.push(Key::Links { doc, page: page as u32 }),
                        Some(page_links) => {
                            for (i, link) in page_links.iter().enumerate() {
                                let area = Rect::from_min_max(
                                    rect.min + link.rect.min.to_vec2() * rect.size(),
                                    rect.min + link.rect.max.to_vec2() * rect.size(),
                                );
                                let response = ui
                                    .interact(area, ui.id().with(("link", doc, page, i)), Sense::click())
                                    .on_hover_cursor(CursorIcon::PointingHand);
                                if response.clicked() {
                                    clicked_link = Some(link.target.clone());
                                }
                            }
                        }
                    }
                    if let Some(prev) = prev_scale {
                        for (key, tile_rect) in tiles(doc, page, size, prev, rect, screen_viewport) {
                            if let Some(id) = cache.peek(&key) {
                                painter.image(id, tile_rect, UV, Color32::WHITE);
                            }
                        }
                    }
                }

                for (key, tile_rect) in tiles(doc, page, size, scale, rect, prefetch) {
                    let tile_visible = tile_rect.intersects(screen_viewport);
                    match cache.get(&key) {
                        Some(id) if tile_visible => {
                            painter.image(id, tile_rect, UV, Color32::WHITE);
                        }
                        Some(_) => {}
                        None if failed.contains(&key) => {}
                        None if tile_visible => {
                            all_visible_ready = false;
                            wants.visible.push(key);
                        }
                        None => wants.prefetch.push(key),
                    }
                }
            }

            // Over the tiles: every match on screen, the current one stronger.
            if let Some(search) = search {
                let (all, current) = highlight_colors();
                for &(page, rect) in &visible_pages {
                    for (area, is_current) in search.highlights(page) {
                        let area = find_bar::on_page(rect, area).expand(1.0);
                        painter.rect_filled(area, 2.0, if is_current { current } else { all });
                    }
                }
            }

            (all_visible_ready, clicked_link, most_visible.1, visible_pages.into_iter().map(|(p, _)| p).collect())
        });
        let (all_visible_ready, clicked_link, most_visible, visible_pages) = output.inner;

        let scrolled = output.state.offset != view.offset;
        if scrolled && !jumped {
            view.landed_page = None;
        }
        view.current_page = view.landed_page.unwrap_or(most_visible);
        view.offset = output.state.offset;
        view.viewport = output.inner_rect;

        if all_visible_ready || scrolled || view.zoom != view.render_zoom {
            *snapshot = None;
        }
        if all_visible_ready && view.prev_render_zoom.take().is_some() {
            // The new scale fully covers the viewport; drop tiles rendered at other scales.
            cache.retain(|k| !matches!(*k, Key::Tile { doc: d, scale: s, .. } if d == doc && s != scale));
        }
        CanvasOutput { ready: all_visible_ready && view.zoom == view.render_zoom, clicked_link, visible_pages }
    }
}

/// Fill colors of search matches: all of them, and the current one.
fn highlight_colors() -> (Color32, Color32) {
    (Color32::from_rgba_unmultiplied(255, 204, 0, 90), Color32::from_rgba_unmultiplied(255, 140, 0, 150))
}

/// Aligns a position to the physical pixel grid.
fn snap_pos(p: Pos2, ppp: f32) -> Pos2 {
    pos2((p.x * ppp).round() / ppp, (p.y * ppp).round() / ppp)
}

/// Tiles of `page` rendered at `scale` that intersect `area`, with their screen rects.
fn tiles(
    doc: DocId,
    page: usize,
    size: Vec2,
    scale: u32,
    page_rect: Rect,
    area: Rect,
) -> impl Iterator<Item = (Key, Rect)> {
    let [pw, ph] = page_pixels(size, scale);
    let fx = page_rect.width() / pw as f32;
    let fy = page_rect.height() / ph as f32;
    let area = area.intersect(page_rect);
    let (tx0, ty0, tx1, ty1) = if area.is_positive() {
        let min = area.min - page_rect.min;
        let max = area.max - page_rect.min;
        (
            (min.x / fx) as u32 / TILE,
            (min.y / fy) as u32 / TILE,
            ((max.x / fx).ceil() as u32).div_ceil(TILE).min(pw.div_ceil(TILE)),
            ((max.y / fy).ceil() as u32).div_ceil(TILE).min(ph.div_ceil(TILE)),
        )
    } else {
        (0, 0, 0, 0)
    };
    (ty0..ty1).flat_map(move |ty| {
        (tx0..tx1).map(move |tx| {
            let x0 = tx * TILE;
            let y0 = ty * TILE;
            let w = TILE.min(pw - x0);
            let h = TILE.min(ph - y0);
            let rect = Rect::from_min_size(
                page_rect.min + vec2(x0 as f32 * fx, y0 as f32 * fy),
                vec2(w as f32 * fx, h as f32 * fy),
            );
            (Key::Tile { doc, page: page as u32, scale, tx, ty }, rect)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn places_round_trip() {
        let layout = DocLayout::new(vec![vec2(612.0, 792.0); 9], PageLayout::Book);
        for offset in [0.0, 300.0, 1234.5] {
            let mut view = View { zoom: 0.8, offset: vec2(0.0, offset), ..View::default() };
            let place = view.place(&layout);
            view.go_to_place(&layout, place.page, place.y);
            assert!((view.pending_offset.unwrap().y - offset).abs() < 0.01, "offset {offset}");
        }
    }

    #[test]
    fn a_row_scrolled_to_is_the_top_one() {
        let layout = DocLayout::new(vec![vec2(612.0, 792.0); 9], PageLayout::Book);
        let mut view = View { zoom: 0.8, ..View::default() };
        view.go_to_page(&layout, 3);
        assert_eq!(view.place(&layout).page, 3);
    }
}
