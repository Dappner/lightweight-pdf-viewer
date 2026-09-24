//! One window: its tabs, sidebar, tab strip and page canvas. The render thread, texture
//! cache and saved state are shared by all windows (see [`Shared`]).

use std::collections::HashSet;
use std::path::PathBuf;

use eframe::egui::{
    self, Align2, CentralPanel, ColorImage, Context, FontId, Frame, Id, Key as KeyCode, Modifiers, Pos2,
    Rect, SidePanel, TextureHandle, TextureOptions, TopBottomPanel, ViewportBuilder, ViewportId, vec2,
};

use crate::app::Command;
use crate::cache::TextureCache;
use crate::config::Config;
use crate::find_bar;
use crate::history::{self, History};
use crate::layout::PageLayout;
use crate::quick_open::{Outcome, QuickOpen};
use crate::renderer::{DocId, Key, Renderer, Snapshot, Target};
use crate::search::{Anchor, Search};
use crate::sidebar::{self, Mode as SidebarMode};
use crate::tab::{DocState, Tab};
use crate::tab_bar::{self, Action};
use crate::theme::{Palette, palette};
use crate::view::{Canvas, Wants, fit_width_zoom};

/// Seconds a tab you switched away from keeps its tiles, so flipping straight back is sharp.
const PARK_SECONDS: f64 = 3.0;
const ZOOM_STEP: f32 = 1.25;
const LINE_SCROLL: f32 = 48.0;

