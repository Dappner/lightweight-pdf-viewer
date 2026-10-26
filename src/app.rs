//! The app: its windows (see [`crate::window`]) and what they share.
//!
//! eframe creates the main window; the others are deferred egui viewports, which eframe
//! repaints on their own. Each window's frame runs [`State::pass`] with the state locked, so
//! the app does the same work whichever window is drawing. Closing the main window would
//! quit, so when other windows are open it takes over one of them instead: it moves to where
//! that window was and shows its tabs.

use eframe::egui::ahash::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use eframe::egui::{
    ColorImage, Context, IconData, Pos2, TextureOptions, Vec2, ViewportBuilder, ViewportCommand, ViewportId, vec2,
};

use crate::annotate;
use crate::cache::TextureCache;
use crate::config::{Config, SavedWindow};
use crate::finder;
use crate::grey::{GreyTexture, Texture};
use crate::history::{self, History};
use crate::layout::{DocLayout, PageLayout};
use crate::memory::Memory;
use crate::menu;
use crate::renderer::{DocId, Key, Output, Pixels, Renderer};
use crate::search_index::Limits;
use crate::sidebar::{self, Mode as SidebarMode};
use crate::tab::DocState;
use crate::tab_bar;
use crate::view::{MAX_ZOOM, MIN_ZOOM, Wants, fit_width_zoom};
use crate::window::{Request, Shared, Window, snapshot_texture};

const DEFAULT_SIZE: Vec2 = vec2(960.0, 1080.0);
/// How far a new window is offset from the one it was opened from.
const CASCADE: Vec2 = vec2(28.0, 28.0);
/// Screen buffers each window keeps (see `desired_maximum_frame_latency` in `main.rs`), each
/// the window's size in pixels at 4 bytes a pixel. `footprint` counts them as IOSurface.
const WINDOW_BUFFERS: f32 = 2.0;

/// The app, shared with the other windows' frame callbacks.
pub struct App {
    state: Arc<Mutex<State>>,
}

pub struct State {
    shared: Shared,
    /// The main window first.
    windows: Vec<Window>,
    /// The window that menu commands and files opened from Finder go to.
    focused: usize,
    next_serial: u64,
    /// Quitting closes the main window, which otherwise hands over to another window.
    quitting: bool,
    /// Texture budget per window.
    texture_budget: usize,
    /// What each window wanted rendered in its latest frame, by serial.
    wants: HashMap<u64, Wants>,
    /// Tiles and thumbnails received from the render thread, and how many of them were
    /// thrown away on arrival (their tab had gone to the background).
    rendered: u64,
    discarded: u64,
}

/// Actions reachable from the menu bar and keyboard shortcuts.
#[derive(Clone, Copy, Debug)]
pub enum Command {
    Open,
    /// Writes the active tab's text boxes into its PDF.
    Save,
    /// Drops the active tab's edits not yet saved.
    Revert,
    /// Shows the print sheet for the active tab, edits not yet saved included.
    Print,
    /// Shows or hides the recent-files palette.
    QuickOpen,
    NewWindow,
    CloseTab,
    NextTab,
    PreviousTab,
    SelectTab(usize),
    MoveTabToNewWindow,
    MergeAllWindows,
    ToggleSidebar,
    /// Switches the sidebar between page thumbnails and the outline.
    ToggleOutline,
    /// Shows the sidebar with thumbnails or the outline.
    ShowSidebar(SidebarMode),
    /// Lays out the active tab's pages one per row or as spreads.
    SetLayout(PageLayout),
    ZoomIn,
    ZoomOut,
    ZoomToFit,
    /// Turns every page of the active tab a quarter turn counterclockwise.
    RotateLeft,
    /// Turns every page of the active tab a quarter turn clockwise.
    RotateRight,
    /// Turns one page of the active tab a quarter turn.
    RotatePage {
        page: usize,
        clockwise: bool,
    },
    /// Opens the find bar, or focuses it.
    Find,
    FindNext,
    FindPrevious,
    /// Deletes the page text searches saved to disk.
    ClearSearchCache,
    /// Shows or hides the settings.
    Settings,
    SetMemory(Memory),
    SetRestoreTabs(bool),
    SetSearchCache(Limits),
    /// Shows or hides the annotate bar.
    ToggleAnnotate,
    /// Sets what new highlights and text boxes look like.
    SetAnnotateStyle(annotate::Style),
    Quit,
}

/// A snapshot of app state for benchmarks, describing the focused window.
pub struct Stats {
    pub windows: usize,
    pub tabs: usize,
    pub active: usize,
    /// The active tab has loaded and every visible tile is sharp.
    pub ready: bool,
    /// Every window showing a tab is `ready`.
    pub windows_ready: bool,
    pub snapshot_shown: bool,
    pub current_page: usize,
    pub page_count: usize,
    /// How far the active tab's current page is turned clockwise, in degrees.
    pub rotation: i32,
    pub texture_bytes: usize,
    /// Compressed snapshots held by background tabs.
    pub snapshot_bytes: usize,
    /// What those snapshots' buffers have allocated, which can exceed `snapshot_bytes`.
    pub snapshot_capacity: usize,
    /// The active tab's find bar status, while the bar is open ("" before a query).
    pub find: Option<String>,
    /// The active tab's search: its reads of page text from the disk index, and with PDFKit.
    pub find_reads: (u64, u64),
    /// The active tab's search has counted every page for the find field's text.
    pub find_done: bool,
    /// Searches with a worker attached, in every window and tab.
    pub search_workers: usize,
    /// Bytes of text layers (for selecting) the active tab holds.
    pub text_bytes: usize,
    /// Tabs with a text layer worker attached, in every window.
    pub text_workers: usize,
    /// Tiles and thumbnails rendered so far: each costs the render thread a page draw.
    pub rendered: u64,
    /// Of those, the ones thrown away on arrival: wasted draws.
    pub discarded: u64,
}

