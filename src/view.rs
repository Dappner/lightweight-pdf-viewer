//! The scrolling page canvas of the active tab.

use eframe::egui::ahash::{HashMap, HashSet};
use std::time::Duration;

use eframe::egui::epaint::{Mesh, Shape, Tessellator, TextShape};
use eframe::egui::scroll_area::ScrollSource;
use eframe::egui::{
    Button, Color32, CursorIcon, Galley, Margin, Pos2, Rect, Response, ScrollArea, Sense, Stroke, StrokeKind, TextEdit,
    TextureHandle, Ui, Vec2, pos2, vec2,
};
use std::sync::Arc;

use crate::annotate::{Style, Tool};
use crate::app::Command;
use crate::cache::TextureCache;
use crate::find_bar;
use crate::grey;
use crate::history::Place;
use crate::layout::{DocLayout, MARGIN, PageLayout, Rotation, Rotations};
use crate::renderer::{DocId, Key, Link, SnapshotJob, SnapshotPage, TILE, Target, page_pixels, scale_key};
use crate::search::Search;
use crate::selection::Unit;
use crate::tab::View;
use crate::text_boxes::{self, BoxRef, Edge, PAD, TextBox, TextBoxes, turn, unturn};
use crate::text_layers::{MIN_LAYER_HEIGHT, TextLayers};

pub const MIN_ZOOM: f32 = 0.1;
pub const MAX_ZOOM: f32 = 8.0;
/// Seconds the zoom must stay unchanged before tiles are re-rendered at the new scale.
// TODO(perf): 0.15 s is most of the ~175 ms "zoom to sharp" in the benchmark. Try
// 0.08-0.1 s: sharper sooner, but more discarded renders during a slow pinch.
const ZOOM_SETTLE: f64 = 0.15;
/// Points a second the pages scroll per point a selection is dragged past the top or bottom
/// edge, up to [`EDGE_SCROLL_MAX`] points past.
const EDGE_SCROLL: f32 = 30.0;
const EDGE_SCROLL_MAX: f32 = 60.0;
/// Opacity highlights are drawn with over the page.
const HIGHLIGHT_ALPHA: u8 = 110;

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

    /// Where the reader is: a page of the top row, and how far down it the top of the
    /// viewport is. Use the tallest page to cover rows with unequal page heights, keeping
    /// an explicitly navigated page when it also covers this position.
    /// A row scrolled to with [`Self::go_to_page`] counts as the top one even
    /// though the margin above it is showing. Only meaningful once the zoom is set.
    pub fn place(&self, layout: &DocLayout) -> Place {
        let y = self.pending_offset.unwrap_or(self.offset).y + MARGIN;
        let first = layout.page_at(y, self.zoom);
        let spread = layout.spread(first);
        let width = self.content_width(layout, self.zoom);
        let page = self
            .landed_page
            .filter(|p| spread.contains(p) && layout.page_rect(*p, self.zoom, width).y_range().contains(y))
            .unwrap_or_else(|| {
                spread.reduce(|a, b| if layout.page_size(b).y > layout.page_size(a).y { b } else { a }).unwrap_or(first)
            });
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
        SnapshotJob { doc, size: [(size.x * s).round().max(1.0) as u32, (size.y * s).round().max(1.0) as u32], pages }
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
    /// Text layers and the selection, which the pointer selects with.
    pub text: &'a mut TextLayers,
    pub text_boxes: &'a mut TextBoxes,
    /// How the reader has turned the pages, which the text boxes turn with.
    pub rotations: &'a Rotations,
    /// Annotate mode's tool, while it's on.
    pub tool: Option<Tool>,
    /// What new highlights and text boxes look like.
    pub style: Style,
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
    /// Chosen in the right-click menu.
    pub command: Option<Command>,
    /// Pages at least partly on screen.
    pub visible_pages: Vec<u32>,
}

