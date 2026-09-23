//! The scrolling page canvas of the active tab.

use std::collections::HashSet;
use std::time::Duration;

use eframe::egui::{
    Color32, Pos2, Rect, ScrollArea, Stroke, StrokeKind, TextureHandle, Ui, Vec2, pos2, vec2,
};

use crate::cache::TextureCache;
use crate::layout::{DocLayout, MARGIN};
use crate::renderer::{DocId, Key, SnapshotJob, SnapshotPage, TILE, page_pixels, scale_key};
use crate::sidebar;
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
    ((viewport_width - 2.0 * MARGIN) / layout.max_width()).clamp(MIN_ZOOM, MAX_ZOOM)
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
        let page = layout.page_at(point.y, self.zoom);
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
    }

    pub fn go_to_page(&mut self, layout: &DocLayout, page: usize) {
        if layout.page_count() == 0 {
            return;
        }
        let page = page.min(layout.page_count() - 1);
        let rect = layout.page_rect(page, self.zoom, self.content_width(layout, self.zoom));
        let x = self.pending_offset.unwrap_or(self.offset).x;
        self.pending_offset = Some(vec2(x, (rect.top() - MARGIN).max(0.0)));
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
    pub page_border: Color32,
}

impl Canvas<'_> {
    /// Returns whether every visible tile was drawn at full sharpness.
    pub fn show(self, ui: &mut Ui, wants: &mut Wants) -> bool {
        let Canvas { doc, layout, view, snapshot, cache, failed, page_border } = self;
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
        if let Some(offset) = view.pending_offset.take() {
            scroll = scroll.scroll_offset(offset);
        }

        let scale = scale_key(view.render_zoom * ppp);
        let prev_scale = view.prev_render_zoom.map(|z| scale_key(z * ppp)).filter(|s| *s != scale);
        let thumb_width = sidebar::thumb_width_px(ppp);

        let output = scroll.show_viewport(ui, |ui, viewport| {
            ui.set_min_size(content);
            let origin = ui.max_rect().min.to_vec2();
            let painter = ui.painter();
            let screen_viewport = viewport.translate(origin);
            let prefetch = screen_viewport.expand2(vec2(0.0, viewport.height() * 0.5));

            if let Some(texture) = snapshot.as_ref() {
                let size = Vec2::from(texture.size().map(|v| v as f32)) / snapshot_scale(ppp);
                painter.image(texture.id(), Rect::from_min_size(screen_viewport.min, size), UV, Color32::WHITE);
            }

            let mut all_visible_ready = true;
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
                    if snapshot.is_none() {
                        painter.rect_filled(rect, 0.0, Color32::WHITE);
                        let thumb = Key::Thumb { doc, page: page as u32, width: thumb_width };
                        if let Some(id) = cache.peek(&thumb) {
                            painter.image(id, rect, UV, Color32::WHITE);
                        }
                    }
                    painter.rect_stroke(rect, 0.0, Stroke::new(1.0, page_border), StrokeKind::Outside);
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

            view.current_page = layout.page_at(viewport.center().y, view.zoom);
            all_visible_ready
        });

        let scrolled = output.state.offset != view.offset;
        view.offset = output.state.offset;
        view.viewport = output.inner_rect;

        if output.inner || scrolled || view.zoom != view.render_zoom {
            *snapshot = None;
        }
        if output.inner && view.prev_render_zoom.take().is_some() {
            // The new scale fully covers the viewport; drop tiles rendered at other scales.
            cache.retain(|k| !matches!(*k, Key::Tile { doc: d, scale: s, .. } if d == doc && s != scale));
        }
        output.inner && view.zoom == view.render_zoom
    }
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