/// How every window is set up: a tab strip in place of the title bar.
pub fn window_builder() -> ViewportBuilder {
    ViewportBuilder::default()
        .with_title("Lightweight PDF")
        // An empty icon keeps the bundle's icon; otherwise eframe swaps in egui's logo.
        .with_icon(IconData::default())
        .with_inner_size(DEFAULT_SIZE)
        .with_min_inner_size([480.0, 360.0])
        .with_fullsize_content_view(true)
        .with_titlebar_shown(false)
        .with_title_shown(false)
        .with_drag_and_drop(true)
}

/// A window whose top-left corner is at `position` on screen, with content `size` big.
pub fn window_at(position: Pos2, size: Vec2) -> ViewportBuilder {
    window_builder().with_position(position).with_inner_size(size)
}

impl App {
    pub fn new(ctx: &Context, config: Config, history: History, files: Vec<PathBuf>) -> Self {
        Self { state: Arc::new(Mutex::new(State::new(ctx, config, history, files))) }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    /// Opens `path` in the focused window and records it in the history.
    pub fn open(&mut self, ctx: &Context, path: PathBuf) {
        self.state().open(ctx, path);
    }

    /// Runs `command` in window `w`.
    pub fn run(&mut self, ctx: &Context, w: usize, command: Command) {
        self.state().run(ctx, w, command);
    }

    pub fn stats(&self) -> Stats {
        self.state().stats()
    }

    /// One frame of the main window, which also keeps the other windows open. Separate from
    /// `eframe::App::update` so it can run headless.
    pub fn ui(&mut self, ctx: &Context) {
        let others = {
            let mut state = self.state();
            state.pass(ctx, 0);
            state.windows[1..].iter().map(|w| (w.serial, w.viewport, w.builder.clone())).collect::<Vec<_>>()
        };
        // Unlocked: without native windows (headless), egui runs these right away.
        for (serial, viewport, builder) in others {
            let state = self.state.clone();
            ctx.show_viewport_deferred(viewport, builder, move |ctx, _| {
                let mut state = state.lock().unwrap();
                if let Some(w) = state.index_of(serial).filter(|w| *w > 0) {
                    state.pass(ctx, w);
                }
            });
        }
    }
}

impl eframe::App for App {
    fn raw_input_hook(&mut self, ctx: &Context, raw: &mut eframe::egui::RawInput) {
        if tab_bar::is_dragging(ctx, raw.viewport_id)
            && let Some((position, down)) = crate::titlebar::drag_pointer()
        {
            tab_bar::update_drag_input(raw, position / ctx.zoom_factor(), down);
        }
    }

    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        self.ui(ctx);
    }

    fn on_exit(&mut self) {
        let state = &mut *self.state();
        for tab in state.windows.iter_mut().flat_map(|w| &mut w.tabs) {
            tab.text_boxes.finish_editing();
            tab.text_boxes.finish_saving();
            state.shared.remember(tab);
        }
        crate::text_boxes::wait_for_detached_saves();
        state.save_session(true);
    }
}

impl State {
    fn new(ctx: &Context, config: Config, history: History, files: Vec<PathBuf>) -> Self {
        // ⌘+/⌘- zoom the document, not the UI.
        ctx.options_mut(|o| o.zoom_with_keyboard = false);
        finder::set_context(ctx);
        let tuning = config.memory.tuning();
        let mut app = Self {
            shared: Shared {
                renderer: Renderer::spawn(ctx.clone(), tuning.render_threads, tuning.pages_per_open),
                cache: TextureCache::new(tuning.texture_budget),
                prefetch: tuning.prefetch,
                launched: config.memory,
                window_buffers: 0,
                failed: HashSet::default(),
                thumb_px: sidebar::thumb_width_px(config.sidebar_width, ctx.pixels_per_point()),
                config,
                history,
                next_id: 0,
            },
            windows: vec![Window::new(0, ViewportId::ROOT, window_builder())],
            focused: 0,
            next_serial: 1,
            quitting: false,
            texture_budget: tuning.texture_budget,
            wants: HashMap::default(),
            rendered: 0,
            discarded: 0,
        };
        if app.shared.config.restore_tabs {
            // Reopening a session isn't a choice to open these files, so it doesn't count
            // towards their rank in the history.
            for (i, saved) in app.shared.config.windows.clone().into_iter().enumerate() {
                let w = match i {
                    0 => 0,
                    _ => app.add_window(
                        ctx,
                        saved.frame.map_or_else(window_builder, |[x, y, w, h]| window_at(Pos2::new(x, y), vec2(w, h))),
                    ),
                };
                let active = saved.tabs.get(saved.active).cloned();
                for file in saved.tabs.into_iter().filter(|p| p.exists()) {
                    if app.find_path(&file).is_none() {
                        app.windows[w].open_tab(ctx, &mut app.shared, file);
                    }
                }
                let window = &mut app.windows[w];
                window.sidebar = saved.sidebar;
                if let Some(i) = active.and_then(|path| window.tabs.iter().position(|t| t.path == path)) {
                    window.activate(ctx, &mut app.shared, i);
                }
                if w > 0 && app.windows[w].tabs.is_empty() {
                    app.windows.pop();
                }
            }
        }
        for file in files {
            app.open(ctx, file);
        }
        app
    }