impl Canvas<'_> {
    pub fn show(self, ui: &mut Ui, wants: &mut Wants) -> CanvasOutput {
        let Canvas {
            doc,
            layout,
            view,
            snapshot,
            cache,
            failed,
            links,
            search,
            text,
            text_boxes,
            rotations,
            tool,
            style,
            thumb_px,
            prefetch,
            page_border,
        } = self;
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
        // Dragging selects text rather than scrolling.
        let mut scroll = ScrollArea::both()
            .id_salt(("doc", doc))
            .auto_shrink(false)
            .scroll_source(ScrollSource { drag: false, ..ScrollSource::ALL });
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
            // Under the links, which take clicks before it; drags are its alone. Clear of the
            // sidebar's resize handle, which reaches into the canvas and was laid out first.
            let grab = ui.style().interaction.resize_grab_radius_side;
            let area = Rect::from_x_y_ranges(
                screen_viewport.left() + grab..=screen_viewport.right(),
                screen_viewport.y_range(),
            );
            let pointer = ui.interact(area, ui.id().with(("canvas", doc)), Sense::click_and_drag());
            let mut over_link = false;
            let mut page_rects: Vec<(u32, Rect)> = Vec::new();

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
                page_rects.push((page as u32, rect));

                if visible {
                    visible_pages.push((page as u32, rect));
                    let shown = rect.intersect(screen_viewport).area();
                    if shown > most_visible.0 + 0.5 {
                        most_visible = (shown, page);
                    }
                    if snapshot.is_none() {
                        painter.rect_filled(rect, 0.0, Color32::WHITE);
                        let thumb = Key::Thumb { doc, page: page as u32, width: thumb_px };
                        if let Some(image) = cache.peek(&thumb) {
                            grey::paint(painter, image, rect);
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
                                over_link |= response.hovered();
                                if response.clicked() {
                                    clicked_link = Some(link.target.clone());
                                }
                            }
                        }
                    }
                    if let Some(prev) = prev_scale {
                        for (key, tile_rect) in tiles(doc, page, size, prev, rect, screen_viewport) {
                            if let Some(image) = cache.peek(&key) {
                                grey::paint(painter, image, tile_rect);
                            }
                        }
                    }
                }

                for (key, tile_rect) in tiles(doc, page, size, scale, rect, prefetch) {
                    let tile_visible = tile_rect.intersects(screen_viewport);
                    match cache.get(&key) {
                        Some(image) if tile_visible => grey::paint(painter, image, tile_rect),
                        Some(_) => {}
                        None if failed.contains(&key) => {
                            if tile_visible {
                                all_visible_ready = false;
                            }
                        }
                        None if tile_visible => {
                            all_visible_ready = false;
                            wants.visible.push(key);
                        }
                        None => wants.prefetch.push(key),
                    }
                }
            }

            let scales = [Some(scale), prev_scale];
            show_highlights(ui, text_boxes, &visible_pages, rotations, layout, cache, doc, scales);
            select(ui, &pointer, text, view, &page_rects, over_link, screen_viewport);
            let command = page_menu(ui, &pointer, &page_rects, layout, text, text_boxes, rotations, &style);
            // Text selected to highlight shows in the highlight's colour.
            let tint = match tool {
                Some(Tool::Highlight) => {
                    let [r, g, b] = style.highlight;
                    Color32::from_rgba_unmultiplied(r, g, b, HIGHLIGHT_ALPHA)
                }
                _ => selection_color(),
            };
            // Over the tiles: the selection, then every match on screen, the current one stronger.
            if let Some(selection) = text.selection() {
                for &(page, rect) in &visible_pages {
                    if !selection.pages().contains(&page) {
                        continue;
                    }
                    // A page selected whole and too small to read is tinted, its text unread.
                    let whole = page != selection.start().page && page != selection.end().page;
                    if whole && rect.height() < MIN_LAYER_HEIGHT {
                        painter.rect_filled(rect, 0.0, tint);
                        continue;
                    }
                    text.want(ui.ctx(), page);
                    let Some(layer) = text.layer(page) else { continue };
                    let Some(range) = selection.range_on(page, layer.len()) else { continue };
                    for area in layer.rects(range) {
                        painter.rect_filled(find_bar::on_page(rect, area), 0.0, tint);
                    }
                }
            }

            if let Some(search) = search {
                let (all, current) = highlight_colors();
                for &(page, rect) in &visible_pages {
                    for (area, is_current) in search.highlights(page) {
                        let area = find_bar::on_page(rect, area).expand(1.0);
                        painter.rect_filled(area, 2.0, if is_current { current } else { all });
                    }
                }
            }

            let zooming = view.zoom != view.render_zoom;
            show_text_boxes(ui, &pointer, text_boxes, &visible_pages, layout, rotations, doc, zooming);
            // After the boxes, whose part in a click would end typing into a box it adds.
            if let Some(tool) = tool {
                annotate(ui, &pointer, text, text_boxes, &page_rects, layout, tool, &style, rotations);
            }

            (
                all_visible_ready,
                clicked_link,
                command,
                most_visible.1,
                visible_pages.into_iter().map(|(p, _)| p).collect::<Vec<u32>>(),
            )
        });
        let (all_visible_ready, clicked_link, command, most_visible, visible_pages) = output.inner;

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
        text.tidy(view.current_page as u32, &visible_pages);
        CanvasOutput { ready: all_visible_ready && view.zoom == view.render_zoom, clicked_link, command, visible_pages }
    }
}

