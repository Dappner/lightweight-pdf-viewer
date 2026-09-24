//! The app: its windows (see [`crate::window`]) and what they share.
//!
//! eframe creates the main window; the others are deferred egui viewports, which eframe
//! repaints on their own. Each window's frame runs [`State::pass`] with the state locked, so
//! the app does the same work whichever window is drawing. Closing the main window would
//! quit, so when other windows are open it takes over one of them instead: it moves to where
//! that window was and shows its tabs.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use eframe::egui::{
    ColorImage, Context, IconData, Pos2, TextureOptions, Vec2, ViewportBuilder, ViewportCommand, ViewportId,
    vec2,
};

use crate::cache::TextureCache;
use crate::config::{Config, SavedWindow};
use crate::finder;
use crate::history::{self, History};
use crate::layout::{DocLayout, PageLayout};
use crate::menu;
use crate::renderer::{DocId, Key, Output, Renderer};
use crate::sidebar::{self, Mode as SidebarMode};
use crate::tab::DocState;
use crate::tab_bar;
use crate::view::{MAX_ZOOM, MIN_ZOOM, Wants};
use crate::window::{Request, Shared, Window, snapshot_texture};

const DEFAULT_SIZE: Vec2 = vec2(960.0, 1080.0);
/// How far a new window is offset from the one it was opened from.
const CASCADE: Vec2 = vec2(28.0, 28.0);

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
    sleep: Sleep,
    /// See [`crate::memory::Tuning::sleep_after`].
    sleep_after: Option<f64>,
    /// When the app was last focused, scrolled or zoomed.
    last_active: f64,
    /// Texture budget per window.
    texture_budget: usize,
    /// What each window wanted rendered in its latest frame, by serial.
    wants: HashMap<u64, Wants>,
}

/// Whether the app has let go of its page tiles while in the background.
#[derive(PartialEq, Eq, Debug)]
enum Sleep {
    Awake,
    /// Waiting for snapshots of these tabs (each window's active one) to show in place of
    /// their tiles.
    Falling(HashSet<DocId>),
    Asleep,
}