/// State shared by every window.
pub struct Shared {
    pub renderer: Renderer,
    pub cache: TextureCache,
    /// Screen heights rendered ahead of the visible area (see [`crate::memory::Tuning`]).
    pub prefetch: f32,
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
    Detach { index: usize, pointer: Pos2 },
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
    sidebar: bool,
    sidebar_mode: SidebarMode,
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
            sidebar: true,
            sidebar_mode: SidebarMode::Pages,
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
        shared.renderer.open(id, path.clone());
        let restore = shared.history.place(&path);
        self.tabs.push(Tab::new(id, path, restore));
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
        let tab = self.take(ctx, shared, index);
        shared.forget(&tab);
    }

    /// Closes every tab, as when the window closes.
    pub fn close_all(&mut self, shared: &mut Shared) {
        for tab in self.tabs.drain(..) {
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
            Command::QuickOpen => self.toggle_quick_open(shared),
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
            Command::ToggleOutline => self.set_sidebar_mode(shared, match self.sidebar_mode {
                SidebarMode::Pages => SidebarMode::Outline,
                SidebarMode::Outline => SidebarMode::Pages,
            }),
            Command::ShowSidebar(mode) => self.set_sidebar_mode(shared, mode),
            Command::SetLayout(mode) => self.set_layout(ctx, mode),
            Command::ZoomIn | Command::ZoomOut | Command::ZoomToFit => self.zoom(ctx, command),
            Command::Find => self.open_find(ctx, shared),
            Command::FindNext => self.find_step(true),
            Command::FindPrevious => self.find_step(false),
            _ => {}
        }
    }

    /// Opens the active tab's find bar, or focuses it and selects its text.
    fn open_find(&mut self, ctx: &Context, shared: &Shared) {
        let Some(tab) = self.tabs.get_mut(self.active) else { return };
        let DocState::Ready(layout) = &tab.doc else { return };
        let start = tab.view.current_page;
        let search = tab
            .search
            .get_or_insert_with(|| Search::new(ctx, tab.path.clone(), layout.page_count(), start, shared.config.search_store()));
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

    /// Keyboard shortcuts. In the app, the menu bar receives the ⌘ shortcuts before egui
    /// does; these also cover them when running without a menu (benchmarks).
    fn shortcuts(&mut self, ctx: &Context) -> Vec<Command> {
        let cmd = Modifiers::COMMAND;
        let shift = Modifiers::SHIFT;
        let alt = Modifiers::ALT;
        let numbers = [
            KeyCode::Num1, KeyCode::Num2, KeyCode::Num3, KeyCode::Num4, KeyCode::Num5,
            KeyCode::Num6, KeyCode::Num7, KeyCode::Num8, KeyCode::Num9,
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
                (cmd, KeyCode::N, Command::NewWindow),
                (cmd, KeyCode::K, Command::QuickOpen),
                (cmd, KeyCode::W, Command::CloseTab),
                (cmd | alt, KeyCode::B, Command::ToggleOutline),
                (cmd, KeyCode::B, Command::ToggleSidebar),
                (cmd, KeyCode::Equals, Command::ZoomIn),
                (cmd, KeyCode::Plus, Command::ZoomIn),
                (cmd, KeyCode::Minus, Command::ZoomOut),
                (cmd, KeyCode::Num0, Command::ZoomToFit),
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
                    numbers.iter().enumerate().filter(|(_, k)| i.consume_key(cmd, **k)).map(|(n, _)| Command::SelectTab(n)),
                );
            }
            commands
        });
        // The palette's list and the text fields take the arrow and editing keys.
        if self.quick_open.is_none() && !find_bar::focused(ctx, self.serial) {
            self.view_shortcuts(ctx);
        }
        commands
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

    fn status(&self) -> Option<String> {
        let tab = self.active_tab()?;
        let DocState::Ready(layout) = &tab.doc else { return None };
        let spread = layout.spread(tab.view.current_page);
        let pages = match spread.len() {
            1 => format!("{}", spread.start + 1),
            _ => format!("{}–{}", spread.start + 1, spread.end),
        };
        Some(format!("{pages} / {}   {:.0}%", layout.page_count(), tab.view.zoom * 100.0))
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
        if find_open && self.quick_open.is_none() && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, KeyCode::Escape)) {
            self.tabs[self.active].search = None;
        }
        self.update_find(ctx);

        let palette = palette(ctx.style().visuals.dark_mode);
        let bar = TopBottomPanel::top(Id::new(("tabs", self.serial)))
            .exact_height(tab_bar::HEIGHT)
            .frame(Frame::NONE)
            .show_separator_line(false)
            .show(ctx, |ui| {
                let status = self.status();
                let view_menu = tab_bar::ViewMenu { sidebar: self.sidebar.then_some(self.sidebar_mode), layout: self.layout() };
                tab_bar::show(ui, &self.tabs, self.active, &mut self.tab_scroll, status, view_menu, &palette)
            })
            .inner;
        match bar {
            Some(Action::Select(i)) => self.activate(ctx, shared, i),
            Some(Action::Close(i)) => self.close(ctx, shared, i),
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

        let clicked_link = CentralPanel::default().frame(content_frame).show(ctx, |ui| {
            self.visible_ready = false;
            let Some(tab) = self.tabs.get_mut(self.active) else {
                empty_state(ui, &palette, "Drop a PDF here or press ⌘O");
                return None;
            };
            match &tab.doc {
                DocState::Loading => None,
                DocState::Failed(e) => {
                    empty_state(ui, &palette, e);
                    None
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
                        thumb_px: shared.thumb_px,
                        prefetch: shared.prefetch,
                        page_border: palette.page_border,
                    }
                    .show(ui, wants);
                    self.visible_ready = out.ready;
                    if let Some(search) = &mut tab.search {
                        search.want_geometry(&out.visible_pages);
                    }
                    out.clicked_link
                }
            }
        })
        .inner;
        if let Some(target) = clicked_link {
            self.follow(target);
        }

        if self.quick_open.is_none()
            && let Some(tab) = self.tabs.get_mut(self.active)
            && let Some(search) = &mut tab.search
        {
            let now = ctx.input(|i| i.time);
            match find_bar::show(ctx, self.serial, tab_bar::HEIGHT, search, &palette) {
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