/// Draws the highlights of the pages on screen, with their screen rects. As Preview does,
/// their colour multiplies the page's, so text under them stays dark: the tiles under them
/// are drawn again, tinted. Tiles at the first of `scales` there is one at are used; where
/// there's none yet, the colour goes over the page, translucent.
#[allow(clippy::too_many_arguments)]
fn show_highlights(
    ui: &Ui,
    boxes: &TextBoxes,
    pages: &[(u32, Rect)],
    rotations: &Rotations,
    layout: &DocLayout,
    cache: &TextureCache,
    doc: DocId,
    scales: [Option<u32>; 2],
) {
    let painter = ui.painter();
    for &(page, rect) in pages {
        let Some(highlights) = boxes.highlights(page) else { continue };
        let rotation = rotations.of(page);
        let size = layout.page_size(page as usize);
        for h in highlights {
            let [r, g, b] = h.color;
            let tint = Color32::from_rgb(r, g, b);
            for area in h.rects() {
                let shown = Rect::from_two_pos(turn(area.min, rotation), turn(area.max, rotation));
                let mut missing = vec![find_bar::on_page(rect, shown)];
                for scale in scales.into_iter().flatten() {
                    missing = missing
                        .into_iter()
                        .flat_map(|part| {
                            let mut left = Vec::new();
                            for (key, tile) in tiles(doc, page as usize, size, scale, rect, part) {
                                let piece = tile.intersect(part);
                                match cache.peek(&key) {
                                    Some(image) => {
                                        let uv = Rect::from_min_max(
                                            ((piece.min - tile.min) / tile.size()).to_pos2(),
                                            ((piece.max - tile.min) / tile.size()).to_pos2(),
                                        );
                                        grey::paint_part(painter, image, piece, uv, tint);
                                    }
                                    None => left.push(piece),
                                }
                            }
                            left
                        })
                        .collect();
                }
                for part in missing {
                    painter.rect_filled(part, 0.0, tint.gamma_multiply_u8(HIGHLIGHT_ALPHA));
                }
            }
        }
    }
}

/// The selected text's line rects on each page it's on, as fractions of the page upright
/// (see [`TextBox::at`]); `None` until every one of those pages' text and highlights is read.
fn selected_lines(text: &TextLayers, boxes: &TextBoxes, rotations: &Rotations) -> Option<Vec<(u32, Vec<Rect>)>> {
    let selection = text.selection().filter(|s| !s.is_empty())?;
    let mut pages = Vec::new();
    for page in selection.pages() {
        let layer = text.layer(page)?;
        boxes.highlights(page)?;
        let Some(range) = selection.range_on(page, layer.len()) else { continue };
        let rotation = rotations.of(page);
        let rects = layer.rects(range).map(|r| Rect::from_two_pos(unturn(r.min, rotation), unturn(r.max, rotation)));
        pages.push((page, rects.collect()));
    }
    Some(pages)
}

/// What the pointer does in annotate mode: double-clicking a page away from its text adds a
/// text box there, and with the highlight tool, the text a drag, double or triple click
/// selects is highlighted once it's selected. A triple click's line replaces the word its
/// double click highlighted. `pages` are the pages laid out this frame, with their screen
/// rects.
#[allow(clippy::too_many_arguments)]
fn annotate(
    ui: &Ui,
    pointer: &Response,
    text: &mut TextLayers,
    boxes: &mut TextBoxes,
    pages: &[(u32, Rect)],
    layout: &DocLayout,
    tool: Tool,
    style: &Style,
    rotations: &Rotations,
) {
    /// Kept between frames: a highlight waiting for its selection (`Some(true)` for a
    /// double click's), and whether the last one was a double click's word.
    #[derive(Clone, Copy, Default)]
    struct State {
        waiting: Option<bool>,
        word: bool,
    }
    let key = pointer.id.with("annotate");
    let mut state = ui.data(|d| d.get_temp::<State>(key)).unwrap_or_default();
    let (double, triple) = (pointer.double_clicked(), pointer.triple_clicked());
    if pointer.drag_started() || (pointer.clicked() && !double && !triple) {
        state = State::default();
    }
    let at = pointer.interact_pointer_pos().and_then(|p| {
        let &(page, rect) = pages.iter().find(|(_, rect)| rect.contains(p))?;
        Some((page, ((p - rect.min) / rect.size()).to_pos2()))
    });
    let blank = at.filter(|&(page, p)| {
        boxes.page(page).is_some() && text.layer(page).is_some_and(|layer| !layer.is_on_letters(p))
    });
    if double
        && !triple
        && let Some((page, p)) = blank
    {
        text.clear_selection();
        let at = centred_box(ui.ctx(), p, layout.page_size(page as usize), rotations.of(page), style);
        boxes.add(page, at, style.text_size, style.text_color);
        state = State::default();
    } else if tool == Tool::Highlight {
        if triple {
            if state.word {
                boxes.undo();
            }
            state = State { waiting: Some(false), word: false };
        } else if double {
            state.waiting = Some(true);
        } else if pointer.drag_stopped() {
            state.waiting = Some(false);
        }
    }
    if let Some(double) = state.waiting
        && text.settled()
        && !text.dragging()
    {
        match selected_lines(text, boxes, rotations) {
            Some(lines) => {
                for (page, rects) in lines {
                    boxes.highlight(page, rects, style.highlight);
                }
                text.clear_selection();
                state = State { waiting: None, word: double };
            }
            // Nothing was selected.
            None if text.selection().is_none_or(|s| s.is_empty()) => state.waiting = None,
            // Waits for the text of the pages selected.
            None => {
                for page in text.selection().into_iter().flat_map(|s| s.pages()) {
                    text.want(ui.ctx(), page);
                }
            }
        }
    }
    ui.data_mut(|d| d.insert_temp(key, state));
}

