use std::collections::HashSet;
use std::path::PathBuf;

use eframe::egui::{
    self, Align2, CentralPanel, ColorImage, Context, FontId, Frame, Key as KeyCode,
    Modifiers, SidePanel, TextureOptions, TopBottomPanel, ViewportCommand, vec2,
};

use crate::cache::TextureCache;
use crate::layout::DocLayout;
use crate::renderer::{DocId, Key, Output, Renderer};
use crate::sidebar;
use crate::tab::{DocState, Snapshot, Tab};
use crate::tab_bar::{self, Action};
use crate::theme::{Palette, palette};
use crate::view::{Canvas, Wants, fit_width_zoom};

/// GPU budget for rendered tiles and thumbnails, shared by every tab.
const TEXTURE_BUDGET: usize = 64 * 1024 * 1024;
const ZOOM_STEP: f32 = 1.25;
const LINE_SCROLL: f32 = 48.0;

pub struct App {
    tabs: Vec<Tab>,
    active: usize,
    next_id: DocId,
    renderer: Renderer,
    cache: TextureCache,
    failed: HashSet<Key>,
    sidebar: bool,
}

impl App {
    pub fn new(cc: &eframe::CreationContext, files: Vec<PathBuf>) -> Self {
        // ⌘+/⌘- zoom the document, not the UI.
        cc.egui_ctx.options_mut(|o| o.zoom_with_keyboard = false);
        let mut app = Self {
            tabs: Vec::new(),
            active: 0,
            next_id: 0,
            renderer: Renderer::spawn(cc.egui_ctx.clone()),
            cache: TextureCache::new(TEXTURE_BUDGET),
            failed: HashSet::new(),
            sidebar: true,
        };
        for file in files {
            app.open(&cc.egui_ctx, file);
        }
        app
    }