    fn add_window(&mut self, ctx: &Context, builder: ViewportBuilder) -> usize {
        let serial = self.next_serial;
        self.next_serial += 1;
        self.windows.push(Window::new(serial, ViewportId::from_hash_of(("window", serial)), builder));
        // The main window's frame is what creates the others.
        ctx.request_repaint_of(ViewportId::ROOT);
        self.windows.len() - 1
    }

    /// Redraws every window, after a change that may touch any of them.
    fn repaint_all(&self, ctx: &Context) {
        for window in &self.windows {
            ctx.request_repaint_of(window.viewport);
        }
    }

    fn index_of(&self, serial: u64) -> Option<usize> {
        self.windows.iter().position(|w| w.serial == serial)
    }

    /// The window and tab showing `path`.
    fn find_path(&self, path: &std::path::Path) -> Option<(usize, usize)> {
        self.windows
            .iter()
            .enumerate()
            .find_map(|(w, window)| window.tabs.iter().position(|t| t.path == path).map(|i| (w, i)))
    }

    /// The window and tab showing document `doc`.
    fn find_doc(&self, doc: DocId) -> Option<(usize, usize)> {
        self.windows
            .iter()
            .enumerate()
            .find_map(|(w, window)| window.tabs.iter().position(|t| t.id == doc).map(|i| (w, i)))
    }

    fn is_active(&self, doc: DocId) -> bool {
        self.windows.iter().any(|w| w.active_tab().is_some_and(|t| t.id == doc))
    }

    fn focus(&mut self, ctx: &Context, w: usize) {
        self.focused = w;
        ctx.send_viewport_cmd_to(self.windows[w].viewport, ViewportCommand::Focus);
    }

    /// Opens `path` in the focused window and records it in the history.
    pub fn open(&mut self, ctx: &Context, path: PathBuf) {
        self.open_in(ctx, self.focused, path);
    }

    /// Opens `path` in window `w`, or switches to it wherever it's already open.
    fn open_in(&mut self, ctx: &Context, w: usize, path: PathBuf) {
        let path = path.canonicalize().unwrap_or(path);
        self.shared.history.visit(&path, history::now());
        if let Some((x, i)) = self.find_path(&path) {
            self.windows[x].activate(ctx, &mut self.shared, i);
            if x != w {
                self.focus(ctx, x);
            }
            return;
        }
        self.windows[w].open_tab(ctx, &mut self.shared, path);
    }

    fn open_dialog(&mut self, ctx: &Context, w: usize) {
        #[cfg(target_os = "macos")]
        let files = rfd::FileDialog::new()
            .add_filter("PDF or image", &["pdf", "png", "jpg", "jpeg", "heic", "heif", "webp"])
            .pick_files()
            .unwrap_or_default();
        #[cfg(not(target_os = "macos"))]
        let files = crate::unsupported::pick_documents();
        for file in files {
            self.open_in(ctx, w, file);
        }
    }

    /// Where a window opened from window `w` goes: a little below and right of it.
    fn cascade(&self, w: usize) -> ViewportBuilder {
        let window = &self.windows[w];
        match (window.outer_rect, window.inner_rect) {
            (Some(outer), Some(inner)) => window_at(outer.min + CASCADE, inner.size()),
            _ => window_builder(),
        }
    }

    /// Moves tab `index` of window `from` into its own window, or into the window whose tab
    /// strip it was dropped on (`pointer` is in screen coordinates).
    fn detach(&mut self, ctx: &Context, from: usize, index: usize, pointer: Option<Pos2>) {
        if index >= self.windows[from].tabs.len() {
            return;
        }
        let target = pointer.and_then(|p| {
            (0..self.windows.len()).filter(|w| *w != from).find_map(|w| {
                let rect = self.windows[w].inner_rect?;
                let local = p - rect.min.to_vec2();
                tab_bar::contains(rect.width(), local).then(|| {
                    let window = &self.windows[w];
                    (w, tab_bar::drop_index(rect.width(), window.tabs.len(), window.tab_scroll, local.x))
                })
            })
        });
        let (to, at) = match target {
            Some(target) => target,
            // A window's only tab dropped away from other windows: just move the window.
            None if self.windows[from].tabs.len() == 1 => {
                if let Some(p) = pointer {
                    let position = p - tab_bar::NEW_WINDOW_GRAB;
                    ctx.send_viewport_cmd_to(self.windows[from].viewport, ViewportCommand::OuterPosition(position));
                }
                return;
            }
            None => {
                let builder = match (pointer, self.windows[from].inner_rect) {
                    (Some(p), Some(inner)) => window_at(p - tab_bar::NEW_WINDOW_GRAB, inner.size()),
                    _ => self.cascade(from),
                };
                (self.add_window(ctx, builder), 0)
            }
        };
        let tab = self.windows[from].take(ctx, &mut self.shared, index);
        self.windows[to].adopt(ctx, &mut self.shared, tab, at, true);
        let to_serial = self.windows[to].serial;
        if self.windows[from].tabs.is_empty() {
            self.close_window(ctx, from);
        }
        if let Some(to) = self.index_of(to_serial) {
            self.focus(ctx, to);
        }
    }

    /// Moves every tab into window `w` and closes the other windows.
    fn merge_into(&mut self, ctx: &Context, w: usize) {
        let target = self.windows[w].serial;
        let others: Vec<u64> = self.windows.iter().map(|w| w.serial).filter(|s| *s != target).collect();
        for serial in others {
            let Some(from) = self.index_of(serial) else { continue };
            while !self.windows[from].tabs.is_empty() {
                let tab = self.windows[from].take(ctx, &mut self.shared, 0);
                let to = self.index_of(target).unwrap_or(0);
                let end = self.windows[to].tabs.len();
                self.windows[to].adopt(ctx, &mut self.shared, tab, end, false);
            }
            self.close_window(ctx, from);
        }
    }

