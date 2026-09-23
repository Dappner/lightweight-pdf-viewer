//! GPU texture cache shared by all tabs, bounded by a byte budget.
//!
//! Entries a window used in its latest frame are never evicted, however long ago that frame
//! was (a window only draws when something in it changes), nor are tiles a window is still
//! waiting for: any window's frame can take in another window's tiles, which that window
//! only draws on its next frame. Everything else is evicted least-recently-used first once
//! the budget is exceeded.

use std::collections::{HashMap, HashSet};

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
    /// The window drawing now, by serial.
    window: u64,
    /// What each window used in its latest frame, by serial.
    shown: HashMap<u64, HashSet<Key>>,
}

impl TextureCache {
    pub fn new(budget: usize) -> Self {
        Self { entries: HashMap::new(), bytes: 0, budget, frame: 0, window: 0, shown: HashMap::new() }
    }

    /// Starts a frame of the window with this serial, replacing what it last showed.
    pub fn begin_frame(&mut self, window: u64) {
        self.frame += 1;
        self.window = window;
        self.shown.entry(window).or_default().clear();
    }

    /// Stops protecting what a closed window showed.
    pub fn forget_window(&mut self, window: u64) {
        self.shown.remove(&window);
    }

    pub fn set_budget(&mut self, budget: usize) {
        self.budget = budget;
    }

    /// Returns the texture and marks it as shown by the window drawing now.
    pub fn get(&mut self, key: &Key) -> Option<TextureId> {
        let entry = self.entries.get_mut(key)?;
        entry.last_used = self.frame;
        self.shown.entry(self.window).or_default().insert(*key);
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

    /// Evicts down to the budget, sparing what each window showed in its latest frame and
    /// the `awaited` keys it asked for then; returns the documents that lost entries.
    pub fn trim(&mut self, awaited: &HashSet<Key>) -> Vec<DocId> {
        let mut evicted = Vec::new();
        if self.bytes <= self.budget {
            return evicted;
        }
        let mut candidates: Vec<(u64, Key)> = self
            .entries
            .iter()
            .filter(|(k, _)| !awaited.contains(*k) && !self.shown.values().any(|shown| shown.contains(*k)))
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

#[cfg(test)]
mod tests {
    use eframe::egui::{ColorImage, Context, TextureOptions};

    use super::*;

    fn tile(doc: DocId, page: u32) -> Key {
        Key::Tile { doc, page, scale: 100, tx: 0, ty: 0 }
    }

    fn insert(cache: &mut TextureCache, ctx: &Context, key: Key) {
        let texture = ctx.load_texture("t", ColorImage::filled([1, 1], Default::default()), TextureOptions::LINEAR);
        cache.insert(key, texture, 1);
    }

    /// A window that stops drawing keeps what it shows, but no longer stops other windows'
    /// old tiles from being evicted.
    #[test]
    fn idle_window_protects_only_what_it_shows() {
        let ctx = Context::default();
        let mut cache = TextureCache::new(3);
        cache.begin_frame(1);
        insert(&mut cache, &ctx, tile(1, 0));
        cache.get(&tile(1, 0));

        // Window 2 scrolls through ten pages while window 1 draws nothing.
        for page in 0..10 {
            cache.begin_frame(2);
            insert(&mut cache, &ctx, tile(2, page));
            cache.get(&tile(2, page));
            cache.trim(&HashSet::new());
            assert!(cache.bytes() <= 3, "{} bytes over a budget of 3 at page {page}", cache.bytes());
        }
        assert!(cache.contains(&tile(1, 0)), "idle window's tile was evicted");
        assert!(cache.contains(&tile(2, 9)), "visible tile was evicted");

        cache.forget_window(1);
        cache.set_budget(1);
        cache.trim(&HashSet::new());
        assert!(!cache.contains(&tile(1, 0)), "closed window's tile is still protected");
        assert!(cache.contains(&tile(2, 9)));
    }

    /// A tile rendered for a window arrives during another window's frame, which trims the
    /// cache before the window it's for draws again.
    #[test]
    fn awaited_tile_survives_until_drawn() {
        let ctx = Context::default();
        let mut cache = TextureCache::new(1);
        // Window 2 shows a tile and waits for another; window 1's tile fills the budget.
        cache.begin_frame(2);
        insert(&mut cache, &ctx, tile(2, 0));
        cache.get(&tile(2, 0));
        let awaited = HashSet::from([tile(2, 1)]);
        cache.begin_frame(1);
        insert(&mut cache, &ctx, tile(1, 0));
        cache.get(&tile(1, 0));

        // The awaited tile arrives during window 1's frame.
        insert(&mut cache, &ctx, tile(2, 1));
        cache.trim(&awaited);
        assert!(cache.contains(&tile(2, 1)), "awaited tile was evicted before its window drew it");

        // Window 2 draws it; its request is no longer outstanding.
        cache.begin_frame(2);
        cache.get(&tile(2, 1));
        cache.trim(&HashSet::new());
        assert!(cache.contains(&tile(2, 1)));
        assert!(!cache.contains(&tile(2, 0)), "tile window 2 no longer shows was kept");
    }
}