    fn open(&mut self, ctx: &Context, path: PathBuf) {
        let path = path.canonicalize().unwrap_or(path);
        if let Some(i) = self.tabs.iter().position(|t| t.path == path) {
            self.activate(ctx, i);
            return;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.renderer.open(id, path.clone());
        self.tabs.push(Tab::new(id, path));
        self.activate(ctx, self.tabs.len() - 1);
    }

    fn open_dialog(&mut self, ctx: &Context) {
        let files = rfd::FileDialog::new().add_filter("PDF", &["pdf"]).pick_files();
        for file in files.unwrap_or_default() {
            self.open(ctx, file);
        }
    }

    /// Switching tabs is where memory is reclaimed: the outgoing tab keeps only a small
    /// compressed snapshot, and its textures and open PDF document are released.
    fn activate(&mut self, ctx: &Context, index: usize) {
        if index != self.active
            && let Some(old) = self.tabs.get_mut(self.active)
        {
            if let DocState::Ready(layout) = &old.doc
                && old.view.zoom > 0.0
            {
                self.renderer.snapshot(old.view.snapshot_job(layout, old.id, ctx.pixels_per_point()));
            }
            self.cache.evict_doc(old.id);
            old.snapshot_texture = None;
        }
        self.active = index;

        let tab = &mut self.tabs[index];
        if let Some(snapshot) = tab.snapshot.take()
            && let Ok((_, rgba)) = qoi::decode_to_vec(&snapshot.qoi)
        {
            let [w, h] = snapshot.size.map(|v| v as usize);
            let image = ColorImage::from_rgba_premultiplied([w, h], &rgba);
            tab.snapshot_texture = Some(ctx.load_texture("snapshot", image, TextureOptions::LINEAR));
        }
    }

    fn close(&mut self, ctx: &Context, index: usize) {
        let tab = self.tabs.remove(index);
        self.renderer.forget(tab.id);
        self.cache.evict_doc(tab.id);
        self.failed.retain(|k| k.doc() != tab.id);
        if self.tabs.is_empty() {
            self.active = 0;
        } else if index < self.active {
            self.active -= 1;
        } else if index == self.active {
            // The closed tab has nothing to park, so activate its neighbour directly.
            self.active = usize::MAX;
            self.activate(ctx, index.min(self.tabs.len() - 1));
        }
    }

    fn receive(&mut self, ctx: &Context) {
        let active_id = self.tabs.get(self.active).map(|t| t.id);
        for output in self.renderer.take_done() {
            match output {
                Output::Opened { doc, result } => {
                    if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == doc) {
                        tab.doc = match result {
                            Ok(sizes) => DocState::Ready(DocLayout::new(sizes)),
                            Err(e) => DocState::Failed(e),
                        };
                    }
                }
                Output::Image { key, size, rgba } if Some(key.doc()) == active_id => {
                    let image = ColorImage::from_rgba_premultiplied(size, &rgba);
                    let texture = ctx.load_texture("page", image, TextureOptions::LINEAR);
                    self.cache.insert(key, texture, size[0] * size[1] * 4);
                }
                Output::Image { .. } => {}
                Output::Failed(key) => {
                    self.failed.insert(key);
                }
                Output::Snapshot { doc, size, qoi } if Some(doc) != active_id => {
                    if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == doc) {
                        tab.snapshot = Some(Snapshot { size, qoi });
                    }
                }
                Output::Snapshot { .. } => {}
            }
        }
    }

    fn shortcuts(&mut self, ctx: &Context) {
        let cmd = Modifiers::COMMAND;
        let (open, close, sidebar, next, prev, number) = ctx.input_mut(|i| {
            // Check the shifted variant first: consume_key ignores extra Shift.
            let prev = i.consume_key(Modifiers::CTRL | Modifiers::SHIFT, KeyCode::Tab)
                || i.consume_key(cmd | Modifiers::SHIFT, KeyCode::OpenBracket)
                || i.consume_key(cmd | Modifiers::SHIFT, KeyCode::OpenCurlyBracket)
                || i.consume_key(cmd | Modifiers::ALT, KeyCode::ArrowLeft);
            let next = i.consume_key(Modifiers::CTRL, KeyCode::Tab)
                || i.consume_key(cmd | Modifiers::SHIFT, KeyCode::CloseBracket)
                || i.consume_key(cmd | Modifiers::SHIFT, KeyCode::CloseCurlyBracket)
                || i.consume_key(cmd | Modifiers::ALT, KeyCode::ArrowRight);
            let number = [
                KeyCode::Num1, KeyCode::Num2, KeyCode::Num3, KeyCode::Num4, KeyCode::Num5,
                KeyCode::Num6, KeyCode::Num7, KeyCode::Num8, KeyCode::Num9,
            ]
            .into_iter()
            .position(|k| i.consume_key(cmd, k));
            (
                i.consume_key(cmd, KeyCode::O),
                i.consume_key(cmd, KeyCode::W),
                i.consume_key(cmd, KeyCode::B),
                next,
                prev,
                number,
            )
        });

        if open {
            self.open_dialog(ctx);
        }
        if close {
            if self.tabs.is_empty() {
                ctx.send_viewport_cmd(ViewportCommand::Close);
            } else {
                self.close(ctx, self.active);
            }
        }
        if sidebar {
            self.sidebar = !self.sidebar;
            if !self.sidebar {
                self.cache.retain(|k| !matches!(k, Key::Thumb { .. }));
            }
        }
        let n = self.tabs.len();
        if n > 0 {
            if next {
                self.activate(ctx, (self.active + 1) % n);
            }
            if prev {
                self.activate(ctx, (self.active + n - 1) % n);
            }
            if let Some(k) = number {
                // ⌘9 always selects the last tab, like browsers.
                self.activate(ctx, if k == 8 { n - 1 } else { k.min(n - 1) });
            }
        }

        self.view_shortcuts(ctx);
    }

    fn view_shortcuts(&mut self, ctx: &Context) {
        let Some(tab) = self.tabs.get_mut(self.active) else { return };
        let DocState::Ready(layout) = &tab.doc else { return };
        let view = &mut tab.view;
        if view.zoom == 0.0 {
            return;
        }
        let cmd = Modifiers::COMMAND;
        let none = Modifiers::NONE;
        let shift = Modifiers::SHIFT;
        let now = ctx.input(|i| i.time);
        let center = view.viewport.size() / 2.0;
        let page_scroll = (view.viewport.height() - LINE_SCROLL).max(LINE_SCROLL);

        ctx.input_mut(|i| {
            if i.consume_key(cmd, KeyCode::Equals) || i.consume_key(cmd, KeyCode::Plus) {
                view.zoom_about(layout, view.zoom * ZOOM_STEP, center, now);
            }
            if i.consume_key(cmd, KeyCode::Minus) {
                view.zoom_about(layout, view.zoom / ZOOM_STEP, center, now);
            }
            if i.consume_key(cmd, KeyCode::Num0) {
                view.zoom_about(layout, fit_width_zoom(layout, view.viewport.width()), center, now);
            }
            if i.consume_key(cmd, KeyCode::ArrowUp) || i.consume_key(none, KeyCode::Home) {
                view.go_to_page(layout, 0);
            }
            if i.consume_key(cmd, KeyCode::ArrowDown) || i.consume_key(none, KeyCode::End) {
                view.go_to_page(layout, layout.page_count() - 1);
            }
            if i.consume_key(shift, KeyCode::Space) || i.consume_key(none, KeyCode::PageUp) {
                view.scroll_by(vec2(0.0, -page_scroll));
            }
            if i.consume_key(none, KeyCode::Space) || i.consume_key(none, KeyCode::PageDown) {
                view.scroll_by(vec2(0.0, page_scroll));
            }
            if i.consume_key(none, KeyCode::ArrowUp) {
                view.scroll_by(vec2(0.0, -LINE_SCROLL));
            }
            if i.consume_key(none, KeyCode::ArrowDown) {
                view.scroll_by(vec2(0.0, LINE_SCROLL));
            }
            if i.consume_key(none, KeyCode::ArrowLeft) {
                view.go_to_page(layout, view.current_page.saturating_sub(1));
            }
            if i.consume_key(none, KeyCode::ArrowRight) {
                view.go_to_page(layout, view.current_page + 1);
            }
        });
    }

    fn status(&self) -> Option<String> {
        let tab = self.tabs.get(self.active)?;
        let DocState::Ready(layout) = &tab.doc else { return None };
        Some(format!(
            "{} / {}   {:.0}%",
            tab.view.current_page + 1,
            layout.page_count(),
            tab.view.zoom * 100.0
        ))
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        self.receive(ctx);
        self.cache.begin_frame();

        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        for path in dropped {
            self.open(ctx, path);
        }
        self.shortcuts(ctx);

        let palette = palette(ctx.style().visuals.dark_mode);
        let mut wants = Wants::default();

        let bar = TopBottomPanel::top("tabs")
            .exact_height(tab_bar::HEIGHT)
            .frame(Frame::NONE)
            .show_separator_line(false)
            .show(ctx, |ui| tab_bar::show(ui, &self.tabs, self.active, self.status(), &palette))
            .inner;
        match bar {
            Some(Action::Select(i)) => self.activate(ctx, i),
            Some(Action::Close(i)) => self.close(ctx, i),
            Some(Action::Open) => self.open_dialog(ctx),
            None => {}
        }

        let content_frame = Frame::NONE.fill(palette.content);
        if self.sidebar
            && let Some(tab) = self.tabs.get_mut(self.active)
            && let DocState::Ready(layout) = &tab.doc
        {
            let clicked = SidePanel::left("sidebar")
                .exact_width(sidebar::WIDTH)
                .resizable(false)
                .frame(content_frame)
                .show(ctx, |ui| {
                    sidebar::show(ui, tab.id, layout, &mut tab.view, &mut self.cache, &self.failed, &mut wants, &palette)
                })
                .inner;
            if let Some(page) = clicked {
                tab.view.go_to_page(layout, page);
            }
        }

        CentralPanel::default().frame(content_frame).show(ctx, |ui| {
            let Some(tab) = self.tabs.get_mut(self.active) else {
                empty_state(ui, &palette, "Drop a PDF here or press ⌘O");
                return;
            };
            match &tab.doc {
                DocState::Loading => {}
                DocState::Failed(e) => empty_state(ui, &palette, e),
                DocState::Ready(layout) => Canvas {
                    doc: tab.id,
                    layout,
                    view: &mut tab.view,
                    snapshot: &mut tab.snapshot_texture,
                    cache: &mut self.cache,
                    failed: &self.failed,
                    page_border: palette.page_border,
                }
                .show(ui, &mut wants),
            }
        });

        self.renderer.set_wanted(wants.into_queue());
        self.cache.trim();
    }
}

fn empty_state(ui: &mut egui::Ui, palette: &Palette, text: &str) {
    ui.painter().text(
        ui.max_rect().center(),
        Align2::CENTER_CENTER,
        text,
        FontId::proportional(14.0),
        palette.text_weak,
    );
}
