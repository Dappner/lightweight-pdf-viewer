use std::collections::HashSet;
use std::path::PathBuf;

use eframe::egui::{
    self, Align2, CentralPanel, ColorImage, Context, FontId, Frame, Key as KeyCode, Modifiers,
    SidePanel, TextureHandle, TextureOptions, TopBottomPanel, Vec2, ViewportCommand, vec2,
};

use crate::cache::TextureCache;
use crate::config::Config;
use crate::finder;
use crate::history::{self, History};
use crate::layout::DocLayout;
use crate::menu;
use crate::quick_open::{Outcome, QuickOpen};
use crate::renderer::{DocId, Key, Output, Renderer, Target};
use crate::sidebar::{self, Mode as SidebarMode};
use crate::tab::{DocState, Snapshot, Tab};
use crate::tab_bar::{self, Action};
use crate::theme::{Palette, palette};
use crate::view::{Canvas, Wants, fit_width_zoom};

/// Seconds a tab you switched away from keeps its tiles, so flipping straight back is sharp.
const PARK_SECONDS: f64 = 3.0;
const ZOOM_STEP: f32 = 1.25;
const LINE_SCROLL: f32 = 48.0;

pub struct App {
    tabs: Vec<Tab>,
    active: usize,
    next_id: DocId,
    renderer: Renderer,
    cache: TextureCache,
    /// Screen heights rendered ahead of the visible area (see [`crate::memory::Tuning`]).
    prefetch: f32,
    failed: HashSet<Key>,
    config: Config,
    history: History,
    /// The ⌘K recent-files palette, while it is open.
    quick_open: Option<QuickOpen>,
    sidebar: bool,
    sidebar_mode: SidebarMode,
    /// Pixel width thumbnails are rendered at. Follows the sidebar's width once a resize
    /// ends, so dragging stretches the existing thumbnails instead of re-rendering them.
    thumb_px: u32,
    tab_scroll: f32,
    /// The last tab switched away from, whose tiles are kept until this time.
    parked: Option<(DocId, f64)>,
    /// Whether every visible tile of the active tab was drawn sharp last frame.
    visible_ready: bool,
    sleep: Sleep,
    /// See [`crate::memory::Tuning::sleep_after`].
    sleep_after: Option<f64>,
    /// When the app was last focused, scrolled or zoomed.
    last_active: f64,
}

/// Whether the app has let go of its page tiles while in the background.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Sleep {
    Awake,
    /// Waiting for a snapshot of the active tab to show in place of its tiles.
    Falling,
    Asleep,
}

/// Actions reachable from the menu bar and keyboard shortcuts.
#[derive(Clone, Copy, Debug)]
pub enum Command {
    Open,
    /// Shows or hides the recent-files palette.
    QuickOpen,
    CloseTab,
    NextTab,
    PreviousTab,
    SelectTab(usize),
    ToggleSidebar,
    /// Switches the sidebar between page thumbnails and the outline.
    ToggleOutline,
    ZoomIn,
    ZoomOut,
    ZoomToFit,
    Quit,
}

/// A snapshot of app state for benchmarks.
pub struct Stats {
    pub tabs: usize,
    pub active: usize,
    /// The active tab has loaded and every visible tile is sharp.
    pub ready: bool,
    pub snapshot_shown: bool,
    pub current_page: usize,
    pub page_count: usize,
    pub texture_bytes: usize,
    /// Compressed snapshots held by background tabs.
    pub snapshot_bytes: usize,
    /// Page tiles were dropped while the app was in the background.
    pub asleep: bool,
}

