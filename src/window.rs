//! One window: its tabs, sidebar, tab strip and page canvas. The render thread, texture
//! cache and saved state are shared by all windows (see [`Shared`]).

use eframe::egui::ahash::HashSet;
use std::path::PathBuf;

use eframe::egui::{
    self, Align2, CentralPanel, ColorImage, Context, FontId, Frame, Id, Key as KeyCode, Modifiers, Popup, Pos2, Rect,
    SidePanel, TextureHandle, TextureOptions, TopBottomPanel, ViewportBuilder, ViewportId, vec2,
};

use crate::annotate::{self, Tool};
use crate::app::Command;
use crate::cache::TextureCache;
use crate::config::Config;
use crate::engine;
use crate::find_bar;
use crate::history::{self, History};
use crate::layout::PageLayout;
use crate::quick_open::{Outcome, QuickOpen};
use crate::renderer::{DocId, Key, Renderer, Snapshot, Target};
use crate::search::{Anchor, Search};
use crate::settings::{self, Settings};
use crate::sidebar::{self, Mode as SidebarMode};
use crate::tab::{DocState, Tab};
use crate::tab_bar::{self, Action};
use crate::text_boxes::{self, Saving};
use crate::text_layers::TextLayers;
use crate::theme::{Palette, palette};
use crate::view::{Canvas, Wants, fit_width_zoom};

/// Seconds a tab you switched away from keeps its tiles, so flipping straight back is sharp.
const PARK_SECONDS: f64 = 3.0;
const ZOOM_STEP: f32 = 1.25;
const LINE_SCROLL: f32 = 48.0;
/// Seconds the status says a save finished.
const SAVED_SECONDS: f64 = 2.0;

/// State shared by every window.
pub struct Shared {
    pub renderer: Renderer,
    pub cache: TextureCache,
    /// Screen heights rendered ahead of the visible area (see [`crate::memory::Tuning`]).
    pub prefetch: f32,
    /// The memory preset the app launched with.
    pub launched: crate::memory::Memory,
    /// What the windows' screen buffers take, estimated from their sizes.
    pub window_buffers: u64,
    pub failed: HashSet<Key>,
    pub config: Config,
    pub history: History,
    /// Pixel width thumbnails are rendered at. Follows the sidebar's width once a resize
    /// ends, so dragging stretches the existing thumbnails instead of re-rendering them.
    pub thumb_px: u32,
    pub next_id: DocId,
}

impl Shared {
    /// Saves where `tab` is left, so reopening the file brings you back to it.
    pub fn remember(&mut self, tab: &Tab) {
        if let DocState::Ready(layout) = &tab.doc
            && tab.view.zoom > 0.0
            && layout.page_count() > 0
        {
            self.history.remember(&tab.path, tab.view.place(layout), history::now());
        }
    }

    /// Lets go of everything held for a tab that is being closed.
    fn forget(&mut self, tab: &Tab) {
        self.remember(tab);
        self.renderer.forget(tab.id);
        self.cache.evict_doc(tab.id);
        self.failed.retain(|k| k.doc() != tab.id);
    }
}

/// What a window asks the app to do after drawing a frame.
pub enum Request {
    Run(Command),
    /// Open these files, picked in the Open dialog or the quick-open palette.
    Open(Vec<PathBuf>),
    OpenDialog,
    /// Tab `index` was dragged out and dropped at `pointer`, in screen coordinates.
    Detach {
        index: usize,
        pointer: Pos2,
    },
}

pub struct Window {
    /// Keeps the window's widget ids apart from other windows'. Unlike `viewport`, it stays
    /// the same when the window becomes the main one.
    pub serial: u64,
    pub viewport: ViewportId,
    /// How to create the native window; not used for the main window, which eframe creates.
    pub builder: ViewportBuilder,
    pub tabs: Vec<Tab>,
    pub active: usize,
    /// The ⌘K recent-files palette, while it is open.
    quick_open: Option<QuickOpen>,
    /// The ⌘, settings, while they are open.
    settings: Option<Settings>,
    sidebar: bool,
    sidebar_mode: SidebarMode,
    /// The annotate bar's tool, while it's showing.
    annotate: Option<Tool>,
    pub tab_scroll: f32,
    /// The last tab switched away from, whose tiles are kept until this time.
    pub parked: Option<(DocId, f64)>,
    /// Whether every visible tile of the active tab was drawn sharp last frame.
    pub visible_ready: bool,
    /// The window's content area on screen, in points, as of its last frame.
    pub inner_rect: Option<Rect>,
    /// The whole window, title bar included.
    pub outer_rect: Option<Rect>,
}

impl Window {
    pub fn new(serial: u64, viewport: ViewportId, builder: ViewportBuilder) -> Self {
        Self {
            serial,
            viewport,
            builder,
            tabs: Vec::new(),
            active: 0,
            quick_open: None,
            settings: None,
            sidebar: true,
            sidebar_mode: SidebarMode::Pages,
            annotate: None,
            tab_scroll: 0.0,
            parked: None,
            visible_ready: false,
            inner_rect: None,
            outer_rect: None,
        }
    }

    pub fn active_tab(&self) -> Option<&Tab> {
        self.tabs.get(self.active)
    }