/// Where a new box goes for a click at `p` (fractions of a page shown `shown` points big
/// and turned by `rotation`): with its empty field centred on the click, as fractions of the
/// page upright (see [`TextBox::at`]).
fn centred_box(ctx: &eframe::egui::Context, p: Pos2, shown: Vec2, rotation: Rotation, style: &Style) -> Pos2 {
    let b = TextBox { size: style.text_size, ..TextBox::new(Pos2::ZERO, String::new()) };
    let galley = b.galley(ctx, "", 1.0);
    // In points, as the field is first drawn: upright, with room for the first letter.
    let field = b.size_around(&galley, 1.0) + vec2(b.size, 0.0);
    unturn((p - field / 2.0 / shown).clamp(Pos2::ZERO, pos2(1.0, 1.0)), rotation)
}

/// The right-click menu: adds a text box, highlights the selection or removes the highlight
/// clicked, copies the selection, and turns the page clicked or all of them (a document of
/// one page, such as an image, only offers the latter). `pages` are the pages laid out this
/// frame, with their screen rects. New boxes and highlights look as `style` says.
#[allow(clippy::too_many_arguments)]
fn page_menu(
    ui: &Ui,
    pointer: &Response,
    pages: &[(u32, Rect)],
    layout: &DocLayout,
    text: &mut TextLayers,
    text_boxes: &mut TextBoxes,
    rotations: &Rotations,
    style: &Style,
) -> Option<Command> {
    // The page clicked and where on it (fractions of the page as shown), kept while its menu
    // is open.
    let key = pointer.id.with("menu_page");
    if pointer.secondary_clicked()
        && let Some(p) = pointer.interact_pointer_pos()
    {
        let nearest = pages.iter().min_by(|a, b| a.1.distance_sq_to_pos(p).total_cmp(&b.1.distance_sq_to_pos(p)));
        let at = nearest.map(|&(page, rect)| (page, ((p - rect.min) / rect.size()).to_pos2()));
        ui.data_mut(|d| d.insert_temp(key, at));
    }
    let clicked = ui.data(|d| d.get_temp::<Option<(u32, Pos2)>>(key)).flatten();
    let mut command = None;
    pointer.context_menu(|ui| {
        // Only once the page's own boxes are read, which a new one is added to.
        let can_add = clicked.filter(|&(page, at)| {
            text_boxes.page(page).is_some() && Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)).contains(at)
        });
        if ui.add_enabled(can_add.is_some(), Button::new("Add Text")).clicked()
            && let Some((page, at)) = can_add
        {
            let at = centred_box(ui.ctx(), at, layout.page_size(page as usize), rotations.of(page), style);
            text_boxes.add(page, at, style.text_size, style.text_color);
        }
        let highlighted = clicked.and_then(|(page, at)| {
            let at = unturn(at, rotations.of(page));
            let index = text_boxes.highlights(page)?.iter().position(|h| h.rects().any(|r| r.contains(at)))?;
            Some((page, index))
        });
        if let Some((page, index)) = highlighted {
            if ui.button("Remove Highlight").clicked() {
                text_boxes.delete_highlight(page, index);
            }
        } else {
            let lines = selected_lines(text, text_boxes, rotations);
            if ui.add_enabled(lines.is_some(), Button::new("Highlight")).clicked()
                && let Some(lines) = lines
            {
                for (page, rects) in lines {
                    text_boxes.highlight(page, rects, style.highlight);
                }
                text.clear_selection();
            }
        }
        ui.separator();
        if ui.add_enabled(text.selection().is_some(), Button::new("Copy").shortcut_text("⌘C")).clicked() {
            text.copy(ui.ctx());
        }
        ui.separator();
        let mut item = |ui: &mut Ui, label: &str, shortcut: &str, chosen: Command| {
            if ui.add(Button::new(label).shortcut_text(shortcut)).clicked() {
                command = Some(chosen);
            }
        };
        match clicked.map(|(page, _)| page).filter(|_| layout.page_count() > 1) {
            Some(page) => {
                let page = page as usize;
                item(ui, "Rotate This Page Left", "", Command::RotatePage { page, clockwise: false });
                item(ui, "Rotate This Page Right", "", Command::RotatePage { page, clockwise: true });
                ui.separator();
                item(ui, "Rotate All Left", "⌘L", Command::RotateLeft);
                item(ui, "Rotate All Right", "⌘R", Command::RotateRight);
            }
            None => {
                item(ui, "Rotate Left", "⌘L", Command::RotateLeft);
                item(ui, "Rotate Right", "⌘R", Command::RotateRight);
            }
        }
    });
    command
}

/// What was done to a text box this frame, applied once they're all drawn.
enum BoxAction {
    Select(BoxRef),
    Edit(BoxRef),
    Drag(BoxRef, Vec2),
    Drop(BoxRef),
    /// A side dragged this far, in PDF points.
    Resize(BoxRef, Edge, f32),
    /// A side let go: the box's left edge (see [`TextBox::at`]) and width now.
    Resized(BoxRef, f32, Option<f32>),
    /// Grows to fit its text again.
    Fit(BoxRef),
    Size(BoxRef, f32),
    Color(BoxRef, [u8; 3]),
    Delete(BoxRef),
    /// Typing ended.
    Finish,
    /// Typing ended, throwing the box away.
    Discard,
}

/// Where typing into a box stands after a frame.
enum Typing {
    Going,
    Done,
    Discard,
}

