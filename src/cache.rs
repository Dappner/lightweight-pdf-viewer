//! GPU texture cache shared by all tabs, bounded by a byte budget.
//!
//! Entries used in any window's latest frame are never evicted; everything else is evicted
//! least-recently-used first once the budget is exceeded.

use std::collections::HashMap;

use eframe::egui::{TextureHandle, TextureId};

use crate::renderer::{DocId, Key};

struct Entry {
    texture: TextureHandle,
    bytes: usize,
    last_used: u64,
}

pub struct TextureCache {
    entries: HashMap<Key, Entry>,
    bytes: usize,
    budget: usize,
    frame: u64,
}

impl TextureCache {
    pub fn new(budget: usize) -> Self {
        Self { entries: HashMap::new(), bytes: 0, budget, frame: 0 }
    }

    /// Starts a window's frame; returns its number, for [`Self::trim`].
    pub fn begin_frame(&mut self) -> u64 {
        self.frame += 1;
        self.frame
    }

    pub fn set_budget(&mut self, budget: usize) {
        self.budget = budget;
    }

    /// Returns the texture and marks it as in use this frame.
    pub fn get(&mut self, key: &Key) -> Option<TextureId> {
        let entry = self.entries.get_mut(key)?;
        entry.last_used = self.frame;
        Some(entry.texture.id())
    }

    pub fn contains(&self, key: &Key) -> bool {
        self.entries.contains_key(key)
    }

    /// Returns the texture without protecting it from eviction.
    pub fn peek(&self, key: &Key) -> Option<TextureId> {
        self.entries.get(key).map(|e| e.texture.id())
    }

    pub fn insert(&mut self, key: Key, texture: TextureHandle, bytes: usize) {
        let entry = Entry { texture, bytes, last_used: self.frame };
        self.bytes += bytes;
        if let Some(old) = self.entries.insert(key, entry) {
            self.bytes -= old.bytes;
        }
    }

    pub fn retain(&mut self, mut keep: impl FnMut(&Key) -> bool) {
        let bytes = &mut self.bytes;
        self.entries.retain(|k, e| {
            let kept = keep(k);
            if !kept {
                *bytes -= e.bytes;
            }
            kept
        });
    }

    pub fn evict_doc(&mut self, doc: DocId) {
        self.retain(|k| k.doc() != doc);
    }

    /// Evicts down to the budget, sparing entries used since frame `keep_since`; returns the
    /// documents that lost entries.
    pub fn trim(&mut self, keep_since: u64) -> Vec<DocId> {
        let mut evicted = Vec::new();
        if self.bytes <= self.budget {
            return evicted;
        }
        let mut candidates: Vec<(u64, Key)> = self
            .entries
            .iter()
            .filter(|(_, e)| e.last_used < keep_since)
            .map(|(k, e)| (e.last_used, *k))
            .collect();
        candidates.sort_unstable_by_key(|(used, _)| *used);
        for (_, key) in candidates {
            if self.bytes <= self.budget {
                break;
            }
            if let Some(e) = self.entries.remove(&key) {
                self.bytes -= e.bytes;
                evicted.push(key.doc());
            }
        }
        evicted
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }
}