/// Actions reachable from the menu bar and keyboard shortcuts.
#[derive(Clone, Copy, Debug)]
pub enum Command {
    Open,
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
    /// Opens the find bar, or focuses it.
    Find,
    FindNext,
    FindPrevious,
    /// Deletes the page text searches saved to disk.
    ClearSearchCache,
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
    pub texture_bytes: usize,
    /// Compressed snapshots held by background tabs.
    pub snapshot_bytes: usize,
    /// What those snapshots' buffers have allocated, which can exceed `snapshot_bytes`.
    pub snapshot_capacity: usize,
    /// Page tiles were dropped while the app was in the background.
    pub asleep: bool,
    /// The active tab's find bar status, while the bar is open ("" before a query).
    pub find: Option<String>,
    /// The active tab's search: its reads of page text from the disk index, and with PDFKit.
    pub find_reads: (u64, u64),
    /// Searches with a worker attached, in every window and tab.
    pub search_workers: usize,
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
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        self.ui(ctx);
    }

    fn on_exit(&mut self) {
        let state = &mut *self.state();
        for tab in state.windows.iter().flat_map(|w| &w.tabs) {
            state.shared.remember(tab);
        }
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
                renderer: Renderer::spawn(ctx.clone(), tuning.pages_per_open),
                cache: TextureCache::new(tuning.texture_budget),
                prefetch: tuning.prefetch,
                failed: HashSet::new(),
                thumb_px: sidebar::thumb_width_px(config.sidebar_width, ctx.pixels_per_point()),
                config,
                history,
                next_id: 0,
            },
            windows: vec![Window::new(0, ViewportId::ROOT, window_builder())],
            focused: 0,
            next_serial: 1,
            quitting: false,
            sleep: Sleep::Awake,
            sleep_after: tuning.sleep_after,
            last_active: 0.0,
            texture_budget: tuning.texture_budget,
            wants: HashMap::new(),
        };
        if app.shared.config.restore_tabs {
            // Reopening a session isn't a choice to open these files, so it doesn't count
            // towards their rank in the history.
            for (i, saved) in app.shared.config.windows.clone().into_iter().enumerate() {
                let w = match i {
                    0 => 0,
                    _ => app.add_window(ctx, saved.frame.map_or_else(window_builder, |[x, y, w, h]| {
                        window_at(Pos2::new(x, y), vec2(w, h))
                    })),
                };
                let active = saved.tabs.get(saved.active).cloned();
                for file in saved.tabs.into_iter().filter(|p| p.exists()) {
                    if app.find_path(&file).is_none() {
                        app.windows[w].open_tab(ctx, &mut app.shared, file);
                    }
                }
                let window = &mut app.windows[w];
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
        self.windows.iter().enumerate().find_map(|(w, window)| {
            window.tabs.iter().position(|t| t.path == path).map(|i| (w, i))
        })
    }

    /// The window and tab showing document `doc`.
    fn find_doc(&self, doc: DocId) -> Option<(usize, usize)> {
        self.windows.iter().enumerate().find_map(|(w, window)| {
            window.tabs.iter().position(|t| t.id == doc).map(|i| (w, i))
        })
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
        let files = rfd::FileDialog::new().add_filter("PDF", &["pdf"]).pick_files();
        for file in files.unwrap_or_default() {
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
        self.windows[to].adopt(ctx, &mut self.shared, tab, at);
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
                self.windows[to].adopt(ctx, &mut self.shared, tab, end);
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
            for search in window.tabs.iter_mut().filter_map(|t| t.search.as_mut()) {
                changed |= search.receive();
            }
            if changed {
                ctx.request_repaint_of(window.viewport);
            }
        }
        self.finish_falling_asleep();
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
                            tab.view.zoom = place.zoom.clamp(MIN_ZOOM, MAX_ZOOM);
                            tab.view.render_zoom = tab.view.zoom;
                            tab.view.go_to_place(&layout, place.page, place.y);
                        }
                        DocState::Ready(layout)
                    }
                    Err(e) => DocState::Failed(e),
                };
            }
            // Tiles that were in flight when the app fell asleep.
            Output::Image { key: Key::Tile { .. }, .. } if self.sleep != Sleep::Awake => {}
            Output::Image { key, size, rgba } if self.is_active(key.doc()) => {
                let image = ColorImage::from_rgba_premultiplied(size, &rgba);
                let texture = ctx.load_texture("page", image, TextureOptions::LINEAR);
                self.shared.cache.insert(key, texture, size[0] * size[1] * 4);
            }
            Output::Image { .. } => {}
            Output::Failed(key) => {
                self.shared.failed.insert(key);
            }
            Output::Snapshot { doc, snapshot } => {
                let Some((w, i)) = self.find_doc(doc) else { return };
                if self.windows[w].active != i {
                    self.windows[w].tabs[i].snapshot = snapshot;
                } else if let Sleep::Falling(pending) = &mut self.sleep
                    && pending.remove(&doc)
                {
                    // Without a snapshot, the pages show as placeholders while asleep.
                    self.windows[w].tabs[i].snapshot_texture =
                        snapshot.and_then(|snapshot| snapshot_texture(ctx, &snapshot));
                }
            }
            Output::Links { doc, page, links } => {
                if let Some((w, i)) = self.find_doc(doc)
                    && self.windows[w].active == i
                {
                    self.windows[w].tabs[i].links.insert(page, links);
                }
            }
            Output::Outline { doc, entries } => {
                if let Some((w, i)) = self.find_doc(doc) {
                    self.windows[w].tabs[i].outline = Some(entries);
                }
            }
        }
    }

    /// Lets go of the tiles once every awaited snapshot has arrived. A tab that was closed
    /// or switched away from since is no longer awaited: its snapshot never comes, or is
    /// kept for when it's switched back to.
    fn finish_falling_asleep(&mut self) {
        let Sleep::Falling(pending) = &self.sleep else { return };
        let pending: HashSet<DocId> = pending.iter().copied().filter(|doc| self.is_active(*doc)).collect();
        if pending.is_empty() {
            self.release_tiles();
            self.sleep = Sleep::Asleep;
        } else {
            self.sleep = Sleep::Falling(pending);
        }
    }

    /// In the background, the page tiles (most of the app's GPU memory) are swapped for a
    /// snapshot of each window once the app has been left alone for a while. Focusing,
    /// scrolling or zooming wakes it; the snapshots show until the tiles are redrawn.
    fn sleep_or_wake(&mut self, ctx: &Context) {
        let Some(after) = self.sleep_after else { return };
        let now = ctx.input(|i| i.time);
        let active = self.windows.iter().any(|w| {
            ctx.input_for(w.viewport, |i| {
                i.viewport().focused.unwrap_or(true) || i.raw_scroll_delta != Vec2::ZERO || i.zoom_delta() != 1.0
            })
        });
        if active {
            self.last_active = now;
            self.sleep = Sleep::Awake;
        } else if self.sleep == Sleep::Awake {
            let idle = now - self.last_active;
            if idle < after {
                ctx.request_repaint_after(std::time::Duration::from_secs_f64(after - idle));
                return;
            }
            // The tiles go once the snapshots that replace them arrive (see `receive`).
            let mut pending = HashSet::new();
            for window in &self.windows {
                if let Some(tab) = window.active_tab()
                    && let DocState::Ready(layout) = &tab.doc
                    && tab.view.zoom > 0.0
                {
                    self.shared.remember(tab);
                    self.shared.renderer.snapshot(tab.view.snapshot_job(layout, tab.id, ctx.pixels_per_point()));
                    pending.insert(tab.id);
                }
            }
            self.sleep = Sleep::Falling(pending);
            self.finish_falling_asleep();
        }
    }

    /// Drops every page tile (thumbnails are small and stay), closes the open documents and
    /// stops the search workers (searches keep their results).
    fn release_tiles(&mut self) {
        self.shared.cache.retain(|k| !matches!(k, Key::Tile { .. }));
        for window in &mut self.windows {
            window.parked = None;
            for search in window.tabs.iter_mut().filter_map(|t| t.search.as_mut()) {
                search.sleep();
            }
        }
        self.shared.renderer.release();
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
                Some(SavedWindow { tabs, active, frame })
            })
            .collect();
        let saved = &self.shared.config.windows;
        let changed = windows.len() != saved.len()
            || windows.iter().zip(saved).any(|(a, b)| a.tabs != b.tabs || a.active != b.active);
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
            asleep: self.sleep == Sleep::Asleep,
            find: active.and_then(|t| t.search.as_ref()).map(|s| s.status().unwrap_or_default()),
            find_reads: active.and_then(|t| t.search.as_ref()).map_or((0, 0), |s| (s.stats().loaded, s.stats().extracted)),
            search_workers: self
                .windows
                .iter()
                .flat_map(|w| &w.tabs)
                .filter_map(|t| t.search.as_ref())
                .filter(|s| s.stats().worker)
                .count(),
        }
    }
}

