//! A tab's text layers (see [`crate::selection`]) and its selection.
//!
//! A page's layer is only read once the pointer moves over it, a selection action needs it,
//! or a selection shows on it, so reading and scrolling cost nothing. Measured with
//! `benches/text_spike.rs`: the first read of a page parses it (2–23 ms), its boxes then take
//! 1–12 ms (1.5 µs a character), and a layer takes 10–85 KB.
//!
//! The layers are read by a worker thread with its own PDFKit document, started when a page
//! is wanted. It reopens the document every [`REOPEN_PAGES`] pages, as search does, and
//! exits [`LINGER`] after its last job, closing it.
//!
//! Memory: layers are kept within [`MAX_LAYERS`] and [`MAX_BYTES`], those farthest from the
//! page shown going first, except for the pages on screen and at the ends of the selection
//! (or of an action waiting for text). Those are always kept, so at most a screenful of
//! pages goes over the budget; pages too small on screen to need a layer
//! ([`MIN_LAYER_HEIGHT`]) aren't read. Requests for pages that are neither are dropped
//! from the queue, which bounds it and the results waiting to be taken by the same pages.
//! A background tab keeps no layers.
//!
//! Selecting is asynchronous: the view reports what the pointer does ([`TextLayers::begin`],
//! [`TextLayers::update`], [`TextLayers::finish`], ...) and the action resolves into a
//! selection once the layers of the pages it touches are in, even after the press is over.
//! A newer action replaces one still waiting.
//!
//! Copying uses the layers when they cover the selection. Otherwise the worker reads the
//! text one page at a time, between reads of layers, which stay interactive. Each copy has an
//! id, and only the latest one reaches the clipboard; a newer copy, a background tab or a
//! closed one cancel the worker's copy between pages.

use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use eframe::egui::ahash::{HashMap, HashSet};
use eframe::egui::{self, Pos2};

use crate::engine::{TextDocument, autoreleasepool};
use crate::search::REOPEN_PAGES;
use crate::selection::{Caret, Selection, TextLayer, Unit};

/// Layers kept per tab, beyond those on screen or at the ends of the selection.
pub const MAX_LAYERS: usize = 32;
/// Bytes of layers kept per tab, beyond those on screen or at the ends of the selection.
pub const MAX_BYTES: usize = 4 * 1024 * 1024;
/// Height on screen, in points, below which a page inside the selection is tinted whole
/// instead of having its layer read to highlight its text.
pub const MIN_LAYER_HEIGHT: f32 = 150.0;
/// How long an idle worker keeps its document open for the next page, before exiting.
const LINGER: Duration = Duration::from_secs(2);

/// A point on a page: the page, and fractions of it with a top-left origin.
pub type At = (u32, Pos2);

/// Between the UI and the worker.
#[derive(Default)]
struct Inbox {
    closed: bool,
    /// The worker has exited: a new one is needed for more work.
    exited: bool,
    /// Pages whose layers are wanted, in the order asked for.
    queue: Vec<u32>,
    /// The copy wanted, with its id; `None` cancels the one under way.
    copy: Option<(u64, Selection)>,

    // Results, taken by the UI.
    read: Vec<(u32, TextLayer)>,
    copied: Option<(u64, String)>,
}

type Channel = Arc<(Mutex<Inbox>, Condvar)>;

/// A selection action, resolved once the layers of the pages it touches are in.
#[derive(Clone, Copy, Debug)]
enum Action {
    /// Select from `anchor` to `focus` by `unit`: a drag, or a double or triple click.
    Span { anchor: At, focus: At, unit: Unit },
    /// Move the selection's focus to the caret nearest a point: a shift-click.
    Extend(At),
}

impl Action {
    fn pages(&self) -> [u32; 2] {
        match *self {
            Action::Span { anchor, focus, .. } => [anchor.0, focus.0],
            Action::Extend(at) => [at.0, at.0],
        }
    }
}

pub struct TextLayers {
    path: PathBuf,
    layers: HashMap<u32, TextLayer>,
    /// Bytes `layers` hold.
    bytes: usize,
    /// Pages asked for whose layers haven't arrived.
    requested: HashSet<u32>,
    channel: Option<Channel>,
    selection: Option<Selection>,
    /// The latest action, until it resolves with the pointer released.
    action: Option<Action>,
    /// The pointer is dragging out `action`.
    dragging: bool,
    /// Id of the latest copy; results of older ones are dropped.
    copy_id: u64,
    /// Text copied, for the window to put on the clipboard.
    copied: Option<String>,
}

