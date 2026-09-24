//! Text search (⌘F).
//!
//! While a tab's find bar is open, a worker thread reads the text of every page through its
//! own PDFKit document, starting at the page you're on and wrapping. PDFKit keeps each page it
//! has read parsed until its document is released (a whole textbook reaches hundreds of MB), so
//! the worker reopens it every [`REOPEN_PAGES`] distinct pages it touches, for text or for
//! highlight geometry alike. Measured with `benches/search_spike.rs`: that holds extraction at
//! a few MB above the text itself.
//!
//! The text is kept folded (see [`crate::text`]) within [`CACHE_BUDGET`], so changing the query
//! only rescans memory. Pages pushed out of the cache stay counted for the current query and
//! are only read again for a new query or for highlights.
//!
//! Each page read is also saved to disk ([`crate::search_index`]), and read back from there
//! instead of from PDFKit: by a later search of the same document, even after a relaunch, and
//! for pages pushed out of the cache. A document whose text is all saved is searched without
//! opening it in PDFKit at all, except for highlights.
//!
//! The UI holds a match count per page, never a list of every match: navigation walks the
//! counts, and highlight rectangles are only computed for the current match and visible pages,
//! in batches. Between pages the worker takes, in order: highlight geometry, matching the
//! cached text against a new query, reading the next page.
//!
//! Closing the bar drops the [`Search`]: the UI forgets its results at once and the worker
//! stops after its current PDFKit call, releasing its document and cache as it exits. Every
//! result carries its query's generation, and stale ones are dropped when they arrive.
//!
//! In the background, a search keeps only what the UI holds: the query, the per-page counts,
//! the current match and the highlights on screen. A tab switched away from pauses its
//! worker (which lets go of its document); once the tab's grace period ends the worker and
//! its cache go ([`Search::suspend`]). A new worker starts when something needs one: a new
//! query, highlights for pages not yet covered, a step waiting for pages to be counted, or
//! the tab coming back with pages still to count. It's told which pages are counted, so it
//! doesn't read them again. When the app sleeps ([`Search::sleep`]), nothing restarts a
//! worker until you use the find bar again.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::Duration;

use eframe::egui::{self, Rect, pos2};
use objc2::AnyThread;
use objc2::rc::{Retained, autoreleasepool};
use objc2_foundation::{NSRange, NSURL};
use objc2_pdf_kit::{PDFDocument, PDFPage};

use crate::engine::PageGeometry;
use crate::search_index::{self, Index, Store};
use crate::text::{PageText, Query};

/// Distinct pages the worker touches before reopening its document.
pub const REOPEN_PAGES: usize = 16;
/// Bytes of folded text and offset maps kept per search.
pub const CACHE_BUDGET: usize = 16 * 1024 * 1024;
/// Matches per page that get highlighted; the rest are still counted and reachable.
const MAX_HIGHLIGHTS: u32 = 2000;
/// Matches whose rectangles are computed in one go.
const GEOMETRY_BATCH: u32 = 128;
/// Pages matched against a new query between other jobs.
const MATCH_CHUNK: usize = 256;
/// Seconds after the last keystroke before the query is searched for.
const DEBOUNCE: f64 = 0.1;

/// A match: the `index`th on `page`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hit {
    pub page: u32,
    pub index: u32,
}

/// Geometry the UI wants from matches `first..first + count` of `page`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Want {
    generation: u64,
    page: u32,
    first: u32,
    count: u32,
    kind: WantKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WantKind {
    /// Highlights for the page, extending those it has.
    Page,
    /// The current match's rectangles (`count` is 1).
    Current,
    /// The first match at or below `y` on the page, in 1/65536ths of its height.
    Locate { y: u32 },
}

struct Highlights {
    want: Want,
    /// Match index and its rectangle on the page (fractions, top-left origin); a match
    /// broken across lines has one per line.
    rects: Vec<(u32, Rect)>,
    /// For [`WantKind::Locate`]: the first match at or below the point, or the page's
    /// match count if none is.
    located: Option<u32>,
}

/// Between the UI and the worker.
#[derive(Default)]
struct Inbox {
    closed: bool,
    /// The tab is in the background: take no work, hold no document.
    paused: bool,
    /// The latest query and its generation; `None` searches for nothing (the worker still
    /// reads ahead so the first keystrokes are fast).
    query: (u64, Option<Query>),
    /// The page being read, which the cache keeps text nearest to.
    focus: u32,
    /// Highlight geometry wanted, most urgent first. Replaced every frame.
    geometry: Vec<Want>,
    busy: Option<Want>,

    // Results, taken by the UI.
    counts_generation: u64,
    counts: Vec<(u32, u32)>,
    highlights: Vec<Highlights>,
    /// Pages read at least once, and those with any text.
    scanned: u32,
    with_text: u32,
    cache_bytes: usize,
    /// Pages read from the disk index and extracted with PDFKit since the UI last took them.
    loaded: u32,
    extracted: u32,
}

type Channel = Arc<(Mutex<Inbox>, Condvar)>;

/// Numbers for benchmarks and the status line.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub pages: usize,
    /// Pages read at least once.
    pub scanned: usize,
    pub with_text: usize,
    /// Pages counted for the current query.
    pub counted: usize,
    pub matches: u64,
    /// Bytes of text and offset maps the worker holds.
    pub cache_bytes: usize,
    /// Page reads served by the disk index, and those that took PDFKit, across workers.
    pub loaded: u64,
    pub extracted: u64,
    /// Whether a worker is attached.
    pub worker: bool,
}

/// Tells whether a search's worker thread is still running, after the search is dropped.
pub struct WorkerProbe(Weak<(Mutex<Inbox>, Condvar)>);

impl WorkerProbe {
    pub fn running(&self) -> bool {
        self.0.strong_count() > 0
    }
}