/// Draws the text boxes of the pages on screen and takes the pointer's part in editing them:
/// a click selects a box, dragging moves it, dragging a side sets the width its text wraps
/// at (a double click on one fits it to the text again), a double click types into it, and its
/// right-click menu sizes, colours and deletes it. A click elsewhere ends typing. `pages` are the pages
/// on screen, with their screen rects. While `zooming`, text is drawn at a rounded size (see
/// [`text_boxes::glyph_scale`]).
#[allow(clippy::too_many_arguments)]
fn show_text_boxes(
    ui: &mut Ui,
    pointer: &Response,
    boxes: &mut TextBoxes,
    pages: &[(u32, Rect)],
    layout: &DocLayout,
    rotations: &Rotations,
    doc: DocId,
    zooming: bool,
) {
    let ctx = ui.ctx().clone();
    if pointer.clicked() || pointer.drag_started() || pointer.secondary_clicked() {
        boxes.finish_editing();
        boxes.selected = None;
    }
    let accent = ui.visuals().selection.stroke.color;
    let mut actions = Vec::new();
    for &(page, rect) in pages {
        let Some(list) = boxes.page(page) else { continue };
        let rotation = rotations.of(page);
        let shown = layout.page_size(page as usize);
        let upright = if rotation.degrees() % 180 == 90 { vec2(shown.y, shown.x) } else { shown };
        let scale = rect.width() / shown.x;
        let typing_into = boxes.editing.as_ref().filter(|e| e.page == page).map(|e| e.index);
        for (index, b) in list.iter().enumerate() {
            if typing_into == Some(Some(index)) {
                continue;
            }
            let r = BoxRef { page, index };
            let offset = boxes.dragging.filter(|(d, _)| *d == r).map_or(Vec2::ZERO, |(_, o)| o);
            let glyphs = text_boxes::glyph_scale(scale, zooming);
            let mut galley = b.galley(&ctx, &b.text, glyphs);
            let b = &match boxes.resizing.filter(|(d, ..)| *d == r) {
                Some((_, edge, by)) => {
                    let resized = b.resized(edge, by, b.size_around(&galley, glyphs).x, upright.x);
                    galley = resized.galley(&ctx, &b.text, glyphs);
                    resized
                }
                None => b.clone(),
            };
            let size = b.size_around(&galley, glyphs) * scale;
            let (anchor, area) = placed(rect, rotation, b.at() + offset, size);
            let text_at = anchor + turn_vec(vec2(PAD, PAD) * scale, rotation);
            paint_text(ui, galley, b.color32(), text_at, scale / glyphs, rotation);

            let response = ui
                .interact(area, ui.id().with(("text box", doc, page, index)), Sense::click_and_drag())
                .on_hover_cursor(CursorIcon::Grab);
            if boxes.selected == Some(r) {
                ui.painter().rect_stroke(area, 0.0, Stroke::new(1.5, accent), StrokeKind::Outside);
            } else if response.hovered() {
                ui.painter().rect_stroke(area, 0.0, Stroke::new(1.0, accent.gamma_multiply(0.5)), StrokeKind::Outside);
            }
            if response.double_clicked() {
                actions.push(BoxAction::Edit(r));
            } else if response.clicked() || response.drag_started() || response.secondary_clicked() {
                actions.push(BoxAction::Select(r));
            }
            if response.dragged() {
                ctx.set_cursor_icon(CursorIcon::Grabbing);
                let delta = unturn_vec(response.drag_delta(), rotation) / (upright * scale);
                actions.push(BoxAction::Drag(r, delta));
            }
            if response.drag_stopped() {
                actions.push(BoxAction::Drop(r));
            }
            for edge in [Edge::Left, Edge::Right] {
                let id = ui.id().with(("text box edge", doc, page, index, edge));
                let handle = edge_handle(ui, id, anchor, size, rotation, edge);
                if boxes.selected == Some(r) {
                    paint_grip(ui, anchor, size, rotation, edge, accent);
                }
                if handle.double_clicked() {
                    actions.push(BoxAction::Fit(r));
                } else if handle.clicked() || handle.drag_started() {
                    actions.push(BoxAction::Select(r));
                }
                if handle.dragged() {
                    let by = unturn_vec(handle.drag_delta(), rotation).x / scale;
                    actions.push(BoxAction::Resize(r, edge, by));
                }
                if handle.drag_stopped() {
                    actions.push(BoxAction::Resized(r, b.at[0], b.width));
                }
            }
            response.context_menu(|ui| {
                if ui.button("Edit Text").clicked() {
                    actions.push(BoxAction::Edit(r));
                }
                ui.menu_button("Size", |ui| {
                    for size in text_boxes::SIZES {
                        if ui.radio(b.size == size, format!("{size} pt")).clicked() {
                            actions.push(BoxAction::Size(r, size));
                        }
                    }
                });
                ui.menu_button("Colour", |ui| {
                    for (name, color) in text_boxes::COLORS {
                        if ui.radio(b.color == color, name).clicked() {
                            actions.push(BoxAction::Color(r, color));
                        }
                    }
                });
                ui.separator();
                if ui.button("Delete").clicked() {
                    actions.push(BoxAction::Delete(r));
                }
            });
        }
        // Only the box being typed into is cloned: `editing` needs the boxes mutably.
        let existing = typing_into.flatten().and_then(|i| boxes.page(page)?.get(i)).cloned();
        if let Some(editing) = boxes.editing.as_mut().filter(|e| e.page == page) {
            match type_into(ui, editing, existing.as_ref(), rect, rotation, scale, accent, doc) {
                Typing::Going => {}
                Typing::Done => actions.push(BoxAction::Finish),
                Typing::Discard => actions.push(BoxAction::Discard),
            }
        }
    }
    for action in actions {
        match action {
            BoxAction::Select(r) => {
                boxes.finish_editing();
                boxes.selected = Some(r);
            }
            BoxAction::Edit(r) => boxes.edit(r),
            // A box can't move while a save runs: it would snap back on release.
            BoxAction::Drag(_, _) if boxes.saving() => {}
            BoxAction::Drag(r, delta) => {
                let so_far = boxes.dragging.filter(|(d, _)| *d == r).map_or(Vec2::ZERO, |(_, o)| o);
                boxes.dragging = Some((r, so_far + delta));
            }
            BoxAction::Drop(r) => {
                if let Some((_, offset)) = boxes.dragging.take().filter(|(d, _)| *d == r)
                    && offset != Vec2::ZERO
                {
                    boxes.change_box(r, |b| {
                        let at = (b.at() + offset).clamp(Pos2::ZERO, pos2(1.0, 1.0));
                        b.at = [at.x, at.y];
                    });
                }
            }
            // As a box can't move while a save runs, nor can it be resized.
            BoxAction::Resize(_, _, _) if boxes.saving() => {}
            BoxAction::Resize(r, edge, by) => {
                let so_far = boxes.resizing.filter(|(d, e, _)| (*d, *e) == (r, edge)).map_or(0.0, |(.., b)| b);
                boxes.resizing = Some((r, edge, so_far + by));
            }
            BoxAction::Resized(r, x, width) => {
                if boxes.resizing.take().is_some_and(|(d, ..)| d == r) {
                    boxes.change_box(r, |b| {
                        b.at[0] = x;
                        b.width = width;
                    });
                }
            }
            BoxAction::Fit(r) => {
                if boxes.get(r).is_some_and(|b| b.width.is_some()) {
                    boxes.change_box(r, |b| b.width = None);
                }
            }
            BoxAction::Size(r, size) => boxes.change_box(r, |b| b.size = size),
            BoxAction::Color(r, color) => boxes.change_box(r, |b| b.color = color),
            BoxAction::Delete(r) => boxes.delete(r),
            BoxAction::Finish => boxes.finish_editing(),
            BoxAction::Discard => boxes.discard_editing(),
        }
    }
}