impl TextLayers {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            layers: HashMap::default(),
            bytes: 0,
            requested: HashSet::default(),
            channel: None,
            selection: None,
            action: None,
            dragging: false,
            copy_id: 0,
            copied: None,
        }
    }

    pub fn layer(&self, page: u32) -> Option<&TextLayer> {
        self.layers.get(&page)
    }

    pub fn selection(&self) -> Option<Selection> {
        self.selection
    }

    /// The pointer is dragging out a selection.
    pub fn dragging(&self) -> bool {
        self.dragging
    }

    /// Bytes of layers held.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Whether a worker is attached (it may have just exited).
    pub fn has_worker(&self) -> bool {
        self.channel.is_some()
    }

    /// Asks for `page`'s layer, if it isn't in or on its way.
    pub fn want(&mut self, ctx: &egui::Context, page: u32) {
        if self.layers.contains_key(&page) || !self.requested.insert(page) {
            return;
        }
        self.send(ctx, |inbox| inbox.queue.push(page));
    }

    /// Takes in what the worker has done, resolving a waiting action. Returns whether
    /// anything arrived.
    pub fn receive(&mut self) -> bool {
        let Some(channel) = &self.channel else { return false };
        let (read, copied, exited) = {
            let mut inbox = channel.0.lock().unwrap();
            (std::mem::take(&mut inbox.read), inbox.copied.take(), inbox.exited)
        };
        if exited {
            self.channel = None;
        }
        let changed = !read.is_empty() || copied.is_some();
        for (page, layer) in read {
            // A request dropped in the meantime is dropped.
            if self.requested.remove(&page) {
                self.bytes += layer.bytes();
                self.layers.insert(page, layer);
            }
        }
        if let Some((id, text)) = copied
            && id == self.copy_id
        {
            self.copied = Some(text);
        }
        if changed {
            self.resolve();
        }
        changed
    }

    /// Once a frame: drops queued requests for pages neither in `visible` nor needed by the
    /// selection or an action, then layers beyond the budget, farthest from `near` first.
    pub fn tidy(&mut self, near: u32, visible: &[u32]) {
        let mut keep: Vec<u32> = visible.to_vec();
        keep.extend(self.selection.iter().flat_map(|s| [s.start().page, s.end().page]));
        keep.extend(self.action.iter().flat_map(Action::pages));
        if let Some(channel) = &self.channel {
            let mut inbox = channel.0.lock().unwrap();
            inbox.queue.retain(|p| {
                let kept = keep.contains(p);
                if !kept {
                    self.requested.remove(p);
                }
                kept
            });
        }
        while self.layers.len() > MAX_LAYERS || self.bytes > MAX_BYTES {
            let victim = self.layers.keys().copied().filter(|p| !keep.contains(p)).max_by_key(|p| p.abs_diff(near));
            let Some(victim) = victim else { break };
            self.bytes -= self.layers.remove(&victim).map_or(0, |l| l.bytes());
        }
    }

    /// Lets go of every layer and stops the worker, cancelling its copy, as when the tab
    /// goes to the background. The selection stays; an action still waiting is dropped.
    pub fn clear(&mut self) {
        self.close();
        self.layers.clear();
        self.bytes = 0;
        self.requested.clear();
        self.action = None;
        self.dragging = false;
    }

    /// A press at `at` starts selecting by `unit`, replacing the selection.
    pub fn begin(&mut self, ctx: &egui::Context, at: At, unit: Unit) {
        self.selection = None;
        self.action = Some(Action::Span { anchor: at, focus: at, unit });
        self.dragging = true;
        self.want(ctx, at.0);
        self.resolve();
    }

    /// The press being dragged is now at `at`.
    pub fn update(&mut self, ctx: &egui::Context, at: At) {
        if let (true, Some(Action::Span { focus, .. })) = (self.dragging, &mut self.action) {
            *focus = at;
            self.want(ctx, at.0);
            self.resolve();
        }
    }

    /// The press is released. Its selection resolves now, or once its pages' text is in.
    pub fn finish(&mut self) {
        self.dragging = false;
        self.resolve();
    }

    /// Selects by `unit` around `at` at once, as a double or triple click does.
    pub fn select(&mut self, ctx: &egui::Context, at: At, unit: Unit) {
        self.begin(ctx, at, unit);
        self.finish();
    }

    /// Moves the end of the selection to the caret nearest `at`, as a shift-click does; with
    /// no selection, as a click does.
    pub fn extend(&mut self, ctx: &egui::Context, at: At) {
        match &mut self.action {
            // A selection still waiting for its text is extended instead.
            Some(Action::Span { focus, .. }) => *focus = at,
            _ if self.selection.is_some() => self.action = Some(Action::Extend(at)),
            _ => return self.clear_selection(),
        }
        self.want(ctx, at.0);
        self.resolve();
    }

    /// Selects every page's text, `pages` of them.
    pub fn select_all(&mut self, pages: u32) {
        self.action = None;
        self.dragging = false;
        self.selection =
            (pages > 0).then(|| Selection { anchor: Caret { page: 0, index: 0 }, focus: Caret::page_end(pages - 1) });
    }

    pub fn clear_selection(&mut self) {
        self.action = None;
        self.dragging = false;
        self.selection = None;
    }

    /// Sets the selection from the action, if the layers it needs are in; once the pointer
    /// is released, that completes the action.
    fn resolve(&mut self) {
        let Some(action) = self.action else { return };
        let resolved = match action {
            Action::Span { anchor, focus, unit } => {
                let (Some(a), Some(f)) = (self.layers.get(&anchor.0), self.layers.get(&focus.0)) else { return };
                let a = a.range_at(anchor.1, unit);
                let f = f.range_at(focus.1, unit);
                let caret = |page, index| Caret { page, index };
                let (a_start, a_end) = (caret(anchor.0, a.start), caret(anchor.0, a.end));
                let (f_start, f_end) = (caret(focus.0, f.start), caret(focus.0, f.end));
                // Whole words or lines at both ends, however the pointer moves.
                if f_start < a_start {
                    Selection { anchor: a_end, focus: f_start }
                } else {
                    Selection { anchor: a_start, focus: f_end.max(a_end) }
                }
            }
            Action::Extend(at) => {
                let (Some(layer), Some(selection)) = (self.layers.get(&at.0), self.selection) else { return };
                let focus = Caret { page: at.0, index: layer.range_at(at.1, Unit::Char).start };
                Selection { focus, ..selection }
            }
        };
        self.selection = Some(resolved);
        if !self.dragging {
            self.action = None;
            if resolved.is_empty() {
                self.selection = None;
            }
        }
    }

    /// Copies the selection: at once if its pages' layers are in, otherwise once the
    /// worker has read their text ([`Self::take_copied`]). Replaces any copy under way.
    pub fn copy(&mut self, ctx: &egui::Context) {
        let Some(selection) = self.selection.filter(|s| !s.is_empty()) else { return };
        self.copy_id += 1;
        if selection.pages().all(|p| self.layers.contains_key(&p)) {
            let mut text = String::new();
            for p in selection.pages() {
                let layer = &self.layers[&p];
                append(&mut text, &selection.range_on(p, layer.len()).map(|r| layer.text(r)).unwrap_or_default());
            }
            self.copied = Some(text);
            self.with_inbox(|inbox| inbox.copy = None);
        } else {
            let id = self.copy_id;
            self.send(ctx, |inbox| inbox.copy = Some((id, selection)));
        }
    }

    /// Text copied, to put on the clipboard.
    pub fn take_copied(&mut self) -> Option<String> {
        self.copied.take()
    }

    /// Runs `f` on the worker's inbox, if there is a worker.
    fn with_inbox(&self, f: impl FnOnce(&mut Inbox)) {
        if let Some(channel) = &self.channel {
            let (lock, cvar) = &**channel;
            f(&mut lock.lock().unwrap());
            cvar.notify_one();
        }
    }

    /// Runs `f` on the worker's inbox, starting a worker if there's none (or it has exited).
    fn send(&mut self, ctx: &egui::Context, f: impl FnOnce(&mut Inbox)) {
        if let Some(channel) = &self.channel {
            let (lock, cvar) = &**channel;
            let mut inbox = lock.lock().unwrap();
            if !inbox.exited {
                f(&mut inbox);
                cvar.notify_one();
                return;
            }
        }
        // Results the exited worker left are taken before it's replaced.
        self.receive();
        let mut inbox = Inbox::default();
        f(&mut inbox);
        let channel: Channel = Arc::new((Mutex::new(inbox), Condvar::new()));
        let (worker, path, ctx) = (channel.clone(), self.path.clone(), ctx.clone());
        std::thread::Builder::new()
            .name("pdf-text".into())
            .spawn(move || Worker { path, ctx, channel: worker, doc: None, read: 0, copying: None }.run())
            .expect("spawn text thread");
        self.channel = Some(channel);
    }

    fn close(&mut self) {
        self.with_inbox(|inbox| inbox.closed = true);
        self.channel = None;
    }
}