/// Where a search for the next or previous match continues from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Seek {
    /// Forward: matches at or after (`page`, `index`); backward: at or before it, where
    /// `index` `u32::MAX` means the page's last.
    from: Hit,
    forward: bool,
}

/// A search for a match waiting to be resolved.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Pending {
    /// From a point on a page (`y`, 0 = top, 1 = bottom), before its matches are placed.
    Start {
        page: u32,
        y: f32,
        forward: bool,
    },
    Seek(Seek),
}

/// Where a search starts from: a page and how far down it, as for [`Pending::Start`].
pub type Anchor = (usize, f32);

#[derive(Debug, PartialEq, Eq)]
enum Found {
    Hit(Hit),
    /// A page that comes first in the search direction hasn't been counted yet.
    Wait,
    None,
}

/// The nearest match from `seek.from` in its direction, in document order, wrapping around.
fn seek(counts: &[Option<u32>], seek: Seek) -> Found {
    let n = counts.len();
    if n == 0 {
        return Found::None;
    }
    let Seek { from, forward } = seek;
    for k in 0..=n {
        let page = if forward { (from.page as usize + k) % n } else { (from.page as usize + n * 2 - k) % n };
        let Some(count) = counts[page] else { return Found::Wait };
        let hit = |index| Found::Hit(Hit { page: page as u32, index });
        let wrapped = k == n;
        match (forward, k) {
            // The starting page: matches from the cursor on (back: up to it).
            (true, 0) if from.index < count => return hit(from.index),
            (false, 0) if count > 0 && from.index != u32::MAX => return hit(from.index.min(count - 1)),
            (false, 0) if count > 0 => return hit(count - 1),
            (_, 0) => {}
            // Back at the starting page: the matches on the other side of the cursor.
            (true, _) if wrapped => return if from.index > 0 && count > 0 { hit(0) } else { Found::None },
            (false, _) if wrapped => {
                return if from.index != u32::MAX && count > from.index + 1 { hit(count - 1) } else { Found::None };
            }
            _ if count > 0 => return hit(if forward { 0 } else { count - 1 }),
            _ => {}
        }
    }
    Found::None
}

/// Highlight rectangles of one page for the current query, at most [`MAX_HIGHLIGHTS`]
/// matches' worth.
#[derive(Default)]
struct PageHighlights {
    /// Matches `0..covered` have their rectangles.
    covered: u32,
    rects: Vec<(u32, Rect)>,
}

/// A tab's search, while its find bar is open. Dropping it stops the worker.
pub struct Search {
    /// The find field's text.
    pub query: String,
    ctx: egui::Context,
    path: PathBuf,
    budget: usize,
    /// Where page text is saved; `None` keeps it in memory only.
    store: Option<Store>,
    /// The worker, while there is one.
    channel: Option<Channel>,
    paused: bool,
    /// Stopped while the app sleeps: no worker starts until the find bar is used again.
    dormant: bool,
    /// The page shown, where a new worker starts reading.
    shown: usize,
    page_count: usize,
    /// When the field last changed, until its query is searched for.
    edited_at: Option<f64>,
    /// Generation of the query below; results for others are dropped.
    generation: u64,
    searched: Option<Query>,
    /// Matches per page for the current query, as pages get counted.
    counts: Vec<Option<u32>>,
    counted: usize,
    matches: u64,
    scanned: usize,
    with_text: usize,
    cache_bytes: usize,
    loaded: u64,
    extracted: u64,
    current: Option<Hit>,
    /// The current match's rectangles, when it's beyond its page's highlights.
    current_rects: Option<(Hit, Vec<Rect>)>,
    /// A search for a match, waiting for pages to be counted or matches placed.
    pending: Option<Pending>,
    /// Where a [`Pending::Start`] resolved to: page, y (as in [`WantKind::Locate`]), match.
    located: Option<(u32, u32, u32)>,
    /// Scroll to the current match once its rectangle arrives.
    reveal: bool,
    highlights: HashMap<u32, PageHighlights>,
    /// Focus the field and select its text on the next frame.
    pub focus: bool,
}

impl Search {
    /// Starts reading the document at `path` from page `start`, saving its text in `store`.
    pub fn new(ctx: &egui::Context, path: PathBuf, page_count: usize, start: usize, store: Option<Store>) -> Self {
        Self::with_budget(ctx, path, page_count, start, CACHE_BUDGET, store)
    }

    /// [`Self::new`] with a different cache budget, in bytes.
    pub fn with_budget(
        ctx: &egui::Context,
        path: PathBuf,
        page_count: usize,
        start: usize,
        budget: usize,
        store: Option<Store>,
    ) -> Self {
        let mut search = Self {
            query: String::new(),
            ctx: ctx.clone(),
            path,
            budget,
            store,
            channel: None,
            paused: false,
            dormant: false,
            shown: start,
            page_count,
            edited_at: None,
            generation: 0,
            searched: None,
            counts: vec![None; page_count],
            counted: 0,
            matches: 0,
            scanned: 0,
            with_text: 0,
            cache_bytes: 0,
            loaded: 0,
            extracted: 0,
            current: None,
            current_rects: None,
            pending: None,
            located: None,
            reveal: false,
            highlights: HashMap::new(),
            focus: true,
        };
        // Read ahead while the first query is typed.
        search.start_worker();
        search
    }