    /// `path` should already be canonical, as saved tabs are.
    pub fn open_tab(&mut self, ctx: &Context, shared: &mut Shared, path: PathBuf) {
        if let Some(i) = self.tabs.iter().position(|t| t.path == path) {
            self.activate(ctx, shared, i);
            return;
        }
        let id = shared.next_id;
        shared.next_id += 1;
        let rotations = shared.history.rotations(&path);
        shared.renderer.open(id, path.clone(), rotations.clone());
        let restore = shared.history.place(&path);
        self.tabs.push(Tab::new(id, path, rotations, restore, shared.config.text_boxes_dir.as_deref()));
        self.activate(ctx, shared, self.tabs.len() - 1);
    }

    /// Switching tabs is where memory is reclaimed: the outgoing tab keeps only a small
    /// compressed snapshot, and its open PDF document is released. Its textures, and its
    /// search's text, are kept for a few seconds (see [`PARK_SECONDS`]) in case you flip
    /// straight back; its search is paused meanwhile.
    pub fn activate(&mut self, ctx: &Context, shared: &mut Shared, index: usize) {
        // Returning to the parked tab within the grace period: its tiles are still cached.
        let returning = self.parked.is_some_and(|(doc, _)| doc == self.tabs[index].id);
        let mut expired = None;
        if index != self.active
            && let Some(old) = self.tabs.get_mut(self.active)
        {
            shared.remember(old);
            if let DocState::Ready(layout) = &old.doc
                && old.view.zoom > 0.0
            {
                shared.renderer.snapshot(old.view.snapshot_job(layout, old.id, ctx.pixels_per_point()));
            }
            old.snapshot_texture = None;
            old.links.clear();
            old.text_boxes.clear_file();
            old.text.clear();
            if let Some(search) = &mut old.search {
                search.pause(true);
            }
            if let Some((parked, _)) = self.parked.take()
                && !returning
            {
                shared.cache.evict_doc(parked);
                expired = Some(parked);
            }
            self.parked = Some((old.id, ctx.input(|i| i.time) + PARK_SECONDS));
        }
        if let Some(doc) = expired {
            self.suspend_search(doc);
        }
        self.active = index;

        let tab = &mut self.tabs[index];
        // Retried, in case the file was only briefly missing (an app saving by replacing it).
        shared.failed.retain(|k| k.doc() != tab.id);
        if let Some(search) = &mut tab.search {
            search.pause(false);
        }
        if returning {
            return;
        }
        if let Some(snapshot) = tab.snapshot.take() {
            tab.snapshot_texture = snapshot_texture(ctx, &snapshot);
        }
    }

    pub fn expire_parked(&mut self, ctx: &Context, shared: &mut Shared) {
        let Some((doc, until)) = self.parked else { return };
        let now = ctx.input(|i| i.time);
        if now >= until {
            shared.cache.evict_doc(doc);
            self.parked = None;
            self.suspend_search(doc);
        } else {
            ctx.request_repaint_after(std::time::Duration::from_secs_f64(until - now));
        }
    }

    /// Stops the search worker of tab `doc`, which is in the background; its results stay.
    fn suspend_search(&mut self, doc: DocId) {
        if let Some(search) = self.tabs.iter_mut().find(|t| t.id == doc).and_then(|t| t.search.as_mut()) {
            search.suspend();
        }
    }

    pub fn close(&mut self, ctx: &Context, shared: &mut Shared, index: usize) {
        let mut tab = self.take(ctx, shared, index);
        tab.text_boxes.finish_saving();
        shared.forget(&tab);
    }

    /// Closes every tab, as when the window closes.
    pub fn close_all(&mut self, shared: &mut Shared) {
        for mut tab in self.tabs.drain(..) {
            tab.text_boxes.finish_saving();
            shared.forget(&tab);
        }
        self.active = 0;
        self.parked = None;
    }

    /// Removes tab `index` without closing it, to move it to another window.
    pub fn take(&mut self, ctx: &Context, shared: &mut Shared, index: usize) -> Tab {
        let tab = self.tabs.remove(index);
        if self.parked.is_some_and(|(doc, _)| doc == tab.id) {
            self.parked = None;
        }
        if self.tabs.is_empty() {
            self.active = 0;
        } else if index < self.active {
            self.active -= 1;
        } else if index == self.active {
            // The removed tab has nothing to park, so activate its neighbour directly.
            self.active = usize::MAX;
            self.activate(ctx, shared, index.min(self.tabs.len() - 1));
        }
        tab
    }

    /// Adds a tab from another window at `index` and makes it active.
    pub fn adopt(&mut self, ctx: &Context, shared: &mut Shared, mut tab: Tab, index: usize) {
        let index = index.min(self.tabs.len());
        // The canvas's scroll state belongs to the old window; carry the position over.
        tab.view.pending_offset.get_or_insert(tab.view.offset);
        tab.view.sidebar_page = None;
        self.tabs.insert(index, tab);
        if self.tabs.len() == 1 {
            self.active = 0;
        } else {
            if index <= self.active {
                self.active += 1;
            }
            self.activate(ctx, shared, index);
        }
    }