impl State {
    /// One frame of window `w`: the work shared by all windows, then the window itself.
    fn pass(&mut self, ctx: &Context, w: usize) {
        // Before `receive`, so a snapshot that arrives as the app wakes is ignored.
        self.sleep_or_wake(ctx);
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

        let mut wants = Wants::default();
        let requests = self.windows[w].ui(ctx, &mut self.shared, &mut wants);
        self.wants.insert(serial, wants);
        let (focused, close) = ctx.input(|i| (i.viewport().focused == Some(true), i.viewport().close_requested()));
        if focused {
            self.focused = w;
        }
        menu::set_layout(self.windows[self.focused].layout());

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
                self.shared.cache.evict_doc(parked);
                window.parked = None;
            }
        }
    }

    /// What every window wants rendered, the focused window's pages first. Windows that
    /// haven't drawn since their wants were met still list them, so what's already rendered
    /// is left out. Nothing while asleep: the snapshots stand in for the pages and the
    /// thumbnails already drawn stay, and drawing anything else would reopen the documents
    /// that falling asleep closed.
    fn render_queue(&self) -> Vec<Key> {
        if self.sleep != Sleep::Awake {
            return Vec::new();
        }
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

    /// An app with one tab (whose file doesn't exist, so it never loads) and a page tile of it.
    fn app() -> (Context, State, DocId) {
        let ctx = Context::default();
        let mut state = State::new(&ctx, Config::default(), History::default(), Vec::new());
        state.open(&ctx, "/nonexistent/a.pdf".into());
        let doc = state.windows[0].tabs[0].id;
        let texture = ctx.load_texture("t", ColorImage::filled([1, 1], Default::default()), TextureOptions::LINEAR);
        state.shared.cache.insert(tile(doc), texture, 4);
        (ctx, state, doc)
    }

    fn tile(doc: DocId) -> Key {
        Key::Tile { doc, page: 0, scale: 100, tx: 0, ty: 0 }
    }

    #[test]
    fn failed_snapshot_still_completes_sleep() {
        let (ctx, mut state, doc) = app();
        state.sleep = Sleep::Falling(HashSet::from([doc]));
        state.handle(&ctx, Output::Snapshot { doc, snapshot: None });
        state.finish_falling_asleep();
        assert_eq!(state.sleep, Sleep::Asleep);
        assert!(!state.shared.cache.contains(&tile(doc)), "tiles were not released");
        assert!(state.windows[0].tabs[0].snapshot_texture.is_none());
    }

    #[test]
    fn snapshot_completes_sleep() {
        let (ctx, mut state, doc) = app();
        state.sleep = Sleep::Falling(HashSet::from([doc]));
        let qoi = qoi::encode_to_vec([255u8; 4], 1, 1).unwrap();
        state.handle(&ctx, Output::Snapshot { doc, snapshot: Some(Snapshot { size: [1, 1], qoi }) });
        state.finish_falling_asleep();
        assert_eq!(state.sleep, Sleep::Asleep);
        assert!(state.windows[0].tabs[0].snapshot_texture.is_some());
    }

    /// The render thread drops a closed tab's snapshot job, so its result never arrives.
    #[test]
    fn closing_the_awaited_tab_completes_sleep() {
        let (ctx, mut state, doc) = app();
        state.open(&ctx, "/nonexistent/b.pdf".into());
        let other = state.windows[0].tabs[1].id;
        state.sleep = Sleep::Falling(HashSet::from([other]));
        state.finish_falling_asleep();
        assert_eq!(state.sleep, Sleep::Falling(HashSet::from([other])), "fell asleep too early");
        state.windows[0].close(&ctx, &mut state.shared, 1);
        state.finish_falling_asleep();
        assert_eq!(state.sleep, Sleep::Asleep);
        assert!(!state.shared.cache.contains(&tile(doc)));
    }

    #[test]
    fn nothing_is_rendered_while_asleep() {
        let (_, mut state, doc) = app();
        let serial = state.windows[0].serial;
        let thumb = Key::Thumb { doc, page: 0, width: 100 };
        state.wants.insert(serial, Wants { thumbs: vec![thumb], ..Default::default() });
        assert_eq!(state.render_queue(), vec![thumb]);
        state.sleep = Sleep::Falling(HashSet::from([doc]));
        assert!(state.render_queue().is_empty(), "rendering while falling asleep");
        state.sleep = Sleep::Asleep;
        assert!(state.render_queue().is_empty(), "rendering while asleep");
    }
}