    /// Starts a worker if there is none, picking up from what the UI already knows.
    fn start_worker(&mut self) {
        if self.channel.is_some() || self.dormant {
            return;
        }
        let inbox = Inbox {
            paused: self.paused,
            query: (self.generation, self.searched.clone()),
            focus: self.shown as u32,
            counts_generation: self.generation,
            ..Inbox::default()
        };
        let channel: Channel = Arc::new((Mutex::new(inbox), Condvar::new()));
        let known = (0..self.page_count).map(|p| self.searched.is_some() && self.counts[p].is_some()).collect();
        let seed = Seed {
            path: self.path.clone(),
            ctx: self.ctx.clone(),
            budget: self.budget,
            store: self.store.clone(),
            start: self.shown,
            generation: self.generation,
            query: self.searched.clone(),
            known,
            with_text: self.with_text as u32,
        };
        let worker = channel.clone();
        std::thread::Builder::new()
            .name("pdf-search".into())
            .spawn(move || Worker::new(seed, worker).run())
            .expect("spawn search thread");
        self.channel = Some(channel);
    }

    /// Stops the worker, dropping its cache and document; what the UI holds stays.
    pub fn suspend(&mut self) {
        self.with_inbox(|inbox| {
            inbox.closed = true;
            inbox.geometry.clear();
        });
        self.channel = None;
        self.cache_bytes = 0;
    }

    /// Stops the worker while the app sleeps. Only using the find bar starts one again:
    /// typing, stepping, or ⌘F ([`Self::wake`]).
    pub fn sleep(&mut self) {
        self.suspend();
        self.dormant = true;
    }

    /// The find bar is being used: workers may start again.
    pub fn wake(&mut self) {
        self.dormant = false;
        if self.searched.is_some() && self.counted < self.page_count {
            self.start_worker();
        }
    }

    /// Pauses the worker while the tab is in the background, or resumes it. Back in front
    /// with pages still to count, a stopped worker is started again to count them.
    pub fn pause(&mut self, paused: bool) {
        self.paused = paused;
        self.with_inbox(|inbox| inbox.paused = paused);
        if !paused && self.searched.is_some() && self.counted < self.page_count {
            self.start_worker();
        }
    }

    /// Every page is counted for the field's text, with no typing waiting to be searched for.
    pub fn done(&self) -> bool {
        self.edited_at.is_none() && self.counted == self.page_count
    }

    pub fn current(&self) -> Option<Hit> {
        self.current
    }

    pub fn probe(&self) -> WorkerProbe {
        WorkerProbe(self.channel.as_ref().map_or_else(Weak::new, Arc::downgrade))
    }

    pub fn stats(&self) -> Stats {
        Stats {
            pages: self.page_count,
            scanned: self.scanned,
            with_text: self.with_text,
            counted: self.counted,
            matches: self.matches,
            cache_bytes: self.cache_bytes,
            loaded: self.loaded,
            extracted: self.extracted,
            worker: self.channel.is_some(),
        }
    }

    /// Runs `f` on the worker's inbox, if there is a worker.
    fn with_inbox<R>(&self, f: impl FnOnce(&mut Inbox) -> R) -> Option<R> {
        let (lock, cvar) = &**self.channel.as_ref()?;
        let r = f(&mut lock.lock().unwrap());
        cvar.notify_one();
        Some(r)
    }

    /// Call when the find field's text changed.
    pub fn edited(&mut self, now: f64) {
        self.dormant = false;
        self.edited_at = Some(now);
    }

    /// Searches for `query` right away (benchmarks; the field goes through [`Self::edited`]).
    pub fn set_query(&mut self, query: &str, anchor: Anchor) {
        self.dormant = false;
        self.query = query.to_owned();
        self.apply_query(anchor);
    }

    /// Runs once per frame: searches for the field's text once typing pauses, and takes a
    /// pending step once the pages it needs are counted. `anchor` is the top of the
    /// viewport, where the first match is looked for from; `focus` the page shown.
    pub fn update(&mut self, ctx: &egui::Context, anchor: Anchor, focus: usize) {
        let now = ctx.input(|i| i.time);
        if let Some(at) = self.edited_at {
            if now - at >= DEBOUNCE {
                self.edited_at = None;
                self.apply_query(anchor);
            } else {
                ctx.request_repaint_after(Duration::from_secs_f64(DEBOUNCE - (now - at)));
            }
        }
        self.update_pending();
        self.shown = focus;
        self.with_inbox(|inbox| inbox.focus = focus as u32);
    }

    fn apply_query(&mut self, anchor: Anchor) {
        let query = Query::new(&self.query);
        if query == self.searched {
            return;
        }
        self.generation += 1;
        self.searched = query.clone();
        self.counts.iter_mut().for_each(|c| *c = None);
        self.counted = 0;
        self.matches = 0;
        self.current = None;
        self.current_rects = None;
        self.located = None;
        self.highlights.clear();
        self.pending = query.is_some().then_some(Pending::Start { page: anchor.0 as u32, y: anchor.1, forward: true });
        if query.is_some() {
            self.start_worker();
        }
        let generation = self.generation;
        self.with_inbox(|inbox| {
            inbox.query = (generation, query);
            inbox.geometry.clear();
        });
    }

    fn select(&mut self, hit: Hit) {
        self.current = Some(hit);
        if self.current_rects.as_ref().is_some_and(|(h, _)| *h != hit) {
            self.current_rects = None;
        }
        self.reveal = true;
    }

    /// Steps to the next or previous match. Without a current match, the search starts
    /// from `anchor`, the top of the viewport.
    pub fn step(&mut self, forward: bool, anchor: Anchor) {
        self.dormant = false;
        if self.searched.is_none() {
            return;
        }
        // Already looking for one: carry on with that.
        if self.pending.is_some() {
            self.update_pending();
            return;
        }
        let n = self.page_count as u32;
        let from = match (self.current, forward) {
            (Some(h), true) => Hit { page: h.page, index: h.index + 1 },
            (Some(h), false) if h.index > 0 => Hit { page: h.page, index: h.index - 1 },
            (Some(h), false) => Hit { page: (h.page + n - 1) % n, index: u32::MAX },
            (None, _) => {
                self.pending = Some(Pending::Start { page: anchor.0 as u32, y: anchor.1, forward });
                self.update_pending();
                return;
            }
        };
        self.pending = Some(Pending::Seek(Seek { from, forward }));
        self.update_pending();
    }