impl App {
    pub fn new(ctx: &Context, config: Config, history: History, files: Vec<PathBuf>) -> Self {
        // ⌘+/⌘- zoom the document, not the UI.
        ctx.options_mut(|o| o.zoom_with_keyboard = false);
        finder::set_context(ctx);
        let tuning = config.memory.tuning();
        let mut app = Self {
            tabs: Vec::new(),
            active: 0,
            next_id: 0,
            renderer: Renderer::spawn(ctx.clone(), tuning.pages_per_open),
            cache: TextureCache::new(tuning.texture_budget),
            prefetch: tuning.prefetch,
            failed: HashSet::new(),
            thumb_px: sidebar::thumb_width_px(config.sidebar_width, ctx.pixels_per_point()),
            config,
            history,
            quick_open: None,
            sidebar: true,
            sidebar_mode: SidebarMode::Pages,
            tab_scroll: 0.0,
            parked: None,
            visible_ready: false,
            sleep: Sleep::Awake,
            sleep_after: tuning.sleep_after,
            last_active: 0.0,
        };
        if app.config.restore_tabs {
            let saved: Vec<PathBuf> = app.config.tabs.iter().filter(|p| p.exists()).cloned().collect();
            let active = app.config.tabs.get(app.config.active_tab).cloned();
            // Reopening a session isn't a choice to open these files, so it doesn't count
            // towards their rank in the history.
            for file in saved {
                app.open_tab(ctx, file);
            }
            if let Some(i) = active.and_then(|path| app.tabs.iter().position(|t| t.path == path)) {
                app.activate(ctx, i);
            }
        }
        for file in files {
            app.open(ctx, file);
        }
        app
    }

    /// Records the open tabs in the config, so the next launch can reopen them.
    fn save_tabs(&mut self) {
        let tabs: Vec<PathBuf> =
            self.tabs.iter().filter(|t| !matches!(t.doc, DocState::Failed(_))).map(|t| t.path.clone()).collect();
        let active = self.tabs.get(self.active).and_then(|a| tabs.iter().position(|p| *p == a.path)).unwrap_or(0);
        if tabs != self.config.tabs || active != self.config.active_tab {
            self.config.tabs = tabs;
            self.config.active_tab = active;
            self.config.save();
        }
    }

    /// Opens `path` in a tab (or switches to it) and records it in the history.
    pub fn open(&mut self, ctx: &Context, path: PathBuf) {
        let path = path.canonicalize().unwrap_or(path);
        self.history.visit(&path, history::now());
        self.open_tab(ctx, path);
    }