    fn move_tab(&mut self, from: usize, to: usize) {
        let active_id = self.tabs[self.active].id;
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        self.active = self.tabs.iter().position(|t| t.id == active_id).unwrap_or(0);
    }

    /// Runs a command that only concerns this window.
    pub fn run(&mut self, ctx: &Context, shared: &mut Shared, command: Command) {
        let n = self.tabs.len();
        match command {
            Command::QuickOpen => {
                self.settings = None;
                self.toggle_quick_open(shared);
            }
            Command::Settings => {
                self.quick_open = None;
                self.settings = match self.settings {
                    Some(_) => None,
                    None => Some(Settings::default()),
                };
            }
            Command::CloseTab if n > 0 => self.close(ctx, shared, self.active),
            Command::NextTab if n > 0 => self.activate(ctx, shared, (self.active + 1) % n),
            Command::PreviousTab if n > 0 => self.activate(ctx, shared, (self.active + n - 1) % n),
            // Like browsers, the ninth tab shortcut always selects the last tab.
            Command::SelectTab(i) if n > 0 => {
                self.activate(ctx, shared, if i == 8 { n - 1 } else { i.min(n - 1) });
            }
            Command::ToggleSidebar => {
                self.sidebar = !self.sidebar;
                if !self.sidebar {
                    self.drop_thumbnails(shared);
                }
            }
            Command::ToggleOutline => self.set_sidebar_mode(
                shared,
                match self.sidebar_mode {
                    SidebarMode::Pages => SidebarMode::Outline,
                    SidebarMode::Outline => SidebarMode::Pages,
                },
            ),
            Command::ShowSidebar(mode) => self.set_sidebar_mode(shared, mode),
            Command::ToggleAnnotate => {
                self.annotate = match self.annotate {
                    Some(_) => None,
                    None => Some(Tool::Highlight),
                };
            }
            Command::SetLayout(mode) => self.set_layout(ctx, mode),
            Command::ZoomIn | Command::ZoomOut | Command::ZoomToFit => self.zoom(ctx, command),
            Command::RotateLeft => self.rotate(ctx, shared, None, false),
            Command::RotateRight => self.rotate(ctx, shared, None, true),
            Command::RotatePage { page, clockwise } => self.rotate(ctx, shared, Some(page as u32), clockwise),
            Command::Find => self.open_find(ctx, shared),
            Command::FindNext => self.find_step(true),
            Command::FindPrevious => self.find_step(false),
            Command::Save => self.save(ctx),
            Command::Revert => self.revert(self.active),
            _ => {}
        }
    }

    /// Opens the active tab's find bar, or focuses it and selects its text.
    fn open_find(&mut self, ctx: &Context, shared: &Shared) {
        let Some(tab) = self.tabs.get_mut(self.active) else { return };
        let DocState::Ready(layout) = &tab.doc else { return };
        let start = tab.view.current_page;
        let search = tab.search.get_or_insert_with(|| {
            Search::new(
                ctx,
                tab.path.clone(),
                tab.rotations.clone(),
                layout.page_count(),
                start,
                shared.config.search_store(),
            )
        });
        search.focus = true;
        search.wake();
    }

    /// The top of the active tab's viewport, where a search without a current match starts
    /// from.
    fn find_anchor(&self) -> Option<Anchor> {
        let tab = self.active_tab()?;
        let DocState::Ready(layout) = &tab.doc else { return None };
        (tab.view.zoom > 0.0).then(|| {
            let place = tab.view.place(layout);
            (place.page, place.y)
        })
    }

    fn find_step(&mut self, forward: bool) {
        let Some(anchor) = self.find_anchor() else { return };
        if let Some(search) = self.tabs.get_mut(self.active).and_then(|t| t.search.as_mut()) {
            search.step(forward, anchor);
        }
    }

    /// Runs the active tab's search for this frame: takes up edits and pending steps, and
    /// scrolls to a newly selected match.
    fn update_find(&mut self, ctx: &Context) {
        let anchor = self.find_anchor();
        let Some(tab) = self.tabs.get_mut(self.active) else { return };
        let (Some(search), DocState::Ready(layout), Some(anchor)) = (tab.search.as_mut(), &tab.doc, anchor) else {
            return;
        };
        search.update(ctx, anchor, tab.view.current_page);
        if let Some((page, area)) = search.take_reveal() {
            tab.view.reveal(layout, page, area);
        }
    }

    fn toggle_quick_open(&mut self, shared: &Shared) {
        self.quick_open = match self.quick_open {
            Some(_) => None,
            None => {
                // The file you're looking at is never the one you want to switch to.
                let active = self.active_tab().map(|t| t.path.as_path());
                let files = shared
                    .history
                    .ranked(history::now())
                    .into_iter()
                    .filter(|p| Some(p.as_path()) != active && p.exists())
                    .collect();
                Some(QuickOpen::new(files))
            }
        };
    }

    fn set_sidebar_mode(&mut self, shared: &mut Shared, mode: SidebarMode) {
        self.sidebar = true;
        self.sidebar_mode = mode;
        if mode == SidebarMode::Outline {
            self.drop_thumbnails(shared);
        }
    }