    /// Closes window `w` and its tabs. The last window quits the app.
    fn close_window(&mut self, ctx: &Context, w: usize) {
        if self.windows.len() == 1 {
            self.quitting = true;
            ctx.send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::Close);
            return;
        }
        let focused = self.windows[self.focused].serial;
        let mut closed = if w == 0 {
            // The main window can't close without quitting, so it takes over the next window.
            let mut next = self.windows.remove(1);
            next.viewport = ViewportId::ROOT;
            next.route_repaints(&self.shared);
            if let (Some(outer), Some(inner)) = (next.outer_rect, next.inner_rect) {
                ctx.send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::OuterPosition(outer.min));
                ctx.send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::InnerSize(inner.size()));
            }
            ctx.request_repaint_of(ViewportId::ROOT);
            std::mem::replace(&mut self.windows[0], next)
        } else {
            self.windows.remove(w)
        };
        closed.close_all(&mut self.shared);
        self.shared.cache.forget_window(closed.serial);
        self.wants.remove(&closed.serial);
        self.focused = self.index_of(focused).unwrap_or(0);
    }

    fn receive(&mut self, ctx: &Context) {
        for output in self.shared.renderer.take_done() {
            self.handle(ctx, output);
        }
        for window in &mut self.windows {
            let mut changed = false;
            for tab in &mut window.tabs {
                changed |= tab.search.as_mut().is_some_and(|s| s.receive());
                changed |= tab.text.receive();
            }
            if changed {
                ctx.request_repaint_of(window.viewport);
            }
        }
    }

    /// Takes in one result from the render thread.
    fn handle(&mut self, ctx: &Context, output: Output) {
        let doc = match &output {
            Output::Opened { doc, .. }
            | Output::Snapshot { doc, .. }
            | Output::Links { doc, .. }
            | Output::Outline { doc, .. } => *doc,
            Output::Image { key, .. } | Output::Failed(key) => key.doc(),
        };
        if let Some((w, _)) = self.find_doc(doc) {
            ctx.request_repaint_of(self.windows[w].viewport);
        }
        if matches!(output, Output::Image { .. }) {
            self.rendered += 1;
        }
        match output {
            Output::Opened { doc, result } => {
                let Some((w, i)) = self.find_doc(doc) else { return };
                let tab = &mut self.windows[w].tabs[i];
                let restore = tab.restore.take();
                tab.doc = match result {
                    Ok(sizes) => {
                        let layout = DocLayout::new(sizes, restore.map(|p| p.layout).unwrap_or_default());
                        if let Some(place) = restore
                            && place.page < layout.page_count()
                        {
                            // A zero zoom (from rotating a tab that fit its width) fits the new width.
                            tab.view.zoom = match place.zoom {
                                0.0 => fit_width_zoom(&layout, tab.view.viewport.width()),
                                zoom => zoom.clamp(MIN_ZOOM, MAX_ZOOM),
                            };
                            tab.view.render_zoom = tab.view.zoom;
                            tab.view.go_to_place(&layout, place.page, place.y);
                        }
                        DocState::Ready(layout)
                    }
                    Err(e) => DocState::Failed(e),
                };
            }
            Output::Image { key, size, pixels } if self.is_active(key.doc()) => {
                let (texture, bytes) = match pixels {
                    Pixels::Rgba(pixels) => {
                        let texture = ctx.load_texture("page", ColorImage::new(size, pixels), TextureOptions::LINEAR);
                        (Texture::Egui(texture), size[0] * size[1] * 4)
                    }
                    Pixels::Grey(pixels) => {
                        (Texture::Grey(Arc::new(GreyTexture::new(size, pixels))), size[0] * size[1])
                    }
                };
                self.shared.cache.insert(key, texture, bytes);
            }
            Output::Image { .. } => self.discarded += 1,
            Output::Failed(key) => {
                self.shared.failed.insert(key);
                // The render thread reopens the file by its path, which fails once it is gone.
                let Some((w, i)) = self.find_doc(key.doc()) else { return };
                let tab = &mut self.windows[w].tabs[i];
                if matches!(tab.doc, DocState::Ready(_)) && !tab.path.exists() {
                    tab.doc = DocState::Failed(format!("{} was moved or deleted", tab.path.display()));
                    self.shared.cache.evict_doc(key.doc());
                }
            }
            Output::Snapshot { doc, snapshot } => {
                let Some((w, i)) = self.find_doc(doc) else { return };
                // A failed replacement must not erase the last usable snapshot. A result
                // may arrive after switching back; show it until the visible tiles are ready.
                if let Some(snapshot) = snapshot {
                    if self.windows[w].active == i && !self.windows[w].visible_ready {
                        let tab = &mut self.windows[w].tabs[i];
                        if let Some(texture) = snapshot_texture(ctx, &snapshot) {
                            tab.snapshot_texture = Some(texture);
                        }
                    }
                    self.windows[w].tabs[i].snapshot = Some(snapshot);
                }
            }
            Output::Links { doc, page, links, text_boxes, highlights } => {
                if let Some((w, i)) = self.find_doc(doc)
                    && self.windows[w].active == i
                {
                    let tab = &mut self.windows[w].tabs[i];
                    tab.links.insert(page, links);
                    tab.text_boxes.set_file(page, text_boxes, highlights);
                }
            }
            Output::Outline { doc, entries } => {
                if let Some((w, i)) = self.find_doc(doc) {
                    self.windows[w].tabs[i].outline = Some(entries);
                }
            }
        }
    }

    /// Runs `command` in window `w`.
    pub fn run(&mut self, ctx: &Context, w: usize, command: Command) {
        match command {
            Command::Open => self.open_dialog(ctx, w),
            Command::NewWindow => {
                let builder = self.cascade(w);
                self.focused = self.add_window(ctx, builder);
            }
            Command::MoveTabToNewWindow if self.windows[w].tabs.len() > 1 => {
                self.detach(ctx, w, self.windows[w].active, None);
            }
            Command::MergeAllWindows => self.merge_into(ctx, w),
            Command::CloseTab if self.windows[w].tabs.is_empty() => self.close_window(ctx, w),
            Command::ClearSearchCache => {
                if let Some(store) = self.shared.config.search_store() {
                    store.clear();
                }
            }
            // What a preset sets in the render threads and the allocator waits for a relaunch.
            Command::SetMemory(memory) => {
                let tuning = memory.tuning();
                self.texture_budget = tuning.texture_budget;
                self.shared.prefetch = tuning.prefetch;
                self.shared.config.memory = memory;
                self.shared.config.save();
            }
            Command::SetRestoreTabs(restore) => {
                self.shared.config.restore_tabs = restore;
                self.shared.config.save();
            }
            // Searches already open keep the limits they started with.
            Command::SetSearchCache(limits) => {
                self.shared.config.search_cache = limits;
                self.shared.config.save();
            }
            Command::SetAnnotateStyle(style) => {
                self.shared.config.annotate = style;
                self.shared.config.save();
            }
            Command::Quit => {
                self.quitting = true;
                ctx.send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::Close);
            }
            _ => self.windows[w].run(ctx, &mut self.shared, command),
        }
    }

    /// Records the windows and their tabs in the config, so the next launch can reopen
    /// them. Moving or resizing a window alone doesn't save, unless `always`.
    fn save_session(&mut self, always: bool) {
        let windows: Vec<SavedWindow> = self
            .windows
            .iter()
            .filter_map(|window| {
                let tabs: Vec<PathBuf> = window
                    .tabs
                    .iter()
                    .filter(|t| !matches!(t.doc, DocState::Failed(_)))
                    .map(|t| t.path.clone())
                    .collect();
                if tabs.is_empty() {
                    return None;
                }
                let active = window.active_tab().and_then(|a| tabs.iter().position(|p| *p == a.path)).unwrap_or(0);
                let frame = match (window.outer_rect, window.inner_rect) {
                    (Some(outer), Some(inner)) => Some([outer.min.x, outer.min.y, inner.width(), inner.height()]),
                    _ => None,
                };
                Some(SavedWindow { tabs, active, sidebar: window.sidebar, frame })
            })
            .collect();
        let saved = &self.shared.config.windows;
        let changed = windows.len() != saved.len()
            || windows
                .iter()
                .zip(saved)
                .any(|(a, b)| a.tabs != b.tabs || a.active != b.active || a.sidebar != b.sidebar);
        if always || changed {
            self.shared.config.windows = windows;
            self.shared.config.save();
        }
    }

    pub fn stats(&self) -> Stats {
        let window = &self.windows[self.focused];
        let active = window.active_tab();
        Stats {
            windows: self.windows.len(),
            tabs: window.tabs.len(),
            active: window.active,
            ready: window.visible_ready,
            windows_ready: self.windows.iter().filter(|w| !w.tabs.is_empty()).all(|w| w.visible_ready),
            snapshot_shown: active.is_some_and(|t| t.snapshot_texture.is_some()),
            current_page: active.map_or(0, |t| t.view.current_page),
            page_count: active.map_or(0, |t| match &t.doc {
                DocState::Ready(layout) => layout.page_count(),
                _ => 0,
            }),
            rotation: active.map_or(0, |t| t.rotations.of(t.view.current_page as u32).degrees()),
            texture_bytes: self.shared.cache.bytes(),
            snapshot_bytes: self
                .windows
                .iter()
                .flat_map(|w| &w.tabs)
                .filter_map(|t| t.snapshot.as_ref())
                .map(|s| s.qoi.len())
                .sum(),
            snapshot_capacity: self
                .windows
                .iter()
                .flat_map(|w| &w.tabs)
                .filter_map(|t| t.snapshot.as_ref())
                .map(|s| s.qoi.capacity())
                .sum(),
            find: active.and_then(|t| t.search.as_ref()).map(|s| s.status().unwrap_or_default()),
            find_reads: active.and_then(|t| t.search.as_ref()).map_or((0, 0), |s| {
                let stats = s.stats();
                (stats.loaded, stats.extracted)
            }),
            find_done: active.and_then(|t| t.search.as_ref()).is_some_and(|s| s.done()),
            search_workers: self
                .windows
                .iter()
                .flat_map(|w| &w.tabs)
                .filter_map(|t| t.search.as_ref())
                .filter(|s| s.stats().worker)
                .count(),
            text_bytes: active.map_or(0, |t| t.text.bytes()),
            text_workers: self.windows.iter().flat_map(|w| &w.tabs).filter(|t| t.text.has_worker()).count(),
            rendered: self.rendered,
            discarded: self.discarded,
        }
    }
}