/// Paints `galley` with its top-left corner at `at`, turned by `rotation`, and scaled by
/// `stretch`: text laid out at a rounded size while zooming (see [`text_boxes::glyph_scale`])
/// is stretched to the exact one, so it grows smoothly with the page, as the tiles do.
fn paint_text(ui: &Ui, galley: Arc<Galley>, color: Color32, at: Pos2, stretch: f32, rotation: Rotation) {
    if stretch == 1.0 {
        let angle = (rotation.degrees() as f32).to_radians();
        ui.painter().add(TextShape::new(at, galley, color).with_angle(angle));
        return;
    }
    let ctx = ui.ctx();
    let font_size = ctx.fonts(|f| f.font_image_size());
    let mut tessellator = Tessellator::new(ctx.pixels_per_point(), Default::default(), font_size, Vec::new());
    let mut mesh = Mesh::default();
    tessellator.tessellate_text(&TextShape::new(Pos2::ZERO, galley, color), &mut mesh);
    for vertex in &mut mesh.vertices {
        vertex.pos = at + turn_vec(vertex.pos.to_vec2() * stretch, rotation);
    }
    ui.painter().add(Shape::mesh(mesh));
}

/// The text field a box's text is typed into, in place of the box. Upright even on a turned
/// page, so typing reads normally. Its right-click menu sizes and colours the box, or ends
/// typing. Typing also ends once the field loses the keyboard (Escape, or a click elsewhere).
#[allow(clippy::too_many_arguments)]
fn type_into(
    ui: &mut Ui,
    editing: &mut text_boxes::Editing,
    existing: Option<&TextBox>,
    page_rect: Rect,
    rotation: Rotation,
    scale: f32,
    accent: Color32,
    doc: DocId,
) -> Typing {
    let ctx = ui.ctx().clone();
    let b = TextBox {
        size: editing.size,
        color: editing.color,
        width: editing.width,
        at: [editing.at.x, editing.at.y],
        ..existing.cloned().unwrap_or_else(|| TextBox::new(editing.at, String::new()))
    };
    let galley = b.galley(&ctx, &editing.draft, scale);
    // Room for the next letter, so the field grows ahead of the typing.
    let size = (b.size_around(&galley, scale) + vec2(if b.width.is_some() { 0.0 } else { b.size }, 0.0)) * scale;
    let anchor = page_rect.min + turn(b.at(), rotation).to_vec2() * page_rect.size();
    let area = Rect::from_min_size(anchor, size);
    let pad = (PAD * scale).round().clamp(0.0, 127.0) as i8;
    let field = TextEdit::multiline(&mut editing.draft)
        .id(ui.id().with(("typing", doc, editing.page)))
        .font(b.font(&ctx, scale))
        .text_color(b.color32())
        .frame(false)
        .margin(Margin::same(pad))
        .desired_width(size.x - 2.0 * pad as f32)
        .desired_rows(1);
    ui.painter().rect_filled(area, 0.0, Color32::from_white_alpha(140));
    let response = ui.put(area, field);
    ui.painter().rect_stroke(area, 0.0, Stroke::new(1.5, accent), StrokeKind::Outside);
    // The field is upright, so on a turned page moving its left side would move it along the
    // page's other axis: only its right side is offered there.
    let edges: &[Edge] = if rotation.degrees() == 0 { &[Edge::Left, Edge::Right] } else { &[Edge::Right] };
    let page_width = if rotation.degrees() % 180 == 90 { page_rect.height() } else { page_rect.width() } / scale;
    let mut resizing = false;
    for &edge in edges {
        let handle = edge_handle(
            ui,
            ui.id().with(("typing edge", doc, editing.page, edge)),
            anchor,
            size,
            Rotation::default(),
            edge,
        );
        paint_grip(ui, anchor, size, Rotation::default(), edge, accent);
        if handle.double_clicked() {
            editing.width = None;
        } else if handle.dragged() {
            let resized = b.resized(edge, handle.drag_delta().x / scale, b.size_around(&galley, scale).x, page_width);
            editing.at.x = resized.at[0];
            editing.width = resized.width;
        }
        // A click there takes the keyboard from the field: typing goes on all the same.
        resizing |= handle.is_pointer_button_down_on() || handle.clicked() || handle.drag_stopped();
    }
    let mut outcome = None;
    let menu = response.context_menu(|ui| {
        ui.menu_button("Size", |ui| {
            for size in text_boxes::SIZES {
                if ui.radio(editing.size == size, format!("{size} pt")).clicked() {
                    editing.size = size;
                }
            }
        });
        ui.menu_button("Colour", |ui| {
            for (name, color) in text_boxes::COLORS {
                if ui.radio(editing.color == color, name).clicked() {
                    editing.color = color;
                }
            }
        });
        ui.separator();
        if ui.button("Delete").clicked() {
            outcome = Some(Typing::Discard);
        }
        if ui.button("Done").clicked() {
            outcome = Some(Typing::Done);
        }
    });
    if let Some(outcome) = outcome {
        return outcome;
    }
    if menu.is_some() || resizing {
        // The menu or a side has the pointer, not the keyboard: typing goes on once it lets go.
        editing.focus = true;
        return Typing::Going;
    }
    if editing.focus {
        response.request_focus();
        editing.focus = false;
        return Typing::Going;
    }
    if response.has_focus() { Typing::Going } else { Typing::Done }
}