    /// Takes the pending search as far as what's known allows; starts a worker if it has
    /// to wait for one.
    fn update_pending(&mut self) {
        let n = self.page_count as u32;
        if let Some(Pending::Start { page, y, forward }) = self.pending {
            let from = |index: u32| match (forward, index) {
                (true, _) => Seek { from: Hit { page, index }, forward },
                (false, 0) => Seek { from: Hit { page: (page + n - 1) % n, index: u32::MAX }, forward },
                (false, _) => Seek { from: Hit { page, index: index - 1 }, forward },
            };
            let y_key = locate_key(y);
            let resolved = match self.counts.get(page as usize).copied().flatten() {
                // At the top of the page, or no match on it to place.
                _ if y_key == 0 => Some(from(0)),
                Some(0) => Some(from(0)),
                Some(_) => self.located.filter(|l| (l.0, l.1) == (page, y_key)).map(|l| from(l.2)),
                None => None,
            };
            match resolved {
                Some(step) => self.pending = Some(Pending::Seek(step)),
                None => {
                    self.start_worker();
                    return;
                }
            }
        }
        let Some(Pending::Seek(step)) = self.pending else { return };
        match seek(&self.counts, step) {
            Found::Hit(hit) => {
                self.pending = None;
                self.select(hit);
            }
            Found::Wait => self.start_worker(),
            Found::None => self.pending = None,
        }
    }

    /// Takes in the worker's results; returns whether anything changed.
    pub fn receive(&mut self) -> bool {
        let generation = self.generation;
        let Some((counts, highlights, (scanned, with_text, cache_bytes), reads)) = self.with_inbox(|inbox| {
            let counts = if inbox.counts_generation == generation { std::mem::take(&mut inbox.counts) } else { vec![] };
            let highlights = std::mem::take(&mut inbox.highlights);
            let reads = (std::mem::take(&mut inbox.loaded), std::mem::take(&mut inbox.extracted));
            (counts, highlights, (inbox.scanned as usize, inbox.with_text as usize, inbox.cache_bytes), reads)
        }) else {
            return false;
        };
        self.loaded += reads.0 as u64;
        self.extracted += reads.1 as u64;
        // A new worker only knows the pages it has read itself.
        let (scanned, with_text) = (scanned.max(self.scanned), with_text.max(self.with_text));
        let changed = !counts.is_empty()
            || !highlights.is_empty()
            || (scanned, with_text, cache_bytes) != (self.scanned, self.with_text, self.cache_bytes);
        (self.scanned, self.with_text, self.cache_bytes) = (scanned, with_text, cache_bytes);
        for (page, count) in counts {
            let slot = &mut self.counts[page as usize];
            if slot.is_none() {
                self.counted += 1;
                self.matches += count as u64;
            }
            *slot = Some(count);
        }
        for h in highlights {
            self.absorb(h);
        }
        if changed {
            self.update_pending();
        }
        changed
    }

    /// Takes in one geometry result for the current query.
    fn absorb(&mut self, h: Highlights) {
        let want = h.want;
        if want.generation != self.generation {
            return;
        }
        match want.kind {
            WantKind::Page => {
                // Pages that scrolled away since were pruned; their geometry isn't kept.
                let Some(page) = self.highlights.get_mut(&want.page) else { return };
                if want.first == page.covered {
                    page.covered += want.count;
                    page.rects.extend(h.rects);
                }
            }
            WantKind::Current => {
                let hit = Hit { page: want.page, index: want.first };
                if self.current == Some(hit) {
                    self.current_rects = Some((hit, h.rects.into_iter().map(|(_, r)| r).collect()));
                }
            }
            WantKind::Locate { y } => {
                if let Some(index) = h.located {
                    self.located = Some((want.page, y, index));
                }
            }
        }
    }

    /// Whether the current match's rectangles are known.
    fn has_current_rects(&self, hit: Hit) -> bool {
        self.highlights.get(&hit.page).is_some_and(|p| hit.index < p.covered)
            || self.current_rects.as_ref().is_some_and(|(h, _)| *h == hit)
    }

    /// Asks for the geometry of the current match and of the matches on `visible` pages,
    /// and lets go of it for pages no longer shown.
    pub fn want_geometry(&mut self, visible: &[u32]) {
        if self.searched.is_none() {
            return;
        }
        let current = self.current;
        self.highlights.retain(|page, _| visible.contains(page) || current.is_some_and(|c| c.page == *page));
        let generation = self.generation;
        let mut wants = Vec::new();
        if let Some(Pending::Start { page, y, .. }) = self.pending
            && let Some(count) = self.counts.get(page as usize).copied().flatten()
        {
            wants.push(Want { generation, page, first: 0, count, kind: WantKind::Locate { y: locate_key(y) } });
        }
        if let Some(hit) = current
            && !self.has_current_rects(hit)
        {
            wants.push(Want { generation, page: hit.page, first: hit.index, count: 1, kind: WantKind::Current });
        }
        for &p in visible {
            let Some(count) = self.counts.get(p as usize).copied().flatten() else { continue };
            let page = self.highlights.entry(p).or_default();
            let limit = count.min(MAX_HIGHLIGHTS);
            if page.covered < limit {
                let count = (limit - page.covered).min(GEOMETRY_BATCH);
                wants.push(Want { generation, page: p, first: page.covered, count, kind: WantKind::Page });
            }
        }
        if !wants.is_empty() {
            self.start_worker();
        }
        self.with_inbox(|inbox| {
            wants.retain(|w| inbox.busy != Some(*w) && !inbox.highlights.iter().any(|h| h.want == *w));
            inbox.geometry = wants;
        });
    }