impl State {
    /// One frame of window `w`: the work shared by all windows, then the window itself.
    fn pass(&mut self, ctx: &Context, w: usize) {
        self.receive(ctx);
        self.windows[w].expire_parked(ctx, &mut self.shared);
        let serial = self.windows[w].serial;
        self.shared.cache.begin_frame(serial);

        let opened = finder::take();
        let commands = menu::take();
        let external = !opened.is_empty() || !commands.is_empty();
        for path in opened {
            self.open(ctx, path);
        }
        for command in commands {
            self.run(ctx, self.focused, command);
        }
        if w == 0 && ctx.input(|i| i.viewport().close_requested()) && !self.quitting && self.windows.len() > 1 {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            self.close_window(ctx, 0);
        }
        let Some(w) = self.index_of(serial) else { return };

        let ppp = ctx.pixels_per_point();
        self.shared.window_buffers = self
            .windows
            .iter()
            .filter_map(|w| w.inner_rect)
            .map(|r| (r.area() * ppp * ppp * 4.0 * WINDOW_BUFFERS) as u64)
            .sum();

        let mut wants = Wants::default();
        let requests = self.windows[w].ui(ctx, &mut self.shared, &mut wants);
        self.wants.insert(serial, wants);
        let (focused, close) = ctx.input(|i| (i.viewport().focused == Some(true), i.viewport().close_requested()));
        if focused {
            self.focused = w;
        }
        menu::set_layout(self.windows[self.focused].layout());
        menu::set_annotating(self.windows[self.focused].annotating());
        menu::set_unsaved(self.windows[self.focused].unsaved());
        menu::set_tabs(self.windows[self.focused].tabs.len());

        let changed = external || close || !requests.is_empty();
        for request in requests {
            let Some(w) = self.index_of(serial) else { break };
            match request {
                Request::Run(command) => self.run(ctx, w, command),
                Request::Open(paths) => {
                    for path in paths {
                        self.open_in(ctx, w, path);
                    }
                }
                Request::OpenDialog => self.open_dialog(ctx, w),
                Request::Detach { index, pointer } => self.detach(ctx, w, index, Some(pointer)),
            }
        }
        if close
            && w > 0
            && let Some(w) = self.index_of(serial)
        {
            self.close_window(ctx, w);
        }
        if changed {
            self.repaint_all(ctx);
        }

        self.save_session(false);
        self.request_renders();
    }