/// A box of `size` (screen points, upright) with its top-left corner at `at` (fractions of
/// the page upright), on a page shown in `page_rect` turned by `rotation`: the screen point
/// its top-left corner lands on, and the screen area it covers.
fn placed(page_rect: Rect, rotation: Rotation, at: Pos2, size: Vec2) -> (Pos2, Rect) {
    let anchor = page_rect.min + turn(at, rotation).to_vec2() * page_rect.size();
    let far = anchor + turn_vec(size, rotation);
    (anchor, Rect::from_two_pos(anchor, far))
}

/// The strip, `half` screen points either side, along `edge` of a box placed at `anchor`
/// (see [`placed`]), `size` screen points upright, turned by `rotation`; `len` long, centred
/// on the edge.
fn edge_strip(anchor: Pos2, size: Vec2, rotation: Rotation, edge: Edge, half: f32, len: f32) -> Rect {
    let x = match edge {
        Edge::Left => 0.0,
        Edge::Right => size.x,
    };
    let y = (size.y - len) / 2.0;
    Rect::from_two_pos(
        anchor + turn_vec(vec2(x - half, y), rotation),
        anchor + turn_vec(vec2(x + half, y + len), rotation),
    )
}

/// Takes the pointer along `edge` of a box (see [`edge_strip`]), to drag it. Narrow on a small
/// box, so the middle is left to move it by.
fn edge_handle(ui: &Ui, id: eframe::egui::Id, anchor: Pos2, size: Vec2, rotation: Rotation, edge: Edge) -> Response {
    let cursor = if rotation.degrees() % 180 == 0 { CursorIcon::ResizeHorizontal } else { CursorIcon::ResizeVertical };
    let strip = edge_strip(anchor, size, rotation, edge, 4.0f32.min(size.x / 4.0), size.y);
    let response = ui.interact(strip, id, Sense::click_and_drag()).on_hover_cursor(cursor);
    if response.dragged() {
        ui.ctx().set_cursor_icon(cursor);
    }
    response
}

