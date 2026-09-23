//! Files you've opened, ranked by frecency, kept in
//! `~/Library/Application Support/Lightweight PDF/history.json`.
//!
//! Each file has a score that halves every [`HALF_LIFE`] and grows by one per open, so files
//! opened often and lately rank first.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{support_file, write_atomically};

const HALF_LIFE: f64 = 5.0 * 24.0 * 60.0 * 60.0;
/// The lowest-ranked files are forgotten past this many.
const MAX_ENTRIES: usize = 500;

#[derive(Default)]
pub struct History {
    entries: Vec<Entry>,
    /// Where the history is saved; `None` keeps it in memory only (tests, benchmarks).
    path: Option<PathBuf>,
}

#[derive(Serialize, Deserialize)]
struct Entry {
    path: PathBuf,
    score: f64,
    /// Unix seconds when `score` was last brought up to date.
    updated: u64,
}

impl Entry {
    fn score_at(&self, now: u64) -> f64 {
        self.score * 0.5f64.powf(now.saturating_sub(self.updated) as f64 / HALF_LIFE)
    }
}

impl History {
    /// Loads the user's history.
    pub fn load() -> Self {
        support_file("history.json").map_or_else(Self::default, Self::load_from)
    }

    /// Loads history saved at `path`, starting empty if the file is missing or invalid.
    pub fn load_from(path: PathBuf) -> Self {
        let entries = std::fs::read(&path).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default();
        Self { entries, path: Some(path) }
    }

    /// Records that `path` was opened at `now` (Unix seconds).
    pub fn visit(&mut self, path: &Path, now: u64) {
        match self.entries.iter_mut().find(|e| e.path == path) {
            Some(entry) => {
                entry.score = entry.score_at(now) + 1.0;
                entry.updated = now;
            }
            None => self.entries.push(Entry { path: path.to_path_buf(), score: 1.0, updated: now }),
        }
        if self.entries.len() > MAX_ENTRIES {
            self.sort(now);
            self.entries.truncate(MAX_ENTRIES);
        }
        self.save();
    }

    /// Every recorded file, highest-ranked first.
    pub fn ranked(&self, now: u64) -> Vec<PathBuf> {
        let mut ranked: Vec<(f64, &Path)> = self.entries.iter().map(|e| (e.score_at(now), e.path.as_path())).collect();
        ranked.sort_by(|a, b| b.0.total_cmp(&a.0));
        ranked.into_iter().map(|(_, path)| path.to_path_buf()).collect()
    }

    fn sort(&mut self, now: u64) {
        self.entries.sort_by(|a, b| b.score_at(now).total_cmp(&a.score_at(now)));
    }

    fn save(&self) {
        let Some(path) = &self.path else { return };
        let Ok(json) = serde_json::to_vec(&self.entries) else { return };
        write_atomically(path, &json);
    }
}

/// The current time in Unix seconds.
pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 24 * 60 * 60;

    fn names(history: &History, now: u64) -> Vec<String> {
        history.ranked(now).iter().map(|p| p.display().to_string()).collect()
    }

    #[test]
    fn frequent_beats_once_and_recent_beats_stale() {
        let mut h = History::default();
        let start = 1_000 * DAY;
        for i in 0..3 {
            h.visit(Path::new("often"), start + i);
        }
        h.visit(Path::new("once"), start + 10);
        assert_eq!(names(&h, start + 20), ["often", "once"]);

        // Three opens a month ago lose to one open today.
        let later = start + 30 * DAY;
        h.visit(Path::new("today"), later);
        assert_eq!(names(&h, later)[0], "today");
    }

    #[test]
    fn keeps_only_the_top_entries() {
        let mut h = History::default();
        h.visit(Path::new("keep"), 0);
        h.visit(Path::new("keep"), 0);
        for i in 0..MAX_ENTRIES {
            h.visit(Path::new(&format!("f{i}")), 0);
        }
        assert_eq!(h.entries.len(), MAX_ENTRIES);
        assert_eq!(names(&h, 0)[0], "keep");
    }

    #[test]
    fn round_trips_through_the_file() {
        let path = std::env::temp_dir().join(format!("lightweight-pdf-history-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut h = History::load_from(path.clone());
        h.visit(Path::new("/a.pdf"), 5);
        h.visit(Path::new("/b.pdf"), 5);
        h.visit(Path::new("/b.pdf"), 5);
        assert_eq!(names(&History::load_from(path.clone()), 5), ["/b.pdf", "/a.pdf"]);
        let _ = std::fs::remove_file(path);
    }
}