    /// Drops the thumbnails of this window's tabs, now that its sidebar doesn't show them.
    fn drop_thumbnails(&self, shared: &mut Shared) {
        let docs: HashSet<DocId> = self.tabs.iter().map(|t| t.id).collect();
        shared.cache.retain(|k| !matches!(k, Key::Thumb { doc, .. } if docs.contains(doc)));
    }

    /// Saves a new sidebar width and re-renders thumbnails to match it.
    fn sidebar_resized(&mut self, ctx: &Context, shared: &mut Shared, width: f32) {
        let width = sidebar::clamp_width(width.round());
        if width != shared.config.sidebar_width {
            shared.config.sidebar_width = width;
            shared.config.save();
        }
        let thumb_px = sidebar::thumb_width_px(width, ctx.pixels_per_point());
        if thumb_px != shared.thumb_px {
            shared.thumb_px = thumb_px;
            shared.cache.retain(|k| !matches!(k, Key::Thumb { .. }));
        }
    }

    /// Follows a link or outline entry.
    fn follow(&mut self, target: Target) {
        match target {
            Target::Page { page, y } => {
                if let Some(tab) = self.tabs.get_mut(self.active)
                    && let DocState::Ready(layout) = &tab.doc
                {
                    tab.view.go_to_point(layout, page as usize, y);
                }
            }
            Target::Url(url) => {
                if ["http://", "https://", "mailto:"].iter().any(|scheme| url.starts_with(scheme)) {
                    let _ = std::process::Command::new("open").arg(&url).spawn();
                }
            }
        }
    }

    fn set_layout(&mut self, ctx: &Context, mode: PageLayout) {
        let Some(tab) = self.tabs.get_mut(self.active) else { return };
        let DocState::Ready(layout) = &mut tab.doc else { return };
        if tab.view.zoom == 0.0 || layout.mode() == mode {
            return;
        }
        tab.view.set_layout(layout, mode, ctx.input(|i| i.time));
        tab.snapshot_texture = None;
    }

    /// The active tab's page layout, once it has loaded.
    pub fn annotating(&self) -> bool {
        self.annotate.is_some()
    }

    pub fn layout(&self) -> Option<PageLayout> {
        match &self.active_tab()?.doc {
            DocState::Ready(layout) => Some(layout.mode()),
            _ => None,
        }
    }

    fn zoom(&mut self, ctx: &Context, command: Command) {
        let Some(tab) = self.tabs.get_mut(self.active) else { return };
        let DocState::Ready(layout) = &tab.doc else { return };
        let view = &mut tab.view;
        if view.zoom == 0.0 {
            return;
        }
        let zoom = match command {
            Command::ZoomIn => view.zoom * ZOOM_STEP,
            Command::ZoomOut => view.zoom / ZOOM_STEP,
            _ => fit_width_zoom(layout, view.viewport.width()),
        };
        let center = view.viewport.size() / 2.0;
        view.zoom_about(layout, zoom, center, ctx.input(|i| i.time));
    }

    /// Turns the active tab's `page`, or all its pages, a quarter turn, keeping the reading
    /// position. The file is
    /// reopened as a new document turned the new way: its tiles, links, outline positions and
    /// text boxes all depend on the turn, and go with the old one. The search starts over with
    /// the same text.
    fn rotate(&mut self, ctx: &Context, shared: &mut Shared, page: Option<u32>, clockwise: bool) {
        let Some(tab) = self.tabs.get_mut(self.active) else { return };
        let restore = match &tab.doc {
            DocState::Ready(layout) if tab.view.zoom > 0.0 => {
                let place = tab.view.place(layout);
                // A page that fit the window's width fits it again once turned.
                let fits = (tab.view.zoom - fit_width_zoom(layout, tab.view.viewport.width())).abs() < 1e-3;
                Some(history::Place { zoom: if fits { 0.0 } else { place.zoom }, ..place })
            }
            // Turned again before the last turn (or the first open) loaded: keep where that lands.
            DocState::Loading => tab.restore,
            _ => return,
        };
        match page {
            Some(page) => tab.rotations.turn_page(page, clockwise),
            None => tab.rotations.turn_all(clockwise),
        }
        shared.history.set_rotations(&tab.path, &tab.rotations, history::now());
        reopen(ctx, shared, tab, restore);
    }

    /// Writes the active tab's text boxes into its PDF, on a thread of its own as PDFKit
    /// rewrites the whole file. The tab reopens once it's done (see [`Self::check_saves`]).
    fn save(&mut self, ctx: &Context) {
        let Some(tab) = self.tabs.get_mut(self.active) else { return };
        let DocState::Ready(layout) = &tab.doc else { return };
        let boxes = &mut tab.text_boxes;
        boxes.finish_editing();
        if !boxes.unsaved() || boxes.saving() {
            return;
        }
        let rotations = &tab.rotations;
        let job = boxes.save_job(|page, b| {
            let shown = layout.page_size(page as usize);
            let upright = if rotations.of(page).degrees() % 180 == 90 { vec2(shown.y, shown.x) } else { shown };
            text_boxes::rect_on_page(ctx, b, upright)
        });
        let job = match job {
            Ok(job) => job,
            Err(e) => {
                boxes.saving = Some(Saving::Failed(e));
                return;
            }
        };
        let (done, result) = std::sync::mpsc::channel();
        let (path, ctx) = (tab.path.clone(), ctx.clone());
        let spawned = std::thread::Builder::new().name("save".into()).spawn(move || {
            let _ = done.send(engine::save_annotations(&path, &job));
            ctx.request_repaint();
        });
        boxes.saving = Some(match spawned {
            Ok(_) => Saving::Running(result),
            Err(e) => Saving::Failed(e.to_string()),
        });
    }