    /// Match rectangles on `page` (fractions of the page), and whether each is the current
    /// match.
    pub fn highlights(&self, page: u32) -> impl Iterator<Item = (Rect, bool)> + '_ {
        let current = self.current.filter(|c| c.page == page).map(|c| c.index);
        let on_page = self
            .highlights
            .get(&page)
            .into_iter()
            .flat_map(move |h| h.rects.iter().map(move |(i, r)| (*r, Some(*i) == current)));
        let beyond = self.current_rects.iter().filter(move |(h, _)| h.page == page && !self.has_page_rects(*h));
        on_page.chain(beyond.flat_map(|(_, rects)| rects.iter().map(|r| (*r, true))))
    }

    /// Whether `hit` is among its page's highlights.
    fn has_page_rects(&self, hit: Hit) -> bool {
        self.highlights.get(&hit.page).is_some_and(|p| hit.index < p.covered)
    }

    /// The current match's area on its page, once, after it was selected and its geometry
    /// arrived: the view should scroll to it. A match PDFKit gave no rectangle for reveals
    /// the top of its page.
    pub fn take_reveal(&mut self) -> Option<(usize, Rect)> {
        let hit = self.current.filter(|_| self.reveal)?;
        let rects: Vec<Rect> = if self.has_page_rects(hit) {
            self.highlights[&hit.page].rects.iter().filter(|(i, _)| *i == hit.index).map(|(_, r)| *r).collect()
        } else {
            self.current_rects.as_ref().filter(|(h, _)| *h == hit)?.1.clone()
        };
        let area =
            rects.into_iter().reduce(|a, b| a.union(b)).unwrap_or(Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 0.05)));
        self.reveal = false;
        Some((hit.page as usize, area))
    }

    /// "3 of 120", "Searching…", "No matches", and so on; `None` with nothing searched for.
    pub fn status(&self) -> Option<String> {
        self.searched.as_ref()?;
        let done = self.counted == self.page_count;
        let more = if done { "" } else { "+" };
        Some(match self.current {
            _ if done && self.matches == 0 && self.scanned == self.page_count && self.with_text == 0 => {
                "No searchable text".into()
            }
            _ if done && self.matches == 0 => "No matches".into(),
            None if self.matches == 0 => "Searching…".into(),
            None => format!("{}{more} matches", self.matches),
            Some(hit) => {
                let before = &self.counts[..hit.page as usize];
                match before.iter().copied().collect::<Option<Vec<u32>>>() {
                    Some(before) => {
                        let rank = before.iter().map(|&c| c as u64).sum::<u64>() + hit.index as u64 + 1;
                        format!("{rank} of {}{more}", self.matches)
                    }
                    None => format!("{}{more} matches", self.matches),
                }
            }
        })
    }
}

/// `y` (0 = top of a page, 1 = bottom) as a [`WantKind::Locate`] key.
fn locate_key(y: f32) -> u32 {
    (y.clamp(0.0, 1.0) * 65536.0) as u32
}

impl Drop for Search {
    fn drop(&mut self) {
        self.suspend();
    }
}

/// What a new worker starts from.
struct Seed {
    path: PathBuf,
    ctx: egui::Context,
    budget: usize,
    store: Option<Store>,
    /// The page shown, read first.
    start: usize,
    generation: u64,
    query: Option<Query>,
    /// Pages already counted for `query`.
    known: Vec<bool>,
    with_text: u32,
}

/// What the worker has for each page.
enum Slot {
    Unread,
    Cached(PageText),
    /// Read, but pushed out of the cache.
    Evicted,
}

/// Page text the worker keeps, within a byte budget. The byte count only changes here.
struct TextCache {
    slots: Vec<Slot>,
    bytes: usize,
    budget: usize,
}

impl TextCache {
    /// Pages already read (`read[p]`) start out evicted.
    fn new(read: &[bool], budget: usize) -> Self {
        let slots = read.iter().map(|&r| if r { Slot::Evicted } else { Slot::Unread }).collect();
        Self { slots, bytes: 0, budget }
    }

    /// Takes page `p`'s text out of the cache, if it's there.
    fn take(&mut self, p: usize) -> Option<PageText> {
        match std::mem::replace(&mut self.slots[p], Slot::Evicted) {
            Slot::Cached(text) => {
                self.bytes -= text.bytes();
                Some(text)
            }
            other => {
                self.slots[p] = other;
                None
            }
        }
    }

    /// Keeps `text` for page `p` if it fits the budget, first pushing out pages that
    /// `evictable` allows (those already counted), farthest from `focus` first. Otherwise
    /// the page is marked read but not kept.
    fn put(&mut self, p: usize, text: PageText, focus: usize, evictable: &[bool]) {
        let bytes = text.bytes();
        if let Slot::Cached(old) = std::mem::replace(&mut self.slots[p], Slot::Evicted) {
            self.bytes -= old.bytes();
        }
        while self.bytes + bytes > self.budget {
            let victim = (0..self.slots.len())
                .filter(|&q| q != p && evictable[q] && matches!(self.slots[q], Slot::Cached(_)))
                .max_by_key(|&q| q.abs_diff(focus));
            let Some(victim) = victim else { break };
            self.take(victim);
        }
        if self.bytes + bytes <= self.budget {
            self.bytes += bytes;
            self.slots[p] = Slot::Cached(text);
        }
    }
}