/// Marks `edge` of a box (see [`edge_strip`]) as one to drag.
fn paint_grip(ui: &Ui, anchor: Pos2, size: Vec2, rotation: Rotation, edge: Edge, color: Color32) {
    let grip = edge_strip(anchor, size, rotation, edge, 1.5, (size.y * 0.6).min(14.0));
    ui.painter().rect_filled(grip, 1.5, color);
}

/// The inverse of [`turn_vec`].
fn unturn_vec(v: Vec2, rotation: Rotation) -> Vec2 {
    match rotation.degrees() {
        90 => vec2(v.y, -v.x),
        180 => -v,
        270 => vec2(-v.y, v.x),
        _ => v,
    }
}

/// `v` turned by `rotation`, clockwise on screen.
fn turn_vec(v: Vec2, rotation: Rotation) -> Vec2 {
    match rotation.degrees() {
        90 => vec2(-v.y, v.x),
        180 => -v,
        270 => vec2(v.y, -v.x),
        _ => v,
    }
}

/// Takes the pointer's part in selecting text: asks for the layer of a page it moves over,
/// shows an I-beam over text, and selects by dragging, double and triple clicks, and
/// shift-clicks. `pages` are the pages laid out this frame, with their screen rects.
fn select(
    ui: &Ui,
    pointer: &eframe::egui::Response,
    text: &mut TextLayers,
    view: &mut View,
    pages: &[(u32, Rect)],
    over_link: bool,
    viewport: Rect,
) {
    let ctx = ui.ctx();
    // The page nearest a screen point, and the point as fractions of it.
    let on_page = |p: Pos2| {
        pages
            .iter()
            .min_by(|a, b| a.1.distance_sq_to_pos(p).total_cmp(&b.1.distance_sq_to_pos(p)))
            .map(|&(page, rect)| (page, ((p - rect.min) / rect.size()).to_pos2()))
    };
    let (hover, moved, shift) = ui.input(|i| (i.pointer.hover_pos(), i.pointer.is_moving(), i.modifiers.shift));
    let hover = hover.filter(|p| viewport.contains(*p)).and_then(on_page);
    // Only a pointer moving over a page reads its text; scrolling past doesn't.
    if let Some((page, _)) = hover
        && (moved || pointer.is_pointer_button_down_on())
    {
        text.want(ctx, page);
    }
    let over_text = hover.is_some_and(|(page, p)| text.layer(page).is_some_and(|l| l.is_over_text(p)));
    if text.dragging() || (over_text && !over_link) {
        ctx.set_cursor_icon(CursorIcon::Text);
    }

    let at = pointer.interact_pointer_pos().and_then(on_page);
    if let Some(at) = at {
        if pointer.triple_clicked() {
            text.select(ctx, at, Unit::Line);
        } else if pointer.double_clicked() {
            text.select(ctx, at, Unit::Word);
        } else if pointer.clicked() && shift {
            text.extend(ctx, at);
        } else if pointer.clicked() {
            text.clear_selection();
        }
    }
    if pointer.drag_started()
        && let Some(anchor) = ui.input(|i| i.pointer.press_origin()).and_then(on_page)
    {
        text.begin(ctx, anchor, Unit::Char);
    }
    if pointer.dragged()
        && let Some(pos) = pointer.interact_pointer_pos()
    {
        if let Some(focus) = on_page(pos) {
            text.update(ctx, focus);
        }
        // Past the top or bottom edge, the pages scroll, faster the farther out.
        let past = (pos.y - viewport.bottom()).max(0.0) + (pos.y - viewport.top()).min(0.0);
        if past != 0.0 {
            let dt = ui.input(|i| i.stable_dt).min(0.1);
            view.scroll_by(vec2(0.0, past.clamp(-EDGE_SCROLL_MAX, EDGE_SCROLL_MAX) * EDGE_SCROLL * dt));
            ctx.request_repaint();
        }
    }
    if pointer.drag_stopped() {
        text.finish();
    }
}

/// Fill color of selected text.
fn selection_color() -> Color32 {
    Color32::from_rgba_unmultiplied(64, 140, 255, 80)
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

    #[test]
    fn unequal_spread_heights_preserve_scroll_position() {
        for sizes in [vec![vec2(612.0, 100.0), vec2(612.0, 792.0)], vec![vec2(612.0, 792.0), vec2(612.0, 100.0)]] {
            let layout = DocLayout::new(sizes, PageLayout::TwoPages);
            for offset in [0.0, 100.0, 346.0, 500.0] {
                let mut view = View { zoom: 1.0, offset: vec2(0.0, offset), ..View::default() };
                let place = view.place(&layout);
                view.go_to_place(&layout, place.page, place.y);
                assert!((view.pending_offset.unwrap().y - offset).abs() < 0.01, "offset {offset}");
            }
        }
    }

    #[test]
    fn changing_layout_keeps_a_navigated_right_page() {
        let mut layout = DocLayout::new(vec![vec2(612.0, 792.0); 6], PageLayout::TwoPages);
        let mut view =
            View { zoom: 1.0, viewport: Rect::from_min_size(Pos2::ZERO, vec2(1000.0, 800.0)), ..View::default() };
        view.go_to_page(&layout, 3);
        view.set_layout(&mut layout, PageLayout::Single, 0.0);
        assert_eq!(view.landed_page, Some(3));
    }
}