    /// Drops tab `index`'s unsaved edits; ⌘Z brings them back.
    fn revert(&mut self, index: usize) {
        if let Some(tab) = self.tabs.get_mut(index) {
            tab.text_boxes.revert();
        }
    }

    /// The active tab has edits not yet saved.
    pub fn unsaved(&self) -> bool {
        self.active_tab().is_some_and(|t| t.text_boxes.unsaved())
    }

    /// Takes in finished saves: a saved tab reopens, so its pages and boxes are read from the
    /// file as it now is.
    fn check_saves(&mut self, ctx: &Context, shared: &mut Shared) {
        for tab in &mut self.tabs {
            let Some(Saving::Running(result)) = &tab.text_boxes.saving else { continue };
            let result = match result.try_recv() {
                Ok(result) => result,
                Err(std::sync::mpsc::TryRecvError::Empty) => continue,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => Err("The save stopped unexpectedly".into()),
            };
            match result {
                Ok(outcome) => {
                    tab.text_boxes.saving = Some(Saving::Done(ctx.input(|i| i.time), outcome));
                    ctx.request_repaint_after(std::time::Duration::from_secs_f64(SAVED_SECONDS));
                    tab.text_boxes.saved();
                    let restore = match &tab.doc {
                        DocState::Ready(layout) if tab.view.zoom > 0.0 => Some(tab.view.place(layout)),
                        _ => tab.restore,
                    };
                    reopen(ctx, shared, tab, restore);
                }
                Err(e) => tab.text_boxes.saving = Some(Saving::Failed(e)),
            }
        }
    }

    /// Gives the selected text box the size or colour the annotate bar changed from `old` to
    /// `new`.
    fn restyle_selected_box(&mut self, old: &annotate::Style, new: &annotate::Style) {
        let Some(tab) = self.tabs.get_mut(self.active) else { return };
        let Some(r) = tab.text_boxes.selected else { return };
        if new.text_size != old.text_size || new.text_color != old.text_color {
            tab.text_boxes.change_box(r, |b| {
                if new.text_size != old.text_size {
                    b.size = new.text_size;
                }
                if new.text_color != old.text_color {
                    b.color = new.text_color;
                }
            });
        }
    }

    /// A text field or a modal has the keyboard: the find bar, the recent-files palette, the
    /// settings, or a text box being typed into.
    fn typing(&self, ctx: &Context) -> bool {
        self.modal_open()
            || find_bar::focused(ctx, self.serial)
            || self.active_tab().is_some_and(|t| t.text_boxes.typing())
    }

    fn modal_open(&self) -> bool {
        self.quick_open.is_some() || self.settings.is_some()
    }

    /// Keyboard shortcuts. In the app, the menu bar receives the ⌘ shortcuts before egui
    /// does; these also cover them when running without a menu (benchmarks).
    fn shortcuts(&mut self, ctx: &Context) -> Vec<Command> {
        let cmd = Modifiers::COMMAND;
        let shift = Modifiers::SHIFT;
        let alt = Modifiers::ALT;
        let numbers = [
            KeyCode::Num1,
            KeyCode::Num2,
            KeyCode::Num3,
            KeyCode::Num4,
            KeyCode::Num5,
            KeyCode::Num6,
            KeyCode::Num7,
            KeyCode::Num8,
            KeyCode::Num9,
        ];
        let palette_open = self.quick_open.is_some();
        let commands = ctx.input_mut(|i| {
            // Shifted variants first: consume_key ignores extra Shift.
            let bindings = [
                (Modifiers::CTRL | shift, KeyCode::Tab, Command::PreviousTab),
                (Modifiers::CTRL, KeyCode::Tab, Command::NextTab),
                (cmd | shift, KeyCode::OpenBracket, Command::PreviousTab),
                (cmd | shift, KeyCode::OpenCurlyBracket, Command::PreviousTab),
                (cmd | shift, KeyCode::CloseBracket, Command::NextTab),
                (cmd | shift, KeyCode::CloseCurlyBracket, Command::NextTab),
                (cmd | alt, KeyCode::ArrowLeft, Command::PreviousTab),
                (cmd | alt, KeyCode::ArrowRight, Command::NextTab),
                (cmd, KeyCode::O, Command::Open),
                (cmd, KeyCode::S, Command::Save),
                (cmd, KeyCode::N, Command::NewWindow),
                (cmd, KeyCode::K, Command::QuickOpen),
                (cmd, KeyCode::Comma, Command::Settings),
                (cmd, KeyCode::W, Command::CloseTab),
                (cmd | shift, KeyCode::A, Command::ToggleAnnotate),
                (cmd | alt, KeyCode::B, Command::ToggleOutline),
                (cmd, KeyCode::B, Command::ToggleSidebar),
                (cmd, KeyCode::Equals, Command::ZoomIn),
                (cmd, KeyCode::Plus, Command::ZoomIn),
                (cmd, KeyCode::Minus, Command::ZoomOut),
                (cmd, KeyCode::Num0, Command::ZoomToFit),
                (cmd, KeyCode::L, Command::RotateLeft),
                (cmd, KeyCode::R, Command::RotateRight),
                (cmd | alt, KeyCode::Num1, Command::SetLayout(PageLayout::Single)),
                (cmd | alt, KeyCode::Num2, Command::SetLayout(PageLayout::TwoPages)),
                (cmd | alt, KeyCode::Num3, Command::SetLayout(PageLayout::Book)),
                (cmd | shift, KeyCode::G, Command::FindPrevious),
                (cmd, KeyCode::G, Command::FindNext),
                (cmd, KeyCode::F, Command::Find),
            ];
            let mut commands: Vec<Command> =
                bindings.into_iter().filter(|(m, k, _)| i.consume_key(*m, *k)).map(|(_, _, c)| c).collect();
            // While the palette is open, ⌘1–9 pick its results instead of tabs.
            if !palette_open {
                commands.extend(
                    numbers
                        .iter()
                        .enumerate()
                        .filter(|(_, k)| i.consume_key(cmd, **k))
                        .map(|(n, _)| Command::SelectTab(n)),
                );
            }
            commands
        });
        // The palette's list and the text fields take the arrow and editing keys.
        if !self.typing(ctx) {
            self.view_shortcuts(ctx);
        }
        commands
    }

