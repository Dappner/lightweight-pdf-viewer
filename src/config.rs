//! User settings, kept in `~/Library/Application Support/Lightweight PDF/config.json`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::sidebar;

#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub sidebar_width: f32,
    /// Reopen the last session's tabs at launch.
    pub restore_tabs: bool,
    /// The tabs open when the app last changed them, and which one was active.
    pub tabs: Vec<PathBuf>,
    pub active_tab: usize,
    /// Where the settings are saved; `None` keeps them in memory only (tests, benchmarks).
    #[serde(skip)]
    path: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            sidebar_width: sidebar::DEFAULT_WIDTH,
            restore_tabs: true,
            tabs: Vec::new(),
            active_tab: 0,
            path: None,
        }
    }
}

impl Config {
    /// Loads the user's settings.
    pub fn load() -> Self {
        match std::env::var_os("HOME") {
            Some(home) => Self::load_from(PathBuf::from(home).join("Library/Application Support/Lightweight PDF/config.json")),
            None => Self::default(),
        }
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
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, json);
    }
}
