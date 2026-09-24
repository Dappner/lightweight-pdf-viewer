use std::collections::HashMap;
use std::path::PathBuf;

use eframe::egui::{Rect, TextureHandle, Vec2};

use crate::history::Place;
use crate::layout::DocLayout;
use crate::renderer::{DocId, Link, OutlineEntry, Snapshot};
use crate::search::Search;

pub enum DocState {
    Loading,
    Failed(String),
    Ready(DocLayout),
}

pub struct Tab {
    pub id: DocId,
    pub path: PathBuf,
    pub title: String,
    pub doc: DocState,
    pub view: View,
    /// Compressed viewport image taken when the tab went to the background.
    pub snapshot: Option<Snapshot>,
    /// Decoded snapshot, shown right after switching back until sharp tiles are ready.
    pub snapshot_texture: Option<TextureHandle>,
    /// Link annotations per page, loaded as pages become visible; cleared in the background.
    pub links: HashMap<u32, Vec<Link>>,
    /// Loaded the first time the outline is shown.
    pub outline: Option<Vec<OutlineEntry>>,
    pub outline_requested: bool,
    /// Where the file was left last time, applied once it loads.
    pub restore: Option<Place>,
    /// The find bar's search, while it is open.
    pub search: Option<Search>,
}

/// Scroll and zoom state of a tab. Survives while the tab is in the background.
pub struct View {
    /// Points per PDF point. Zero until the first layout fits the page width.
    pub zoom: f32,
    /// Zoom that tiles are rendered at; follows `zoom` once it stops changing.
    pub render_zoom: f32,
    /// Previous render zoom, whose tiles are drawn while the new ones are rendering.
    pub prev_render_zoom: Option<f32>,
    pub zoom_changed_at: f64,
    pub offset: Vec2,
    pub pending_offset: Option<Vec2>,
    /// Screen rect of the page canvas in the last frame.
    pub viewport: Rect,
    /// The page being read: the one last navigated to until you scroll, and otherwise the
    /// one taking up the most of the viewport.
    pub current_page: usize,
    /// The page last navigated to (thumbnail, outline, link, arrow keys), which stays current
    /// until you scroll. Short pages can leave several on screen, the one navigated to at the top.
    pub landed_page: Option<usize>,
    /// Page the sidebar last scrolled to.
    pub sidebar_page: Option<usize>,
}

impl Tab {
    pub fn new(id: DocId, path: PathBuf, restore: Option<Place>) -> Self {
        let title = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        Self {
            id,
            path,
            title,
            doc: DocState::Loading,
            view: View::default(),
            snapshot: None,
            snapshot_texture: None,
            links: HashMap::new(),
            outline: None,
            outline_requested: false,
            restore,
            search: None,
        }
    }
}

impl Default for View {
    fn default() -> Self {
        Self {
            zoom: 0.0,
            render_zoom: 0.0,
            prev_render_zoom: None,
            zoom_changed_at: 0.0,
            offset: Vec2::ZERO,
            pending_offset: None,
            viewport: Rect::NOTHING,
            current_page: 0,
            landed_page: None,
            sidebar_page: None,
        }
    }
}