    /// Hands the render thread what every window wants (see [`Self::render_queue`]) and
    /// trims the texture cache.
    fn request_renders(&mut self) {
        let queue = self.render_queue();
        let showing = self.windows.iter().filter(|w| !w.tabs.is_empty()).count().max(1);
        self.shared.renderer.set_open_limit(showing);
        self.shared.renderer.set_wanted(queue);

        // A parked tab that has lost tiles to the budget would come back with holes, so it
        // gives up the rest and shows its snapshot when switched back to instead.
        self.shared.cache.set_budget(self.texture_budget * showing);
        let awaited: HashSet<Key> = self.wants.values().flat_map(|w| w.visible.iter().copied()).collect();
        let evicted = self.shared.cache.trim(&awaited);
        for window in &mut self.windows {
            if let Some((parked, _)) = window.parked
                && evicted.contains(&parked)
            {
                window.release_parked(&mut self.shared);
            }
        }
    }

    /// What every window wants rendered, the focused window's pages first. Windows that
    /// haven't drawn since their wants were met still list them, so what's already rendered
    /// is left out. Every open window keeps its current pages even when unfocused.
    fn render_queue(&self) -> Vec<Key> {
        let focused = self.windows[self.focused].serial;
        let mut order: Vec<u64> = self.windows.iter().map(|w| w.serial).collect();
        order.sort_by_key(|serial| *serial != focused);
        let mut merged = Wants::default();
        for serial in &order {
            if let Some(w) = self.wants.get(serial) {
                merged.visible.extend(&w.visible);
                merged.thumbs.extend(&w.thumbs);
                merged.prefetch.extend(&w.prefetch);
            }
        }
        let mut queue = merged.into_queue();
        queue.retain(|key| {
            !self.shared.cache.contains(key)
                && !self.shared.failed.contains(key)
                && !matches!(*key, Key::Links { doc, page } if self.find_doc(doc).is_some_and(|(w, i)| {
                    self.windows[w].tabs[i].links.contains_key(&page)
                }))
        });
        queue
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::renderer::Snapshot;
    use eframe::egui;

    /// An app with one tab (whose file doesn't exist, so it never loads) and a page tile of it.
    fn app() -> (Context, State, DocId) {
        let ctx = Context::default();
        let mut state = State::new(&ctx, Config::default(), History::default(), Vec::new());
        state.open(&ctx, "/nonexistent/a.pdf".into());
        let doc = state.windows[0].tabs[0].id;
        let texture = ctx.load_texture("t", ColorImage::filled([1, 1], Default::default()), TextureOptions::LINEAR);
        state.shared.cache.insert(tile(doc), Texture::Egui(texture), 4);
        (ctx, state, doc)
    }

    fn tile(doc: DocId) -> Key {
        Key::Tile { doc, page: 0, scale: 100, tx: 0, ty: 0 }
    }

    #[test]
    fn tabs_rejoin_another_window_after_the_source_loses_its_pointer() {
        use egui::{Event, Modifiers, PointerButton, RawInput, Rect, ViewportInfo};
        // Exercise closing/handover of the main window as well as closing a child window.
        for source in [0, 1] {
            let (ctx, mut state, _) = app();
            state.open(&ctx, "/nonexistent/b.pdf".into());
            state.detach(&ctx, 0, 1, None);
            let rects = [
                Rect::from_min_size(egui::pos2(20.0, 40.0), vec2(1000.0, 800.0)),
                Rect::from_min_size(egui::pos2(1100.0, 40.0), vec2(1000.0, 800.0)),
            ];
            for (window, rect) in state.windows.iter_mut().zip(rects) {
                window.inner_rect = Some(rect);
                window.outer_rect = Some(rect);
            }
            let viewport = state.windows[source].viewport;
            let viewports: egui::ViewportIdMap<ViewportInfo> = state
                .windows
                .iter()
                .zip(rects)
                .map(|(w, rect)| {
                    (
                        w.viewport,
                        ViewportInfo {
                            inner_rect: Some(rect),
                            outer_rect: Some(rect),
                            native_pixels_per_point: Some(1.0),
                            ..Default::default()
                        },
                    )
                })
                .collect();
            let raw = |events| RawInput {
                viewport_id: viewport,
                viewports: viewports.clone(),
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, rects[source].size())),
                events,
                ..Default::default()
            };
            let button = |pressed| Event::PointerButton {
                pos: egui::pos2(230.0, 16.0),
                button: PointerButton::Primary,
                pressed,
                modifiers: Modifiers::NONE,
            };
            let _ = ctx.run(raw(vec![]), |ctx| state.pass(ctx, source));
            let _ = ctx.run(raw(vec![button(true)]), |ctx| state.pass(ctx, source));
            let _ = ctx.run(raw(vec![Event::PointerMoved(egui::pos2(280.0, 16.0))]), |ctx| state.pass(ctx, source));
            assert!(tab_bar::is_dragging(&ctx, viewport));
            let _ = ctx.run(raw(vec![Event::PointerGone]), |ctx| state.pass(ctx, source));
            assert!(tab_bar::is_dragging(&ctx, viewport), "leaving the source stopped native tracking");

            // The OS reports the cursor over the other strip, with the button up;
            // there is no PointerButton release event from the source window.
            let mut released = raw(vec![]);
            tab_bar::update_drag_input(&mut released, rects[1 - source].min + vec2(230.0, 16.0), false);
            let _ = ctx.run(released, |ctx| state.pass(ctx, source));
            assert_eq!(state.windows.len(), 1, "source {source} was not closed after the drop");
            assert_eq!(state.windows[0].tabs.len(), 2, "the tab did not join the destination");
            assert!(!tab_bar::is_dragging(&ctx, viewport));
        }
    }

    #[test]
    fn sidebar_visibility_is_saved_and_restored_per_window() {
        let dir = std::env::temp_dir().join(format!("lightweight-pdf-window-sidebar-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let paths = [dir.join("a.pdf"), dir.join("b.pdf")];
        for path in &paths {
            std::fs::write(path, b"%PDF-1.4").unwrap();
        }
        let config_path = dir.join("config.json");
        // Existing sessions without a sidebar field retain the previous expanded default.
        let legacy: SavedWindow = serde_json::from_str(r#"{"tabs":[],"active":0}"#).unwrap();
        assert!(legacy.sidebar);

        let ctx = Context::default();
        let mut state = State::new(&ctx, Config::load_from(config_path.clone()), History::default(), paths.into());
        state.run(&ctx, 0, Command::MoveTabToNewWindow);
        state.save_session(false);
        // Only visibility changes: saving must detect it even when the tabs stay put.
        state.run(&ctx, 0, Command::ToggleSidebar);
        state.save_session(false);
        let config = Config::load_from(config_path.clone());
        assert_eq!(config.windows.iter().map(|w| w.sidebar).collect::<Vec<_>>(), [false, true]);

        let mut restored = State::new(&ctx, config, History::default(), Vec::new());
        assert_eq!(restored.windows.iter().map(|w| w.sidebar).collect::<Vec<_>>(), [false, true]);
        restored.run(&ctx, 0, Command::ToggleSidebar);
        restored.run(&ctx, 1, Command::ToggleSidebar);
        restored.save_session(false);
        let config = Config::load_from(config_path);
        assert_eq!(config.windows.iter().map(|w| w.sidebar).collect::<Vec<_>>(), [true, false]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn closing_or_quitting_commits_annotation_drafts() {
        use crate::text_boxes::{BoxRef, TextBox, TextBoxes};
        for action in ["tab", "window", "quit", "switch"] {
            let dir = std::env::temp_dir().join(format!("lightweight-pdf-draft-{}-{action}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let pdf = dir.join("a.pdf");
            std::fs::write(&pdf, b"%PDF-1.4").unwrap();
            let pdf = pdf.canonicalize().unwrap();
            let ctx = Context::default();
            let mut config = Config::default();
            config.text_boxes_dir = Some(dir.clone());
            let mut app = App::new(&ctx, config, History::default(), vec![pdf.clone()]);
            {
                let mut state = app.state();
                let boxes = &mut state.windows[0].tabs[0].text_boxes;
                let existing = TextBox {
                    origin: Some(0),
                    changed: false,
                    ..TextBox::new(Pos2::new(0.25, 0.5), "Original".into())
                };
                boxes.set_file(0, vec![existing], Vec::new());
                boxes.edit(BoxRef { page: 0, index: 0 });
                boxes.editing.as_mut().unwrap().draft = "Edited draft".into();
                match action {
                    "tab" => state.run(&ctx, 0, Command::CloseTab),
                    "window" => {
                        state.add_window(&ctx, window_builder());
                        state.close_window(&ctx, 0);
                    }
                    "switch" => state.open(&ctx, dir.join("b.pdf")),
                    _ => {}
                }
            }
            if action == "quit" {
                eframe::App::on_exit(&mut app);
            }
            let restored = TextBoxes::load(&pdf, Some(&dir));
            assert_eq!(restored.page(0).unwrap()[0].text, "Edited draft", "{action} lost the draft");
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn texture_eviction_stops_the_parked_search() {
        let (ctx, mut state, doc) = app();
        let tab = &mut state.windows[0].tabs[0];
        tab.search = Some(crate::search::Search::new(&ctx, tab.path.clone(), tab.rotations.clone(), 1, 0, None));
        let worker = tab.search.as_ref().unwrap().probe();
        state.open(&ctx, "/nonexistent/b.pdf".into());
        assert_eq!(state.windows[0].parked.map(|p| p.0), Some(doc));
        assert_eq!(state.stats().search_workers, 1);
        state.texture_budget = 0;
        state.request_renders();
        assert!(state.windows[0].parked.is_none());
        assert_eq!(state.stats().search_workers, 0);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while worker.running() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(!worker.running(), "the detached search thread did not exit");
    }

    #[test]
    fn a_background_search_resumes_in_its_new_window() {
        let (ctx, mut state, _) = app();
        let tab = &mut state.windows[0].tabs[0];
        tab.search = Some(crate::search::Search::new(&ctx, tab.path.clone(), tab.rotations.clone(), 1, 0, None));
        state.open(&ctx, "/nonexistent/b.pdf".into());
        state.detach(&ctx, 0, 0, None);
        assert_eq!(state.windows.len(), 2);
        let search = state.windows[1].tabs[0].search.as_mut().unwrap();
        search.query = "needle".into();
        search.edited(0.0);
        let _ = ctx.run(eframe::egui::RawInput { time: Some(1.0), ..Default::default() }, |ctx| {
            search.update(ctx, (0, 0.0), 0);
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !search.done() && std::time::Instant::now() < deadline {
            search.receive();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(search.done(), "adoption left the search paused");
    }

    fn snapshot() -> Snapshot {
        Snapshot { size: [1, 1], qoi: qoi::encode_to_vec([255u8; 4], 1, 1).unwrap() }
    }

    #[test]
    fn late_snapshot_covers_a_tab_switched_back_before_rendering() {
        let (ctx, mut state, doc) = app();
        state.windows[0].visible_ready = true;
        state.open(&ctx, "/nonexistent/b.pdf".into());
        state.windows[0].activate(&ctx, &mut state.shared, 0);
        assert!(!state.windows[0].visible_ready);
        state.handle(&ctx, Output::Snapshot { doc, snapshot: Some(snapshot()) });
        assert!(state.windows[0].tabs[0].snapshot_texture.is_some());
        let texture = state.windows[0].tabs[0].snapshot_texture.as_ref().unwrap().id();
        state.handle(&ctx, Output::Snapshot { doc, snapshot: None });
        assert_eq!(state.windows[0].tabs[0].snapshot_texture.as_ref().unwrap().id(), texture);
        assert!(state.windows[0].tabs[0].snapshot.is_some());
    }

    #[test]
    fn failed_tiles_do_not_discard_the_snapshot() {
        let (ctx, mut state, doc) = app();
        let tab = &mut state.windows[0].tabs[0];
        tab.doc = DocState::Ready(DocLayout::new(vec![Vec2::splat(1000.0)], Default::default()));
        tab.view.zoom = 1.0;
        tab.view.render_zoom = 1.0;
        let raw = || egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(Pos2::ZERO, vec2(800.0, 600.0))),
            time: Some(1.0),
            ..Default::default()
        };
        let mut wants = Wants::default();
        let _ = ctx.run(raw(), |ctx| {
            state.windows[0].ui(ctx, &mut state.shared, &mut wants);
        });
        let tiles: Vec<_> = wants.visible.into_iter().filter(|k| matches!(k, Key::Tile { .. })).collect();
        assert!(!tiles.is_empty());
        state.shared.failed.extend(tiles.iter().copied());
        state.handle(&ctx, Output::Snapshot { doc, snapshot: Some(snapshot()) });
        let _ = ctx.run(raw(), |ctx| {
            state.windows[0].ui(ctx, &mut state.shared, &mut Wants::default());
        });
        assert!(!state.windows[0].visible_ready);
        assert!(state.windows[0].tabs[0].snapshot_texture.is_some(), "failed tiles erased the fallback");

        state.shared.failed.clear();
        for key in tiles {
            state.handle(&ctx, Output::Image { key, size: [1, 1], pixels: Pixels::Rgba(vec![egui::Color32::WHITE]) });
        }
        let _ = ctx.run(raw(), |ctx| {
            state.windows[0].ui(ctx, &mut state.shared, &mut Wants::default());
        });
        assert!(state.windows[0].visible_ready);
        assert!(state.windows[0].tabs[0].snapshot_texture.is_none(), "ready tiles kept the fallback alive");
        assert!(state.windows[0].tabs[0].snapshot.is_none());
    }

    #[test]
    fn deleted_file_shows_an_error_instead_of_empty_pages() {
        let (ctx, mut state, doc) = app();
        state.windows[0].tabs[0].doc = DocState::Ready(DocLayout::new(vec![Vec2::splat(100.0)], Default::default()));
        state.handle(&ctx, Output::Failed(tile(doc)));
        assert!(matches!(state.windows[0].tabs[0].doc, DocState::Failed(_)));
        assert!(!state.shared.cache.contains(&tile(doc)), "tiles were not released");
    }

    #[test]
    fn switching_back_retries_failed_tiles() {
        let (ctx, mut state, doc) = app();
        state.open(&ctx, "/nonexistent/b.pdf".into());
        state.shared.failed.insert(tile(doc));
        state.windows[0].activate(&ctx, &mut state.shared, 0);
        assert!(!state.shared.failed.contains(&tile(doc)));
    }

    #[test]
    fn memory_preset_applies_without_a_relaunch() {
        let (ctx, mut state, _) = app();
        state.run(&ctx, 0, Command::SetMemory(Memory::Low));
        let low = Memory::Low.tuning();
        assert_eq!(state.shared.config.memory, Memory::Low);
        assert_eq!(state.texture_budget, low.texture_budget);
        assert_eq!(state.shared.prefetch, low.prefetch);
        state.request_renders();
        assert_eq!(state.shared.cache.budget(), low.texture_budget);
    }
}