impl Drop for TextLayers {
    fn drop(&mut self) {
        self.close();
    }
}

/// Adds a page's text to `text`, on a new line.
fn append(text: &mut String, page: &str) {
    if page.is_empty() {
        return;
    }
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(page);
}

/// A copy the worker is reading, a page at a time.
struct Copying {
    id: u64,
    selection: Selection,
    next: u32,
    text: String,
}

enum Job {
    Read(u32),
    CopyPage,
}

struct Worker {
    path: PathBuf,
    ctx: egui::Context,
    channel: Channel,
    doc: Option<TextDocument>,
    /// Pages read since the document was (re)opened.
    read: usize,
    copying: Option<Copying>,
}

impl Worker {
    fn run(mut self) {
        loop {
            let channel = self.channel.clone();
            let job = {
                let (lock, cvar) = &*channel;
                let mut inbox = lock.lock().unwrap();
                loop {
                    if inbox.closed {
                        break None;
                    }
                    // Follow the latest copy: a new one restarts, none cancels.
                    match inbox.copy {
                        None => self.copying = None,
                        Some((id, selection)) if self.copying.as_ref().is_none_or(|c| c.id != id) => {
                            let next = selection.start().page;
                            self.copying = Some(Copying { id, selection, next, text: String::new() });
                        }
                        Some(_) => {}
                    }
                    // Layers first: they're for what the pointer is doing now.
                    if !inbox.queue.is_empty() {
                        break Some(Job::Read(inbox.queue.remove(0)));
                    }
                    if self.copying.is_some() {
                        break Some(Job::CopyPage);
                    }
                    let (guard, wait) = cvar.wait_timeout(inbox, LINGER).unwrap();
                    inbox = guard;
                    if wait.timed_out() && inbox.queue.is_empty() && inbox.copy.is_none() {
                        inbox.exited = true;
                        break None;
                    }
                }
            };
            let Some(job) = job else { break };
            match job {
                Job::Read(page) => {
                    // Each page in its own pool, so what PDFKit autoreleases goes with it.
                    let layer = autoreleasepool(|| self.layer(page));
                    channel.0.lock().unwrap().read.push((page, layer));
                    self.ctx.request_repaint();
                }
                Job::CopyPage => {
                    let mut copying = self.copying.take().unwrap();
                    let page = autoreleasepool(|| self.text(copying.next, copying.selection));
                    append(&mut copying.text, &page);
                    copying.next += 1;
                    if copying.next < copying.selection.end().page + 1 {
                        self.copying = Some(copying);
                        continue;
                    }
                    let mut inbox = channel.0.lock().unwrap();
                    if inbox.copy.is_some_and(|(id, _)| id == copying.id) {
                        inbox.copy = None;
                        inbox.copied = Some((copying.id, copying.text));
                        drop(inbox);
                        self.ctx.request_repaint();
                    }
                }
            }
        }
        if let Some(doc) = self.doc.take() {
            doc.close();
        }
    }