    /// ⌘C copies the active tab's selection, ⌘A selects all its text and Escape clears the
    /// selection, or with none, leaves annotate mode, unless a text field has the keys. Copied text goes on the clipboard here.
    fn selection_keys(&mut self, ctx: &Context) {
        let typing = self.typing(ctx);
        let Some(tab) = self.tabs.get_mut(self.active) else { return };
        // An open menu takes Escape to close.
        let menu_open = Popup::is_any_open(ctx);
        if !typing && let DocState::Ready(layout) = &tab.doc {
            let (copy, all, escape, undo, delete) = ctx.input_mut(|i| {
                (
                    i.events.iter().any(|e| matches!(e, egui::Event::Copy)),
                    i.consume_key(Modifiers::COMMAND, KeyCode::A),
                    !menu_open && i.consume_key(Modifiers::NONE, KeyCode::Escape),
                    i.consume_key(Modifiers::COMMAND, KeyCode::Z),
                    tab.text_boxes.selected.is_some()
                        && (i.consume_key(Modifiers::NONE, KeyCode::Backspace)
                            || i.consume_key(Modifiers::NONE, KeyCode::Delete)),
                )
            });
            if all {
                tab.text.select_all(layout.page_count() as u32);
            }
            if escape {
                if tab.text.selection().is_none() && tab.text_boxes.selected.is_none() {
                    self.annotate = None;
                }
                tab.text.clear_selection();
                tab.text_boxes.selected = None;
            }
            if undo {
                tab.text_boxes.undo();
            }
            if delete && let Some(r) = tab.text_boxes.selected {
                tab.text_boxes.delete(r);
            }
            if copy {
                tab.text.copy(ctx);
            }
        }
        if let Some(text) = tab.text.take_copied() {
            ctx.copy_text(text);
        }
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
        let page_scroll = (view.viewport.height() - LINE_SCROLL).max(LINE_SCROLL);

        ctx.input_mut(|i| {
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
            // From the row at the top, by a whole spread when pages are side by side. Several
            // rows can fit on screen, so stepping from the current (middle) one would skip some.
            if i.consume_key(none, KeyCode::ArrowLeft) {
                let top = view.place(layout).page;
                view.go_to_page(layout, layout.spread(top).start.saturating_sub(1));
            }
            if i.consume_key(none, KeyCode::ArrowRight) {
                let top = view.place(layout).page;
                view.go_to_page(layout, layout.spread(top).end);
            }
        });
    }

    /// `now` is egui's clock.
    fn status(&self, now: f64) -> Option<String> {
        let tab = self.active_tab()?;
        let DocState::Ready(layout) = &tab.doc else { return None };
        let spread = layout.spread(tab.view.current_page);
        let pages = match spread.len() {
            1 => format!("{}", spread.start + 1),
            _ => format!("{}–{}", spread.start + 1, spread.end),
        };
        let edits = match &tab.text_boxes.saving {
            Some(Saving::Running(_)) => "   Saving…".to_string(),
            Some(Saving::Failed(e)) => format!("   Not saved: {e}"),
            _ if tab.text_boxes.unsaved() => "   Edited".to_string(),
            Some(Saving::Done(_, engine::SaveOutcome::PreviewMayResize)) => {
                "   Saved; Preview may resize text when editing".to_string()
            }
            Some(Saving::Done(at, engine::SaveOutcome::Saved)) if now - at < SAVED_SECONDS => "   Saved".to_string(),
            _ => String::new(),
        };
        Some(format!("{pages} / {}   {:.0}%{edits}", layout.page_count(), tab.view.zoom * 100.0))
    }