struct Worker {
    path: PathBuf,
    ctx: egui::Context,
    channel: Channel,
    /// Pages in reading order: from the start page on, wrapping.
    order: Vec<u32>,
    /// Position in `order` of the next page that may need reading for the current query.
    cursor: usize,
    /// Pages whose counts may be missing for the current query, from the start.
    match_cursor: usize,
    cache: TextCache,
    scanned: u32,
    with_text: u32,
    generation: u64,
    query: Option<Query>,
    counted: Vec<bool>,
    doc: Option<Retained<PDFDocument>>,
    /// Pages touched since the document was (re)opened.
    touched: Vec<u32>,
    failed: bool,
    store: Option<Store>,
    /// The document's saved text, opened when the worker starts.
    index: Option<Index>,
    /// Page reads from `index` and from PDFKit, not yet reported.
    loaded: u32,
    extracted: u32,
}

enum Job {
    Geometry(Want),
    Match,
    Read(u32),
}

impl Worker {
    /// Pages already counted count as read, and are only read again if their text is wanted.
    fn new(seed: Seed, channel: Channel) -> Self {
        let page_count = seed.known.len();
        let start = seed.start.min(page_count.saturating_sub(1));
        Self {
            path: seed.path,
            ctx: seed.ctx,
            channel,
            order: (start..page_count).chain(0..start).map(|p| p as u32).collect(),
            cursor: 0,
            match_cursor: 0,
            cache: TextCache::new(&seed.known, seed.budget),
            scanned: seed.known.iter().filter(|&&k| k).count() as u32,
            with_text: seed.with_text,
            generation: seed.generation,
            query: seed.query,
            counted: seed.known,
            doc: None,
            touched: Vec::new(),
            failed: false,
            store: seed.store,
            index: None,
            loaded: 0,
            extracted: 0,
        }
    }

    fn run(mut self) {
        if let Some(store) = &self.store {
            search_index::sweep(store);
            self.index = store.open(&self.path, self.cache.slots.len());
        }
        loop {
            let channel = self.channel.clone();
            let next = {
                let (lock, cvar) = &*channel;
                let mut inbox = lock.lock().unwrap();
                loop {
                    if inbox.closed {
                        break None;
                    }
                    if inbox.query.0 != self.generation {
                        self.generation = inbox.query.0;
                        self.query = inbox.query.1.clone();
                        self.counted.iter_mut().for_each(|c| *c = false);
                        self.cursor = 0;
                        self.match_cursor = 0;
                    }
                    // Geometry for an older query is dropped.
                    while inbox.geometry.first().is_some_and(|w| w.generation != self.generation) {
                        inbox.geometry.remove(0);
                    }
                    let job = if inbox.paused {
                        None
                    } else if inbox.geometry.is_empty() {
                        self.next_job()
                    } else {
                        let want = inbox.geometry.remove(0);
                        inbox.busy = Some(want);
                        Some(Job::Geometry(want))
                    };
                    if let Some(job) = job {
                        break Some((job, inbox.focus));
                    }
                    // Nothing to do (or paused) until the query changes or highlights are
                    // wanted: let go of the pages PDFKit holds parsed meanwhile.
                    if self.doc.is_some() {
                        autoreleasepool(|_| drop(self.doc.take()));
                        self.touched.clear();
                    }
                    inbox = cvar.wait(inbox).unwrap();
                }
            };
            let Some((job, focus)) = next else { break };
            let urgent = matches!(job, Job::Geometry(_));
            let (counts, highlights) = autoreleasepool(|_| self.run_job(job, focus));

            let (lock, _) = &*self.channel;
            let mut inbox = lock.lock().unwrap();
            inbox.busy = None;
            if inbox.counts_generation != self.generation {
                inbox.counts_generation = self.generation;
                inbox.counts.clear();
            }
            inbox.counts.extend(counts);
            inbox.highlights.extend(highlights);
            inbox.scanned = self.scanned;
            inbox.with_text = self.with_text;
            inbox.cache_bytes = self.cache.bytes;
            inbox.loaded += std::mem::take(&mut self.loaded);
            inbox.extracted += std::mem::take(&mut self.extracted);
            drop(inbox);
            // Progress while reading needn't redraw at the rate pages are read.
            if urgent {
                self.ctx.request_repaint();
            } else {
                self.ctx.request_repaint_after(Duration::from_millis(50));
            }
        }
        autoreleasepool(|_| drop(self.doc.take()));
        // Let go of the lock before sweeping, so this document's file counts too.
        drop(self.index.take());
        if let Some(store) = &self.store {
            search_index::sweep(store);
        }
    }

    /// Matching cached text first, then reading pages still needed.
    fn next_job(&mut self) -> Option<Job> {
        if self.query.is_some() {
            while self.match_cursor < self.cache.slots.len() {
                let p = self.match_cursor;
                if matches!(self.cache.slots[p], Slot::Cached(_)) && !self.counted[p] {
                    return Some(Job::Match);
                }
                self.match_cursor += 1;
            }
        }
        while self.cursor < self.order.len() {
            let p = self.order[self.cursor] as usize;
            let needed = match self.cache.slots[p] {
                Slot::Unread => true,
                Slot::Evicted => self.query.is_some() && !self.counted[p],
                Slot::Cached(_) => false,
            };
            if needed {
                return Some(Job::Read(p as u32));
            }
            self.cursor += 1;
        }
        None
    }