    /// `path` should already be canonical, as saved tabs are.
    fn open_tab(&mut self, ctx: &Context, path: PathBuf) {
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
    /// compressed snapshot, and its open PDF document is released. Its textures are kept for
    /// a few seconds (see [`PARK_SECONDS`]) in case you flip straight back.
    fn activate(&mut self, ctx: &Context, index: usize) {
        // Returning to the parked tab within the grace period: its tiles are still cached.
        let returning = self.parked.is_some_and(|(doc, _)| doc == self.tabs[index].id);
        if index != self.active
            && let Some(old) = self.tabs.get_mut(self.active)
        {
            if let DocState::Ready(layout) = &old.doc
                && old.view.zoom > 0.0
            {
                self.renderer.snapshot(old.view.snapshot_job(layout, old.id, ctx.pixels_per_point()));
            }
            old.snapshot_texture = None;
            old.links.clear();
            if let Some((parked, _)) = self.parked.take()
                && !returning
            {
                self.cache.evict_doc(parked);
            }
            self.parked = Some((old.id, ctx.input(|i| i.time) + PARK_SECONDS));
        }
        self.active = index;

        let tab = &mut self.tabs[index];
        if returning {
            return;
        }
        if let Some(snapshot) = tab.snapshot.take() {
            tab.snapshot_texture = snapshot_texture(ctx, &snapshot);
        }
    }

    /// In the background, the page tiles (most of the app's GPU memory) are swapped for a
    /// snapshot of the viewport once the app has been left alone for a while. Focusing,
    /// scrolling or zooming wakes it; the snapshot shows until the tiles are redrawn.
    fn sleep_or_wake(&mut self, ctx: &Context) {
        let Some(after) = self.sleep_after else { return };
        let (now, active) = ctx.input(|i| {
            let focused = i.viewport().focused.unwrap_or(true);
            (i.time, focused || i.raw_scroll_delta != Vec2::ZERO || i.zoom_delta() != 1.0)
        });
        if active {
            self.last_active = now;
            self.sleep = Sleep::Awake;
        } else if self.sleep == Sleep::Awake {
            let idle = now - self.last_active;
            if idle < after {
                ctx.request_repaint_after(std::time::Duration::from_secs_f64(after - idle));
            } else if let Some(tab) = self.tabs.get(self.active)
                && let DocState::Ready(layout) = &tab.doc
                && tab.view.zoom > 0.0
            {
                // The tiles go once the snapshot that replaces them arrives (see `receive`).
                self.renderer.snapshot(tab.view.snapshot_job(layout, tab.id, ctx.pixels_per_point()));
                self.sleep = Sleep::Falling;
            } else {
                self.release_tiles();
                self.sleep = Sleep::Asleep;
            }
        }
    }

    /// Drops every page tile (thumbnails are small and stay) and closes the open document.
    fn release_tiles(&mut self) {
        self.cache.retain(|k| !matches!(k, Key::Tile { .. }));
        self.parked = None;
        self.renderer.release();
    }

    fn expire_parked(&mut self, ctx: &Context) {
        let Some((doc, until)) = self.parked else { return };
        let now = ctx.input(|i| i.time);
        if now >= until {
            self.cache.evict_doc(doc);
            self.parked = None;
        } else {
            ctx.request_repaint_after(std::time::Duration::from_secs_f64(until - now));
        }
    }

    fn close(&mut self, ctx: &Context, index: usize) {
        let tab = self.tabs.remove(index);
        self.renderer.forget(tab.id);
        self.cache.evict_doc(tab.id);
        if self.parked.is_some_and(|(doc, _)| doc == tab.id) {
            self.parked = None;
        }
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

    fn move_tab(&mut self, from: usize, to: usize) {
        let active_id = self.tabs[self.active].id;
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        self.active = self.tabs.iter().position(|t| t.id == active_id).unwrap_or(0);
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
                // Tiles that were in flight when the app fell asleep.
                Output::Image { key: Key::Tile { .. }, .. } if self.sleep != Sleep::Awake => {}
                Output::Image { key, size, rgba } if Some(key.doc()) == active_id => {
                    let image = ColorImage::from_rgba_premultiplied(size, &rgba);
                    let texture = ctx.load_texture("page", image, TextureOptions::LINEAR);
                    self.cache.insert(key, texture, size[0] * size[1] * 4);
                }
                Output::Image { .. } => {}
                Output::Failed(key) => {
                    self.failed.insert(key);
                }
                Output::Snapshot { doc, size, qoi } if Some(doc) == active_id && self.sleep == Sleep::Falling => {
                    self.tabs[self.active].snapshot_texture = snapshot_texture(ctx, &Snapshot { size, qoi });
                    self.release_tiles();
                    self.sleep = Sleep::Asleep;
                }
                Output::Snapshot { doc, size, qoi } if Some(doc) != active_id => {
                    if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == doc) {
                        tab.snapshot = Some(Snapshot { size, qoi });
                    }
                }
                Output::Snapshot { .. } => {}
                Output::Links { doc, page, links } if Some(doc) == active_id => {
                    self.tabs[self.active].links.insert(page, links);
                }
                Output::Links { .. } => {}
                Output::Outline { doc, entries } => {
                    if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == doc) {
                        tab.outline = Some(entries);
                    }
                }
            }
        }
    }

    pub fn run(&mut self, ctx: &Context, command: Command) {
        let n = self.tabs.len();
        match command {
            Command::Open => self.open_dialog(ctx),
            Command::QuickOpen => self.toggle_quick_open(),
            Command::CloseTab if n == 0 => ctx.send_viewport_cmd(ViewportCommand::Close),
            Command::CloseTab => self.close(ctx, self.active),
            Command::NextTab if n > 0 => self.activate(ctx, (self.active + 1) % n),
            Command::PreviousTab if n > 0 => self.activate(ctx, (self.active + n - 1) % n),
            // Like browsers, the ninth tab shortcut always selects the last tab.
            Command::SelectTab(i) if n > 0 => self.activate(ctx, if i == 8 { n - 1 } else { i.min(n - 1) }),
            Command::NextTab | Command::PreviousTab | Command::SelectTab(_) => {}
            Command::ToggleSidebar => {
                self.sidebar = !self.sidebar;
                if !self.sidebar {
                    self.cache.retain(|k| !matches!(k, Key::Thumb { .. }));
                }
            }
            Command::ToggleOutline => self.set_sidebar_mode(match self.sidebar_mode {
                SidebarMode::Pages => SidebarMode::Outline,
                SidebarMode::Outline => SidebarMode::Pages,
            }),
            Command::ZoomIn | Command::ZoomOut | Command::ZoomToFit => self.zoom(ctx, command),
            Command::Quit => ctx.send_viewport_cmd(ViewportCommand::Close),
        }
    }

    fn toggle_quick_open(&mut self) {
        self.quick_open = match self.quick_open {
            Some(_) => None,
            None => {
                // The file you're looking at is never the one you want to switch to.
                let active = self.tabs.get(self.active).map(|t| t.path.as_path());
                let files = self
                    .history
                    .ranked(history::now())
                    .into_iter()
                    .filter(|p| Some(p.as_path()) != active && p.exists())
                    .collect();
                Some(QuickOpen::new(files))
            }
        };
    }

    fn set_sidebar_mode(&mut self, mode: SidebarMode) {
        self.sidebar = true;
        self.sidebar_mode = mode;
        if mode == SidebarMode::Outline {
            self.cache.retain(|k| !matches!(k, Key::Thumb { .. }));
        }
    }

    /// Saves a new sidebar width and re-renders thumbnails to match it.
    fn sidebar_resized(&mut self, ctx: &Context, width: f32) {
        let width = sidebar::clamp_width(width.round());
        if width != self.config.sidebar_width {
            self.config.sidebar_width = width;
            self.config.save();
        }
        let thumb_px = sidebar::thumb_width_px(width, ctx.pixels_per_point());
        if thumb_px != self.thumb_px {
            self.thumb_px = thumb_px;
            self.cache.retain(|k| !matches!(k, Key::Thumb { .. }));
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
    fn shortcuts(&mut self, ctx: &Context) {
        let cmd = Modifiers::COMMAND;
        let shift = Modifiers::SHIFT;
        let numbers = [
            KeyCode::Num1, KeyCode::Num2, KeyCode::Num3, KeyCode::Num4, KeyCode::Num5,
            KeyCode::Num6, KeyCode::Num7, KeyCode::Num8, KeyCode::Num9,
        ];
        let palette_open = self.quick_open.is_some();
        let commands: Vec<Command> = ctx.input_mut(|i| {
            // Shifted variants first: consume_key ignores extra Shift.
            let bindings = [
                (Modifiers::CTRL | shift, KeyCode::Tab, Command::PreviousTab),
                (Modifiers::CTRL, KeyCode::Tab, Command::NextTab),
                (cmd | shift, KeyCode::OpenBracket, Command::PreviousTab),
                (cmd | shift, KeyCode::OpenCurlyBracket, Command::PreviousTab),
                (cmd | shift, KeyCode::CloseBracket, Command::NextTab),
                (cmd | shift, KeyCode::CloseCurlyBracket, Command::NextTab),
                (cmd | Modifiers::ALT, KeyCode::ArrowLeft, Command::PreviousTab),
                (cmd | Modifiers::ALT, KeyCode::ArrowRight, Command::NextTab),
                (cmd, KeyCode::O, Command::Open),
                (cmd, KeyCode::K, Command::QuickOpen),
                (cmd, KeyCode::W, Command::CloseTab),
                (cmd | Modifiers::ALT, KeyCode::B, Command::ToggleOutline),
                (cmd, KeyCode::B, Command::ToggleSidebar),
                (cmd, KeyCode::Equals, Command::ZoomIn),
                (cmd, KeyCode::Plus, Command::ZoomIn),
                (cmd, KeyCode::Minus, Command::ZoomOut),
                (cmd, KeyCode::Num0, Command::ZoomToFit),
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
        for command in commands {
            self.run(ctx, command);
        }
        // The palette's list and text field take the arrow and editing keys.
        if self.quick_open.is_none() {
            self.view_shortcuts(ctx);
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
            if i.consume_key(none, KeyCode::ArrowLeft) {
                view.go_to_page(layout, view.current_page.saturating_sub(1));
            }
            if i.consume_key(none, KeyCode::ArrowRight) {
                view.go_to_page(layout, view.current_page + 1);
            }
        });
    }

    pub fn stats(&self) -> Stats {
        let active = self.tabs.get(self.active);
        Stats {
            tabs: self.tabs.len(),
            active: self.active,
            ready: self.visible_ready,
            snapshot_shown: active.is_some_and(|t| t.snapshot_texture.is_some()),
            current_page: active.map_or(0, |t| t.view.current_page),
            page_count: active.map_or(0, |t| match &t.doc {
                DocState::Ready(layout) => layout.page_count(),
                _ => 0,
            }),
            texture_bytes: self.cache.bytes(),
            snapshot_bytes: self.tabs.iter().filter_map(|t| t.snapshot.as_ref()).map(|s| s.qoi.len()).sum(),
            asleep: self.sleep == Sleep::Asleep,
        }
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
        self.ui(ctx);
    }
}

impl App {
    /// One frame of the app. Separate from `eframe::App::update` so it can run headless.
    pub fn ui(&mut self, ctx: &Context) {
        // Before `receive`, so a snapshot that arrives as the app wakes is ignored.
        self.sleep_or_wake(ctx);
        self.receive(ctx);
        self.expire_parked(ctx);
        self.cache.begin_frame();

        let mut opened = finder::take();
        opened.extend(ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect::<Vec<_>>()));
        for path in opened {
            self.open(ctx, path);
        }
        for command in menu::take() {
            self.run(ctx, command);
        }
        self.shortcuts(ctx);
        if self.sidebar
            && self.sidebar_mode == SidebarMode::Outline
            && let Some(tab) = self.tabs.get_mut(self.active)
            && matches!(tab.doc, DocState::Ready(_))
            && !tab.outline_requested
        {
            tab.outline_requested = true;
            self.renderer.outline(tab.id);
        }

        let palette = palette(ctx.style().visuals.dark_mode);
        let mut wants = Wants::default();

        let bar = TopBottomPanel::top("tabs")
            .exact_height(tab_bar::HEIGHT)
            .frame(Frame::NONE)
            .show_separator_line(false)
            .show(ctx, |ui| {
                let status = self.status();
                tab_bar::show(ui, &self.tabs, self.active, &mut self.tab_scroll, status, &palette)
            })
            .inner;
        match bar {
            Some(Action::Select(i)) => self.activate(ctx, i),
            Some(Action::Close(i)) => self.close(ctx, i),
            Some(Action::Move { from, to }) => self.move_tab(from, to),
            Some(Action::Open) => self.open_dialog(ctx),
            None => {}
        }

        let content_frame = Frame::NONE.fill(palette.content);
        if self.sidebar
            && let Some(tab) = self.tabs.get_mut(self.active)
            && let DocState::Ready(layout) = &tab.doc
        {
            let panel = SidePanel::left("sidebar")
                .default_width(self.config.sidebar_width)
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
                        self.thumb_px,
                        &mut self.cache,
                        &self.failed,
                        &mut wants,
                        &palette,
                    )
                });
            if !ctx.input(|i| i.pointer.any_down()) {
                self.sidebar_resized(ctx, panel.response.rect.width());
            }
            match panel.inner {
                Some(sidebar::Action::GoTo(target)) => self.follow(target),
                Some(sidebar::Action::SetMode(mode)) => self.set_sidebar_mode(mode),
                None => {}
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
                        cache: &mut self.cache,
                        failed: &self.failed,
                        links: &tab.links,
                        thumb_px: self.thumb_px,
                        prefetch: self.prefetch,
                        page_border: palette.page_border,
                    }
                    .show(ui, &mut wants);
                    self.visible_ready = out.ready;
                    out.clicked_link
                }
            }
        })
        .inner;
        if let Some(target) = clicked_link {
            self.follow(target);
        }

        if let Some(quick_open) = &mut self.quick_open
            && let Some(outcome) = quick_open.show(ctx, &palette)
        {
            self.quick_open = None;
            if let Outcome::Open(path) = outcome {
                self.open(ctx, path);
            }
        }

        self.save_tabs();
        if self.sleep != Sleep::Awake {
            // The snapshot stands in for the page; only thumbnails are still drawn.
            wants.visible.clear();
            wants.prefetch.clear();
        }
        self.renderer.set_wanted(wants.into_queue());
        // A parked tab that has lost tiles to the budget would come back with holes, so it
        // gives up the rest and shows its snapshot when switched back to instead.
        let evicted = self.cache.trim();
        if let Some((parked, _)) = self.parked
            && evicted.contains(&parked)
        {
            self.cache.evict_doc(parked);
            self.parked = None;
        }
    }
}

fn snapshot_texture(ctx: &Context, snapshot: &Snapshot) -> Option<TextureHandle> {
    let (_, rgba) = qoi::decode_to_vec(&snapshot.qoi).ok()?;
    let [w, h] = snapshot.size.map(|v| v as usize);
    let image = ColorImage::from_rgba_premultiplied([w, h], &rgba);
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