    /// Draws one frame of the window. `ctx` must be in this window's viewport.
    pub fn ui(&mut self, ctx: &Context, shared: &mut Shared, wants: &mut Wants) -> Vec<Request> {
        let mut requests = Vec::new();
        for command in self.shortcuts(ctx) {
            match command {
                Command::Open
                | Command::NewWindow
                | Command::MoveTabToNewWindow
                | Command::MergeAllWindows
                | Command::Quit => requests.push(Request::Run(command)),
                // Closing an empty window closes the window.
                Command::CloseTab if self.tabs.is_empty() => requests.push(Request::Run(command)),
                _ => self.run(ctx, shared, command),
            }
        }
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        if !dropped.is_empty() {
            requests.push(Request::Open(dropped));
        }
        (self.inner_rect, self.outer_rect) = ctx.input(|i| (i.viewport().inner_rect, i.viewport().outer_rect));

        if self.sidebar
            && self.sidebar_mode == SidebarMode::Outline
            && let Some(tab) = self.tabs.get_mut(self.active)
            && matches!(tab.doc, DocState::Ready(_))
            && !tab.outline_requested
        {
            tab.outline_requested = true;
            shared.renderer.outline(tab.id);
        }

        let find_open = self.active_tab().is_some_and(|t| t.search.is_some());
        let typing_box = self.active_tab().is_some_and(|t| t.text_boxes.typing());
        if find_open
            && !self.modal_open()
            && !typing_box
            && !Popup::is_any_open(ctx)
            && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, KeyCode::Escape))
        {
            self.tabs[self.active].search = None;
        }
        self.selection_keys(ctx);
        self.check_saves(ctx, shared);
        self.update_find(ctx);

        let palette = palette(ctx.style().visuals.dark_mode);
        let bar = TopBottomPanel::top(Id::new(("tabs", self.serial)))
            .exact_height(tab_bar::HEIGHT)
            .frame(Frame::NONE)
            .show_separator_line(false)
            .show(ctx, |ui| {
                let status = self.status(ctx.input(|i| i.time));
                let view_menu = tab_bar::ViewMenu {
                    sidebar: self.sidebar.then_some(self.sidebar_mode),
                    layout: self.layout(),
                    annotating: self.annotate.is_some(),
                };
                tab_bar::show(ui, &self.tabs, self.active, &mut self.tab_scroll, status, view_menu, &palette)
            })
            .inner;
        match bar {
            Some(Action::Select(i)) => self.activate(ctx, shared, i),
            Some(Action::Close(i)) => self.close(ctx, shared, i),
            Some(Action::CloseOthers(keep)) => {
                for i in (0..self.tabs.len()).rev().filter(|&i| i != keep) {
                    self.close(ctx, shared, i);
                }
            }
            Some(Action::Revert(i)) => self.revert(i),
            Some(Action::Reveal(i)) => {
                if let Some(tab) = self.tabs.get(i) {
                    let _ = std::process::Command::new("open").arg("-R").arg(&tab.path).spawn();
                }
            }
            Some(Action::Move { from, to }) => self.move_tab(from, to),
            Some(Action::Detach { index, pointer }) => {
                let origin = self.inner_rect.map_or(Pos2::ZERO, |r| r.min);
                requests.push(Request::Detach { index, pointer: origin + pointer.to_vec2() });
            }
            Some(Action::Open) => requests.push(Request::OpenDialog),
            Some(Action::Run(command)) => requests.push(Request::Run(command)),
            None => {}
        }

        let content_frame = Frame::NONE.fill(palette.content);
        if self.sidebar
            && let Some(tab) = self.tabs.get_mut(self.active)
            && let DocState::Ready(layout) = &tab.doc
        {
            let panel = SidePanel::left(Id::new(("sidebar", self.serial)))
                .default_width(shared.config.sidebar_width)
                .width_range(sidebar::MIN_WIDTH..=sidebar::MAX_WIDTH)
                .resizable(true)
                .frame(content_frame)
                .show(ctx, |ui| {
                    sidebar::show(
                        ui,
                        self.sidebar_mode,
                        tab.id,
                        layout,
                        &mut tab.view,
                        tab.outline.as_deref(),
                        shared.thumb_px,
                        &mut shared.cache,
                        &shared.failed,
                        wants,
                        &palette,
                    )
                });
            if !ctx.input(|i| i.pointer.any_down()) {
                self.sidebar_resized(ctx, shared, panel.response.rect.width());
            }
            if let Some(target) = panel.inner {
                self.follow(target);
            }
        }

        // After the sidebar, so the bar spans the pages only.
        let annotate_bar = self.annotate.filter(|_| !self.tabs.is_empty());
        if let Some(tool) = annotate_bar {
            let style = shared.config.annotate;
            let action = TopBottomPanel::top(Id::new(("annotate", self.serial)))
                .exact_height(annotate::HEIGHT)
                .frame(Frame::NONE)
                .show_separator_line(false)
                .show(ctx, |ui| annotate::show(ui, tool, &style, &palette))
                .inner;
            match action {
                Some(annotate::Action::Tool(tool)) => self.annotate = Some(tool),
                Some(annotate::Action::Style(new)) => {
                    self.restyle_selected_box(&style, &new);
                    requests.push(Request::Run(Command::SetAnnotateStyle(new)));
                }
                Some(annotate::Action::Done) => self.annotate = None,
                None => {}
            }
        }

        let (clicked_link, menu_command) = CentralPanel::default()
            .frame(content_frame)
            .show(ctx, |ui| {
                self.visible_ready = false;
                let Some(tab) = self.tabs.get_mut(self.active) else {
                    empty_state(ui, &palette, "Drop a PDF or image here, or press ⌘O");
                    return (None, None);
                };
                match &tab.doc {
                    DocState::Loading => (None, None),
                    DocState::Failed(e) => {
                        empty_state(ui, &palette, e);
                        (None, None)
                    }
                    DocState::Ready(layout) => {
                        let out = Canvas {
                            doc: tab.id,
                            layout,
                            view: &mut tab.view,
                            snapshot: &mut tab.snapshot_texture,
                            cache: &mut shared.cache,
                            failed: &shared.failed,
                            links: &tab.links,
                            search: tab.search.as_ref(),
                            text: &mut tab.text,
                            text_boxes: &mut tab.text_boxes,
                            rotations: &tab.rotations,
                            tool: self.annotate,
                            style: shared.config.annotate,
                            thumb_px: shared.thumb_px,
                            prefetch: shared.prefetch,
                            page_border: palette.page_border,
                        }
                        .show(ui, wants);
                        self.visible_ready = out.ready;
                        if let Some(search) = &mut tab.search {
                            search.want_geometry(&out.visible_pages);
                        }
                        (out.clicked_link, out.command)
                    }
                }
            })
            .inner;
        if let Some(target) = clicked_link {
            self.follow(target);
        }
        if let Some(command) = menu_command {
            self.run(ctx, shared, command);
        }

        if !self.modal_open()
            && let Some(tab) = self.tabs.get_mut(self.active)
            && let Some(search) = &mut tab.search
        {
            let now = ctx.input(|i| i.time);
            let top = tab_bar::HEIGHT + if annotate_bar.is_some() { annotate::HEIGHT } else { 0.0 };
            match find_bar::show(ctx, self.serial, top, search, &palette) {
                Some(find_bar::Action::Edited) => search.edited(now),
                Some(find_bar::Action::Next) => self.find_step(true),
                Some(find_bar::Action::Previous) => self.find_step(false),
                Some(find_bar::Action::Close) => self.tabs[self.active].search = None,
                None => {}
            }
        }

        if let Some(quick_open) = &mut self.quick_open
            && let Some(outcome) = quick_open.show(ctx, &palette)
        {
            self.quick_open = None;
            if let Outcome::Open(path) = outcome {
                requests.push(Request::Open(vec![path]));
            }
        }

        let status = settings::Status {
            config: &shared.config,
            launched: shared.launched,
            window_buffers: shared.window_buffers,
            texture_bytes: shared.cache.bytes(),
            texture_budget: shared.cache.budget(),
        };
        if let Some(settings) = &mut self.settings
            && let Some(outcome) = settings.show(ctx, &palette, &status)
        {
            match outcome {
                settings::Outcome::Run(command) => requests.push(Request::Run(command)),
                settings::Outcome::Dismiss => self.settings = None,
            }
        }
        requests
    }
}