    fn run_job(&mut self, job: Job, focus: u32) -> (Vec<(u32, u32)>, Vec<Highlights>) {
        let mut counts = Vec::new();
        let mut highlights = Vec::new();
        match job {
            Job::Match => {
                let query = self.query.clone().unwrap();
                let end = (self.match_cursor + MATCH_CHUNK).min(self.cache.slots.len());
                for p in self.match_cursor..end {
                    if let Slot::Cached(text) = &self.cache.slots[p]
                        && !self.counted[p]
                    {
                        counts.push((p as u32, text.count(&query)));
                        self.counted[p] = true;
                    }
                }
                self.match_cursor = end;
            }
            Job::Read(p) => {
                let text = self.read(p);
                counts.extend(self.count(p, &text));
                self.cache.put(p as usize, text, focus as usize, &self.counted);
            }
            Job::Geometry(want) => {
                let p = want.page;
                let text = match self.cache.take(p as usize) {
                    Some(text) => text,
                    None => self.read(p),
                };
                counts.extend(self.count(p, &text));
                if let Some(query) = self.query.clone() {
                    let (rects, located) = match want.kind {
                        WantKind::Locate { y } => (vec![], Some(self.locate(p, &text, &query, y))),
                        WantKind::Page | WantKind::Current => (self.geometry(want, &text, &query), None),
                    };
                    highlights.push(Highlights { want, rects, located });
                }
                self.cache.put(p as usize, text, focus as usize, &self.counted);
            }
        }
        (counts, highlights)
    }

    /// Counts `p` for the current query, if it hasn't been.
    fn count(&mut self, p: u32, text: &PageText) -> Option<(u32, u32)> {
        let query = self.query.as_ref()?;
        (!std::mem::replace(&mut self.counted[p as usize], true)).then(|| (p, text.count(query)))
    }

    /// The PDFKit page, reopening the document first if it has touched enough others.
    fn page(&mut self, p: u32) -> Option<Retained<PDFPage>> {
        if self.failed {
            return None;
        }
        if !self.touched.contains(&p) && self.touched.len() >= REOPEN_PAGES {
            autoreleasepool(|_| drop(self.doc.take()));
            self.touched.clear();
        }
        if self.doc.is_none() {
            // The saved text is only good while the file is the one it came from.
            if self.index.as_ref().is_some_and(|i| !i.is_current()) {
                self.index = None;
            }
            self.doc = open(&self.path);
            self.failed = self.doc.is_none();
        }
        if !self.touched.contains(&p) {
            self.touched.push(p);
        }
        // SAFETY: plain PDFKit getter on a document this thread owns.
        unsafe { self.doc.as_ref()?.pageAtIndex(p as usize) }
    }

    /// Page `p`'s text: saved, or extracted with PDFKit and then saved.
    fn read(&mut self, p: u32) -> PageText {
        let saved = self.index.as_mut().and_then(|i| i.load(p as usize));
        let text = match saved {
            Some(text) => {
                self.loaded += 1;
                text
            }
            None => {
                let page = self.page(p);
                let source = page.and_then(|page| unsafe { page.string() }).map(|s| s.to_string());
                let text = PageText::new(source.as_deref().unwrap_or(""));
                self.extracted += 1;
                // A page PDFKit couldn't open isn't known to be empty.
                if !self.failed
                    && let Some(index) = &mut self.index
                {
                    index.save(p as usize, &text);
                }
                text
            }
        };
        if matches!(self.cache.slots[p as usize], Slot::Unread) {
            self.scanned += 1;
            self.with_text += !text.is_empty() as u32;
        }
        text
    }

    /// The first match on page `p` whose top is at or below `y` (a [`WantKind::Locate`]
    /// key), in text order; the page's match count if none is.
    fn locate(&mut self, p: u32, text: &PageText, query: &Query, y: u32) -> u32 {
        let starts: Vec<u32> = text.source_ranges(query, 0..u32::MAX).into_iter().map(|(start, _)| start).collect();
        let y = y as f32 / 65536.0;
        let Some(page) = self.page(p) else { return 0 };
        // SAFETY: plain PDFKit getters on objects this thread owns.
        let Some(cg) = (unsafe { page.pageRef() }) else { return 0 };
        let geometry = PageGeometry::of(&cg);
        starts
            .iter()
            .position(|&start| {
                let b = unsafe { page.characterBoundsAtIndex(start as isize) };
                let top = geometry
                    .to_fraction(b.origin.x, b.origin.y)
                    .y
                    .min(geometry.to_fraction(b.origin.x + b.size.width, b.origin.y + b.size.height).y);
                top >= y
            })
            .unwrap_or(starts.len()) as u32
    }

    /// Rectangles of matches `want.first..` on the page, one per line of each match.
    fn geometry(&mut self, want: Want, text: &PageText, query: &Query) -> Vec<(u32, Rect)> {
        let ranges = text.source_ranges(query, want.first..want.first + want.count);
        let Some(page) = self.page(want.page) else { return vec![] };
        // SAFETY: plain PDFKit getters on objects this thread owns.
        let Some(cg) = (unsafe { page.pageRef() }) else { return vec![] };
        let geometry = PageGeometry::of(&cg);
        let mut rects = Vec::new();
        for (i, (start, len)) in ranges.into_iter().enumerate() {
            let Some(selection) = (unsafe { page.selectionForRange(NSRange::new(start as usize, len as usize)) })
            else {
                continue;
            };
            let lines = unsafe { selection.selectionsByLine() };
            for line in 0..lines.count() {
                let b = unsafe { lines.objectAtIndex(line).boundsForPage(&page) };
                let corners = [
                    geometry.to_fraction(b.origin.x, b.origin.y),
                    geometry.to_fraction(b.origin.x + b.size.width, b.origin.y + b.size.height),
                ];
                rects.push((want.first + i as u32, Rect::from_two_pos(corners[0], corners[1])));
            }
        }
        rects
    }
}

