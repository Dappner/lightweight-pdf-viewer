//! User settings, kept in `~/Library/Application Support/Lightweight PDF/config.json`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::memory::Memory;
use crate::search_index::{self, Limits, Store};
use crate::sidebar;

#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub sidebar_width: f32,
    /// Reopen the last session's windows and tabs at launch.
    pub restore_tabs: bool,
    /// The windows open when the app last changed them, the main one first.
    pub windows: Vec<SavedWindow>,
    /// How much to keep in memory to make scrolling and going back fast.
    pub memory: Memory,
    /// How much searched text to keep on disk, so searching a document again is fast.
    pub search_cache: Limits,
    /// Where searched text is saved; `None` doesn't save it (tests, benchmarks).
    #[serde(skip)]
    pub search_cache_dir: Option<PathBuf>,
    /// Where the settings are saved; `None` keeps them in memory only (tests, benchmarks).
    #[serde(skip)]
    path: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            sidebar_width: sidebar::DEFAULT_WIDTH,
            restore_tabs: true,
            windows: Vec::new(),
            memory: Memory::default(),
            search_cache: Limits::default(),
            search_cache_dir: None,
            path: None,
        }
    }
}

/// A window's tabs, which one was active, and where the window was.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedWindow {
    pub tabs: Vec<PathBuf>,
    pub active: usize,
    /// Screen position of the window's top-left corner and the size of its content, in points.
    pub frame: Option<[f32; 4]>,
}

impl Config {
    /// Loads the user's settings.
    pub fn load() -> Self {
        let mut config = support_file("config.json").map_or_else(Self::default, Self::load_from);
        config.search_cache_dir = search_index::user_dir();
        config
    }

    /// Where searches save page text, unless that's turned off.
    pub fn search_store(&self) -> Option<Store> {
        let dir = self.search_cache_dir.clone()?;
        (self.search_cache.max_mb > 0).then_some(Store { dir, limits: self.search_cache })
    }

    /// Loads settings saved at `path`, falling back to defaults if the file is missing or invalid.
    pub fn load_from(path: PathBuf) -> Self {
        let mut config: Self = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        config.sidebar_width = sidebar::clamp_width(config.sidebar_width);
        config.path = Some(path);
        config
    }

    pub fn save(&self) {
        let Some(path) = &self.path else { return };
        let Ok(json) = serde_json::to_vec_pretty(self) else { return };
        write_atomically(path, &json);
    }
}

/// A file in `~/Library/Application Support/Lightweight PDF/`, where the app keeps its state.
pub(crate) fn support_file(name: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Application Support/Lightweight PDF").join(name))
}

/// Writes through a temporary file and a rename, so a crash never leaves a half-written file.
pub(crate) fn write_atomically(path: &Path, bytes: &[u8]) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let temp = path.with_extension("tmp");
    if std::fs::write(&temp, bytes).is_ok() {
        let _ = std::fs::rename(&temp, path);
    }
}