    /// The document, reopened every [`REOPEN_PAGES`] pages so PDFKit lets go of those it
    /// parsed; `None` if it can't be opened (not a PDF, or gone).
    fn doc(&mut self) -> Option<&TextDocument> {
        if self.read >= REOPEN_PAGES
            && let Some(doc) = self.doc.take()
        {
            doc.close();
        }
        if self.doc.is_none() {
            self.doc = TextDocument::open(&self.path);
            self.read = 0;
        }
        self.read += 1;
        self.doc.as_ref()
    }

    fn layer(&mut self, page: u32) -> TextLayer {
        let Some(page) = self.doc().and_then(|d| d.page(page)) else { return TextLayer::empty() };
        let (units, rects) = page.boxes();
        TextLayer::new(units, rects)
    }

    /// The selected text of `page`.
    fn text(&mut self, page: u32, selection: Selection) -> String {
        let Some(text) = self.doc().and_then(|d| d.page(page)).and_then(|p| p.text()) else { return String::new() };
        let units: Vec<u16> = text.encode_utf16().collect();
        selection
            .range_on(page, units.len() as u32)
            .map(|r| String::from_utf16_lossy(&units[r.start as usize..r.end as usize]))
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_go_on_separate_lines() {
        let mut text = String::new();
        for page in ["one\n", "", "two", "three"] {
            append(&mut text, page);
        }
        assert_eq!(text, "one\ntwo\nthree");
    }
}