fn open(path: &Path) -> Option<Retained<PDFDocument>> {
    let url = NSURL::from_file_path(path)?;
    // SAFETY: `url` is a valid file URL.
    unsafe { PDFDocument::initWithURL(PDFDocument::alloc(), &url) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(page: u32, index: u32) -> Found {
        Found::Hit(Hit { page, index })
    }

    fn fwd(page: u32, index: u32) -> Seek {
        Seek { from: Hit { page, index }, forward: true }
    }

    fn back(page: u32, index: u32) -> Seek {
        Seek { from: Hit { page, index }, forward: false }
    }

    #[test]
    fn seeks_forward_and_wraps() {
        let counts = [Some(2), Some(0), Some(3), Some(0)];
        assert_eq!(seek(&counts, fwd(0, 0)), hit(0, 0));
        assert_eq!(seek(&counts, fwd(0, 2)), hit(2, 0));
        assert_eq!(seek(&counts, fwd(2, 3)), hit(0, 0), "wraps past the end");
        assert_eq!(seek(&counts, fwd(1, 0)), hit(2, 0));
    }

    #[test]
    fn seeks_backward_and_wraps() {
        let counts = [Some(2), Some(0), Some(3), Some(0)];
        assert_eq!(seek(&counts, back(2, 1)), hit(2, 1));
        assert_eq!(seek(&counts, back(1, u32::MAX)), hit(0, 1));
        assert_eq!(seek(&counts, back(3, u32::MAX)), hit(2, 2));
        assert_eq!(seek(&counts, back(0, u32::MAX)), hit(0, 1));
        assert_eq!(seek(&counts, back(3, u32::MAX)), hit(2, 2));
    }

    #[test]
    fn a_single_match_wraps_to_itself() {
        let counts = [Some(0), Some(1), Some(0)];
        assert_eq!(seek(&counts, fwd(1, 1)), hit(1, 0));
        assert_eq!(seek(&counts, back(0, u32::MAX)), hit(1, 0));
    }

    #[test]
    fn waits_for_pages_in_between() {
        let counts = [Some(0), None, Some(4)];
        assert_eq!(seek(&counts, fwd(0, 0)), Found::Wait, "page 1 might have an earlier match");
        assert_eq!(seek(&counts, fwd(2, 0)), hit(2, 0), "no page comes first");
        assert_eq!(seek(&counts, back(2, u32::MAX)), hit(2, 3));
    }

    #[test]
    fn cache_counts_each_page_once() {
        let text = || PageText::new("some page text");
        let one = text().bytes();
        let counted = [true; 3];
        let mut cache = TextCache::new(&[false; 3], one * 2);
        cache.put(0, text(), 0, &counted);
        assert_eq!(cache.bytes, one);
        // A geometry request takes the text out and puts it back.
        for _ in 0..5 {
            let t = cache.take(0).unwrap();
            cache.put(0, t, 0, &counted);
        }
        assert_eq!(cache.bytes, one);
        cache.put(0, text(), 0, &counted);
        assert_eq!(cache.bytes, one, "replacing a page's text doesn't count it twice");
        cache.put(1, text(), 0, &counted);
        cache.put(2, text(), 0, &counted);
        assert_eq!(cache.bytes, one * 2, "the farthest counted page went");
        assert!(matches!(cache.slots[2], Slot::Cached(_)) && matches!(cache.slots[1], Slot::Evicted));
    }

    /// A search on a file that doesn't exist: its workers find no text, which is all these
    /// tests need.
    fn search(pages: usize) -> (egui::Context, Search) {
        let ctx = egui::Context::default();
        let search = Search::new(&ctx, PathBuf::from("/nonexistent.pdf"), pages, 0, None);
        (ctx, search)
    }

    #[test]
    fn only_the_find_bar_wakes_a_sleeping_search() {
        let (ctx, mut s) = search(3);
        s.set_query("x", (0, 0.5));
        s.sleep();
        assert!(!s.stats().worker);
        // What drawing and switching tabs do.
        s.update(&ctx, (1, 0.3), 1);
        s.want_geometry(&[0, 1]);
        s.pause(true);
        s.pause(false);
        s.receive();
        assert!(!s.stats().worker, "nothing passive starts a worker");
        s.step(true, (0, 0.0));
        assert!(s.stats().worker, "stepping does");

        s.sleep();
        s.wake();
        assert!(s.stats().worker, "so does ⌘F, with pages still to count");
        s.sleep();
        s.edited(0.0);
        s.update(&ctx, (0, 0.0), 0);
        assert!(s.stats().worker, "and typing");
    }

    #[test]
    fn stepping_past_the_highlight_cap_keeps_one_current_match() {
        let (_ctx, mut s) = search(1);
        s.set_query("x", (0, 0.0));
        s.sleep();
        let generation = s.generation;
        s.counts[0] = Some(MAX_HIGHLIGHTS + 100);
        let rect = Rect::from_min_max(pos2(0.0, 0.0), pos2(0.1, 0.1));
        s.highlights.insert(0, PageHighlights::default());
        s.absorb(Highlights {
            want: Want { generation, page: 0, first: 0, count: MAX_HIGHLIGHTS, kind: WantKind::Page },
            rects: (0..MAX_HIGHLIGHTS).map(|i| (i, rect)).collect(),
            located: None,
        });
        for index in MAX_HIGHLIGHTS - 5..MAX_HIGHLIGHTS + 20 {
            let hit = Hit { page: 0, index };
            s.select(hit);
            if !s.has_current_rects(hit) {
                s.absorb(Highlights {
                    want: Want { generation, page: 0, first: index, count: 1, kind: WantKind::Current },
                    rects: vec![(index, rect)],
                    located: None,
                });
            }
            assert!(s.highlights(0).count() <= MAX_HIGHLIGHTS as usize + 1);
            assert_eq!(s.highlights(0).filter(|(_, current)| *current).count(), 1);
            assert!(s.take_reveal().is_some());
        }
    }

    #[test]
    fn no_matches_anywhere() {
        assert_eq!(seek(&[Some(0), Some(0)], fwd(1, 0)), Found::None);
        assert_eq!(seek(&[Some(0), Some(0)], back(1, u32::MAX)), Found::None);
    }
}