pub fn snapshot_texture(ctx: &Context, snapshot: &Snapshot) -> Option<TextureHandle> {
    let [w, h] = snapshot.size.map(|v| v as usize);
    // Decoded straight into the pixels egui uploads, rather than into bytes and then copied.
    let mut pixels: Vec<egui::Color32> = bytemuck::allocation::zeroed_vec(w * h);
    qoi::decode_to_buf(bytemuck::cast_slice_mut(&mut pixels), &snapshot.qoi).ok()?;
    let image = ColorImage::new([w, h], pixels);
    Some(ctx.load_texture("snapshot", image, TextureOptions::LINEAR))
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

/// Reopens `tab`'s file as a new document, at `restore`: its tiles, links, outline positions
/// and text layers go with the old one. The search starts over with the same text.
fn reopen(ctx: &Context, shared: &mut Shared, tab: &mut Tab, restore: Option<history::Place>) {
    shared.renderer.forget(tab.id);
    shared.cache.evict_doc(tab.id);
    shared.failed.retain(|k| k.doc() != tab.id);
    tab.id = shared.next_id;
    shared.next_id += 1;
    shared.renderer.open(tab.id, tab.path.clone(), tab.rotations.clone());
    tab.doc = DocState::Loading;
    tab.restore = restore;
    tab.snapshot = None;
    tab.snapshot_texture = None;
    tab.links.clear();
    tab.text_boxes.clear_file();
    tab.outline = None;
    tab.outline_requested = false;
    tab.text = TextLayers::new(tab.path.clone(), tab.rotations.clone());
    if let Some(old) = tab.search.take() {
        let start = restore.map_or(0, |p| p.page);
        let store = shared.config.search_store();
        let mut search = Search::new(ctx, tab.path.clone(), tab.rotations.clone(), old.page_count(), start, store);
        search.query = old.query.clone();
        search.focus = false;
        search.edited(ctx.input(|i| i.time));
        tab.search = Some(search);
    }
}
