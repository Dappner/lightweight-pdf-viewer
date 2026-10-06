//! Background render threads: rasterize pages and read document structure (outline, links)
//! with the [`crate::engine`], and decide what to render next.
//!
//! Each thread takes one page's work at a time from a shared queue, so several threads draw
//! different pages at once (see [`crate::memory::Tuning::render_threads`]). A thread opens
//! its own copy of each document it works on, and keeps at most one per window open:
//! touching another one closes the least recently used, which frees its per-document caches
//! (decoded images, fonts, parsed content streams). CoreGraphics keeps every page it
//! has drawn in that cache until the document is released, so a document is also closed
//! once it has drawn a set number of distinct pages (see [`crate::memory::Tuning::pages_per_open`]), and
//! reopened in a millisecond or two on its next job.
//!
//! The UI replaces the whole render queue every frame with what it currently wants, so
//! work for pages that scrolled out of view is dropped without explicit cancellation.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};

use eframe::egui::{self, Color32, Rect, Vec2, vec2};

use crate::engine::{self, Bitmap, Document, Page};
use crate::grey;

pub type DocId = u64;

/// Tile edge length in physical pixels. A row of adjacent tiles is drawn at once (see
/// [`render_tiles`]), so smaller tiles would not add draws, only textures to upload and track.
pub const TILE: u32 = 512;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Key {
    /// `scale` is physical pixels per PDF point, in 1/256 steps (see [`scale_key`]).
    Tile {
        doc: DocId,
        page: u32,
        scale: u32,
        tx: u32,
        ty: u32,
    },
    Thumb {
        doc: DocId,
        page: u32,
        width: u32,
    },
    /// The page's link annotations.
    Links {
        doc: DocId,
        page: u32,
    },
}

impl Key {
    pub fn doc(&self) -> DocId {
        match *self {
            Key::Tile { doc, .. } | Key::Thumb { doc, .. } | Key::Links { doc, .. } => doc,
        }
    }

    fn page(&self) -> u32 {
        match *self {
            Key::Tile { page, .. } | Key::Thumb { page, .. } | Key::Links { page, .. } => page,
        }
    }
}

/// Where a link or outline entry points.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    /// `y` is the vertical position on the page, 0 = top and 1 = bottom, when specified.
    Page { page: u32, y: Option<f32> },
    #[cfg_attr(not(target_os = "macos"), allow(dead_code, reason = "the synthetic engine has no links"))]
    Url(String),
}

#[derive(Clone, Debug)]
pub struct Link {
    /// The clickable area as fractions of the displayed page (top-left origin).
    pub rect: Rect,
    pub target: Target,
}

#[derive(Clone, Debug)]
pub struct OutlineEntry {
    pub title: String,
    pub depth: usize,
    pub target: Option<Target>,
}

pub fn scale_key(pixels_per_point: f32) -> u32 {
    (pixels_per_point * 256.0).round().max(1.0) as u32
}

/// Size in physical pixels of a page rendered at `scale` (a [`scale_key`]).
pub fn page_pixels(size: Vec2, scale: u32) -> [u32; 2] {
    let s = scale as f32 / 256.0;
    [(size.x * s).round().max(1.0) as u32, (size.y * s).round().max(1.0) as u32]
}

pub fn thumb_height(size: Vec2, width: u32) -> u32 {
    (width as f32 * size.y / size.x).round().max(1.0) as u32
}

/// A page placed inside a snapshot bitmap, in snapshot pixels.
pub struct SnapshotPage {
    pub page: u32,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

pub struct SnapshotJob {
    pub doc: DocId,
    pub size: [u32; 2],
    pub pages: Vec<SnapshotPage>,
}

/// QOI-compressed RGBA viewport image, kept while the tab is in the background.
pub struct Snapshot {
    pub size: [u32; 2],
    pub qoi: Vec<u8>,
}

pub enum Output {
    Opened {
        doc: DocId,
        result: Result<Vec<Vec2>, String>,
    },
    Image {
        key: Key,
        size: [usize; 2],
        pixels: Pixels,
    },
    Failed(Key),
    /// `None` if the viewport could not be drawn.
    Snapshot {
        doc: DocId,
        snapshot: Option<Snapshot>,
    },
    Links {
        doc: DocId,
        page: u32,
        links: Vec<Link>,
    },
    Outline {
        doc: DocId,
        entries: Vec<OutlineEntry>,
    },
}

pub enum Pixels {
    /// Premultiplied, in the layout egui uploads, so taking them in on the UI thread doesn't
    /// copy them.
    Rgba(Vec<Color32>),
    /// A tile without colour, one byte a pixel (see [`crate::grey`]).
    Grey(Vec<u8>),
}

impl Pixels {
    fn tile(rgba: Vec<Color32>) -> Self {
        grey::to_grey(&rgba).map_or(Pixels::Rgba(rgba), Pixels::Grey)
    }
}

impl Output {
    /// The queued key this output answers, if any.
    fn key(&self) -> Option<Key> {
        match *self {
            Output::Image { key, .. } | Output::Failed(key) => Some(key),
            Output::Links { doc, page, .. } => Some(Key::Links { doc, page }),
            _ => None,
        }
    }
}

enum Control {
    Open(DocId),
    Snapshot(SnapshotJob),
    Outline(DocId),
}

impl Control {
    fn doc(&self) -> DocId {
        match self {
            Control::Open(doc) | Control::Outline(doc) => *doc,
            Control::Snapshot(job) => job.doc,
        }
    }
}

enum Work {
    Control(Control),
    Render(Vec<Key>),
}

impl Work {
    fn doc(&self) -> DocId {
        match self {
            Work::Control(c) => c.doc(),
            Work::Render(batch) => batch[0].doc(),
        }
    }

    /// The pages this job parses, or `None` if it may parse any number of them.
    fn pages(&self) -> Option<Vec<u32>> {
        match self {
            // Reading every page size, or resolving outline destinations.
            Work::Control(Control::Open(_) | Control::Outline(_)) => None,
            Work::Control(Control::Snapshot(job)) => Some(job.pages.iter().map(|p| p.page).collect()),
            Work::Render(batch) => Some(vec![batch[0].page()]),
        }
    }
}

#[derive(Default)]
struct Queue {
    control: VecDeque<Control>,
    /// What to render next: what the UI wants, less what is in flight or done.
    wanted: Vec<Key>,
    /// Everything the UI wanted in its latest frame, in flight or not. A batch skips keys
    /// that have left it, such as tiles of a tab switched away from mid-page.
    current: HashSet<Key>,
    /// Keys being worked on by any thread.
    busy: Vec<Key>,
    done: Vec<Output>,
    paths: HashMap<DocId, PathBuf>,
    /// Bumped to have every thread close its open documents, releasing CoreGraphics' caches
    /// for them.
    release: u64,
    /// How many documents may be open at once: one for each window showing one.
    open_limit: usize,
}

impl Queue {
    /// Queues `job`, dropping a snapshot of the same tab still waiting: only the latest is of
    /// use. Snapshots go before tiles, so flipping quickly between tabs would otherwise pile
    /// up one per switch, each a full viewport render, and hold back the tiles of the tab
    /// shown (a second of them after 40 flips between two dense documents).
    fn queue_snapshot(&mut self, job: SnapshotJob) {
        self.control.retain(|c| !matches!(c, Control::Snapshot(queued) if queued.doc == job.doc));
        self.control.push_back(Control::Snapshot(job));
    }
}

pub struct Renderer {
    shared: Arc<(Mutex<Queue>, Condvar)>,
}

impl Renderer {
    /// `threads` and `pages_per_open`: see [`crate::memory::Tuning`].
    pub fn spawn(ctx: egui::Context, threads: usize, pages_per_open: usize) -> Self {
        let shared = Arc::new((Mutex::new(Queue::default()), Condvar::new()));
        for _ in 0..threads.max(1) {
            let (shared, ctx) = (shared.clone(), ctx.clone());
            std::thread::Builder::new()
                .name("pdf-render".into())
                // TODO(perf): on AC power (M1 Pro) QoS makes no difference, even background
                // (`render_spike`); QOS_CLASS_USER_INITIATED may still matter on battery.
                .spawn(move || worker(&shared, &ctx, pages_per_open))
                .expect("spawn render thread");
        }
        Self { shared }
    }

    fn with_queue(&self, f: impl FnOnce(&mut Queue)) {
        let (lock, cvar) = &*self.shared;
        f(&mut lock.lock().unwrap());
        cvar.notify_all();
    }

    pub fn open(&self, doc: DocId, path: PathBuf) {
        self.with_queue(|q| {
            q.paths.insert(doc, path);
            q.control.push_back(Control::Open(doc));
        });
    }

    /// Drops the document; the worker closes it if it is open.
    pub fn forget(&self, doc: DocId) {
        self.with_queue(|q| {
            q.paths.remove(&doc);
            q.wanted.retain(|k| k.doc() != doc);
            q.current.retain(|k| k.doc() != doc);
        });
    }

    /// Lets `limit` documents stay open at once, one for each window.
    pub fn set_open_limit(&self, limit: usize) {
        self.shared.0.lock().unwrap().open_limit = limit;
    }

    /// Drops queued work and closes the open documents; the next job reopens what it needs.
    pub fn release(&self) {
        self.with_queue(|q| {
            q.wanted.clear();
            q.current.clear();
            q.release += 1;
        });
    }

    /// Renders a snapshot of a tab's viewport, replacing one of the same tab still queued.
    pub fn snapshot(&self, job: SnapshotJob) {
        self.with_queue(|q| q.queue_snapshot(job));
    }

    /// Reads the document outline. Resolving entries to pages parses those pages, so this is
    /// only requested when the outline is shown.
    pub fn outline(&self, doc: DocId) {
        self.with_queue(|q| q.control.push_back(Control::Outline(doc)));
    }

    pub fn take_done(&self) -> Vec<Output> {
        std::mem::take(&mut self.shared.0.lock().unwrap().done)
    }

    /// Replaces the render queue. Keys already in flight, or finished but not yet taken,
    /// are skipped so nothing is rendered twice.
    pub fn set_wanted(&self, mut wanted: Vec<Key>) {
        self.with_queue(|q| {
            q.current.clear();
            q.current.extend(wanted.iter().copied());
            wanted.retain(|k| !q.busy.contains(k) && !q.done.iter().any(|o| o.key() == Some(*k)));
            q.wanted = wanted;
        });
    }
}

/// A document the worker has open, and the pages it has parsed since opening.
struct OpenDoc {
    id: DocId,
    doc: Document,
    pages: HashSet<u32>,
}

fn worker(shared: &(Mutex<Queue>, Condvar), ctx: &egui::Context, pages_per_open: usize) {
    // Least recently used first.
    let mut open: Vec<OpenDoc> = Vec::new();
    let mut released = 0;

    loop {
        let (lock, cvar) = shared;
        let (work, path, limit) = {
            let mut q = lock.lock().unwrap();
            loop {
                if q.release != released {
                    released = q.release;
                    open.clear();
                }
                open.retain(|o| q.paths.contains_key(&o.id));
                if let Some(c) = q.control.pop_front() {
                    match q.paths.get(&c.doc()) {
                        Some(path) => break (Work::Control(c), path.clone(), q.open_limit.max(1)),
                        None => continue,
                    }
                }
                if !q.wanted.is_empty() {
                    // Batch every queued job for the same page so it is only looked up once.
                    let first = q.wanted.remove(0);
                    let mut batch = vec![first];
                    q.wanted.retain(|k| {
                        let same = k.doc() == first.doc() && k.page() == first.page();
                        if same {
                            batch.push(*k);
                        }
                        !same
                    });
                    if let Some(path) = q.paths.get(&first.doc()).cloned() {
                        q.busy.extend(&batch);
                        break (Work::Render(batch), path, q.open_limit.max(1));
                    }
                }
                q = cvar.wait(q).unwrap();
            }
        };

        let out = Delivery { shared, ctx };
        let mine = match &work {
            Work::Render(batch) => batch.clone(),
            Work::Control(_) => vec![],
        };
        let doc_id = work.doc();
        let pages = work.pages();
        // CoreGraphics may autorelease internally; drain per job so nothing piles up on
        // this long-lived thread.
        engine::autoreleasepool(|| {
            let current = match open.iter().position(|o| o.id == doc_id) {
                Some(i) => Some(open.remove(i)),
                None => {
                    // Close documents over the limit before opening another.
                    let excess = (open.len() + 1).saturating_sub(limit);
                    open.drain(..excess);
                    Document::open(&path).map(|doc| OpenDoc { id: doc_id, doc, pages: HashSet::new() })
                }
            };
            match (&current, work) {
                (Some(o), work) => run(&o.doc, doc_id, work, &out),
                (None, Work::Control(Control::Open(doc))) => {
                    out.send(Output::Opened { doc, result: Err(format!("Could not open {}", path.display())) })
                }
                (None, Work::Control(Control::Snapshot(job))) => {
                    out.send(Output::Snapshot { doc: job.doc, snapshot: None })
                }
                (None, Work::Control(Control::Outline(doc))) => out.send(Output::Outline { doc, entries: vec![] }),
                (None, Work::Render(batch)) => batch.into_iter().for_each(|key| out.send(Output::Failed(key))),
            }
            if let Some(mut o) = current {
                o.pages.extend(pages.iter().flatten());
                if pages.is_some() && o.pages.len() <= pages_per_open {
                    open.push(o);
                }
            }
        });
        // Skipped keys never produce an output.
        lock.lock().unwrap().busy.retain(|k| !mine.contains(k));
    }
}

/// Hands the render thread's outputs to the UI as each is made, so a page's visible tiles
/// don't wait for the rest of its batch.
struct Delivery<'a> {
    shared: &'a (Mutex<Queue>, Condvar),
    ctx: &'a egui::Context,
}

impl Delivery<'_> {
    fn send(&self, output: Output) {
        let mut q = self.shared.0.lock().unwrap();
        if let Some(key) = output.key() {
            q.busy.retain(|k| *k != key);
        }
        q.done.push(output);
        drop(q);
        self.ctx.request_repaint();
    }

    /// Whether the UI still wants `key`; a batch skips it otherwise.
    fn wanted(&self, key: &Key) -> bool {
        self.shared.0.lock().unwrap().current.contains(key)
    }
}

fn run(doc: &Document, doc_id: DocId, work: Work, out: &Delivery) {
    match work {
        Work::Control(Control::Open(_)) => {
            let sizes = (0..doc.page_count()).map(|i| doc.page(i).map_or(vec2(612.0, 792.0), |p| p.size())).collect();
            out.send(Output::Opened { doc: doc_id, result: Ok(sizes) });
        }
        Work::Control(Control::Snapshot(job)) => {
            out.send(Output::Snapshot { doc: job.doc, snapshot: render_snapshot(doc, &job) });
        }
        Work::Control(Control::Outline(_)) => out.send(Output::Outline { doc: doc_id, entries: doc.outline() }),
        Work::Render(batch) => {
            let Some(page) = doc.page(batch[0].page()) else {
                return batch.into_iter().for_each(|key| out.send(Output::Failed(key)));
            };
            let mut keys = batch.into_iter().filter(|key| out.wanted(key)).peekable();
            while let Some(key) = keys.next() {
                match key {
                    Key::Links { doc: d, page: p } => out.send(Output::Links { doc: d, page: p, links: page.links() }),
                    Key::Thumb { width, .. } => {
                        out.send(render_thumb(&page, key, width).unwrap_or(Output::Failed(key)))
                    }
                    Key::Tile { .. } => {
                        // The view lists tiles row by row, left to right.
                        let mut run = vec![key];
                        while let Some(next) = keys.next_if(|k| next_in_row(run[run.len() - 1], *k)) {
                            run.push(next);
                        }
                        match render_tiles(&page, &run) {
                            Some(tiles) => tiles.into_iter().for_each(|t| out.send(t)),
                            None => run.into_iter().for_each(|key| out.send(Output::Failed(key))),
                        }
                    }
                }
            }
        }
    }
}

/// Whether `next` is the tile right of `prev`, at the same scale.
fn next_in_row(prev: Key, next: Key) -> bool {
    matches!(
        (prev, next),
        (Key::Tile { scale: s0, tx: x0, ty: y0, .. }, Key::Tile { scale: s1, tx: x1, ty: y1, .. })
            if s0 == s1 && y0 == y1 && x1 == x0 + 1
    )
}

/// Renders a run of adjacent tiles in a row (see [`next_in_row`]) with one draw, sliced into
/// tiles. CoreGraphics walks the whole page's content however small the clip, so a draw per
/// tile cost 1.6-2.8x as much per page (`cargo bench --bench render_spike`).
fn render_tiles(page: &Page, run: &[Key]) -> Option<Vec<Output>> {
    let (Key::Tile { scale, tx, ty, .. }, Some(&Key::Tile { tx: last, .. })) = (run[0], run.last()) else {
        return None;
    };
    let page_px = page_pixels(page.size(), scale);
    let origin = [tx * TILE, ty * TILE];
    let w = ((last + 1) * TILE).min(page_px[0]).checked_sub(origin[0])?;
    let h = TILE.min(page_px[1].checked_sub(origin[1])?);
    let mut strip: Vec<Color32> = bytemuck::allocation::zeroed_vec((w * h) as usize);
    Bitmap::new(bytemuck::cast_slice_mut(&mut strip), w, h)?.draw(
        page,
        [page_px[0] as f64, page_px[1] as f64],
        [-(origin[0] as f64), -(origin[1] as f64)],
    );
    let [w, h] = [w as usize, h as usize];
    if let [key] = *run {
        return Some(vec![Output::Image { key, size: [w, h], pixels: Pixels::tile(strip) }]);
    }
    let tile = TILE as usize;
    let tiles = run.iter().enumerate().map(|(i, &key)| {
        let x = i * tile;
        let tw = tile.min(w - x);
        let pixels = strip.chunks_exact(w).flat_map(|row| &row[x..x + tw]).copied().collect();
        Output::Image { key, size: [tw, h], pixels: Pixels::tile(pixels) }
    });
    Some(tiles.collect())
}

fn render_thumb(page: &Page, key: Key, width: u32) -> Option<Output> {
    let h = thumb_height(page.size(), width);
    let mut pixels: Vec<Color32> = bytemuck::allocation::zeroed_vec((width * h) as usize);
    Bitmap::new(bytemuck::cast_slice_mut(&mut pixels), width, h)?.draw(page, [width as f64, h as f64], [0.0, 0.0]);
    Some(Output::Image { key, size: [width as usize, h as usize], pixels: Pixels::Rgba(pixels) })
}

// TODO(perf): the QOI encode runs on the render thread, delaying tiles for the tab just
// switched to by its duration: ~1.9 ms for a 1512x945 snapshot (measured on x86 Linux;
// likely less on M-series). Low priority; if it shows in the "cycle all tabs" numbers,
// encode on a short-lived thread instead.
fn render_snapshot(doc: &Document, job: &SnapshotJob) -> Option<Snapshot> {
    let [w, h] = job.size;
    let mut rgba = vec![0u8; (w * h * 4) as usize];
    {
        let bitmap = Bitmap::new(&mut rgba, w, h)?;
        for p in &job.pages {
            let Some(page) = doc.page(p.page) else { continue };
            bitmap.draw(&page, [p.width as f64, p.height as f64], [p.x as f64, p.y as f64]);
        }
    }
    let mut qoi = qoi::encode_to_vec(&rgba, w, h).ok()?;
    // The encoder allocates for the worst case (5 bytes a pixel, more than uncompressed);
    // snapshots are kept for as long as their tab is in the background.
    qoi.shrink_to_fit();
    Some(Snapshot { size: job.size, qoi })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgba(pixels: Pixels) -> Vec<Color32> {
        match pixels {
            Pixels::Rgba(p) => p,
            Pixels::Grey(p) => p.into_iter().map(Color32::from_gray).collect(),
        }
    }

    fn write_png(path: &str, rgba: &[u8], w: u32, h: u32) {
        let mut enc = png::Encoder::new(std::fs::File::create(path).unwrap(), w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.write_header().unwrap().write_image_data(rgba).unwrap();
    }

    #[test]
    fn a_tab_has_at_most_one_snapshot_queued() {
        let job = |doc| SnapshotJob { doc, size: [1, 1], pages: vec![] };
        let mut q = Queue::default();
        q.control.push_back(Control::Open(1));
        // Flipping between two tabs: each switch snapshots the one left.
        for _ in 0..20 {
            q.queue_snapshot(job(1));
            q.queue_snapshot(job(2));
        }
        q.control.push_back(Control::Outline(1));
        let queued: Vec<_> = q
            .control
            .iter()
            .map(|c| match c {
                Control::Open(_) => "open",
                Control::Snapshot(job) if job.doc == 1 => "snapshot 1",
                Control::Snapshot(_) => "snapshot 2",
                Control::Outline(_) => "outline",
            })
            .collect();
        assert_eq!(queued, ["open", "snapshot 1", "snapshot 2", "outline"]);
    }

    /// Headless render check: `RENDER_TEST_PDF=<pdf> RENDER_TEST_OUT=<dir> cargo test`.
    #[test]
    fn renders_tiles_thumbs_and_snapshots() {
        let (Ok(pdf), Ok(out)) = (std::env::var("RENDER_TEST_PDF"), std::env::var("RENDER_TEST_OUT")) else {
            return;
        };
        let document = Document::open(std::path::Path::new(&pdf)).expect("open");
        let entries = document.outline();
        println!("outline: {} entries", entries.len());
        for e in entries.iter().take(4) {
            println!("  {}{} -> {:?}", "  ".repeat(e.depth), e.title, e.target);
        }
        let with_links = (0..document.page_count().min(60)).find_map(|p| {
            let links = document.page(p)?.links();
            (!links.is_empty()).then_some((p, links))
        });
        if let Some((p, links)) = with_links {
            println!("page {} has {} links, e.g. {:?}", p + 1, links.len(), links[0]);
            assert!(
                links.iter().all(|l| l.rect.min.x >= -0.01 && l.rect.max.y <= 1.01),
                "link rects should be page fractions"
            );
        }
        let page = document.page(1).expect("page");
        println!("page size {:?}", page.size());

        let tile = |scale, tx, ty| Key::Tile { doc: 0, page: 1, scale: scale_key(scale), tx, ty };
        let row = [tile(2.0, 0, 1), tile(2.0, 1, 1), tile(2.0, 2, 1)];
        let mut images = render_tiles(&page, &[tile(0.8, 0, 0)]).expect("render tile");
        images.extend(render_tiles(&page, &row).expect("render row"));
        images.push(render_thumb(&page, Key::Thumb { doc: 0, page: 1, width: 108 }, 108).expect("render thumb"));
        for image in images {
            let Output::Image { key, size, pixels } = image else { panic!("not an image") };
            let pixels = rgba(pixels);
            if row.contains(&key)
                && let Some(Output::Image { pixels: alone, .. }) =
                    render_tiles(&page, &[key]).and_then(|t| t.into_iter().next())
            {
                let alone = rgba(alone);
                // Image resampling rounds differently by a level, and a lone tile's clip can
                // shade a few edge pixels, so allow that much.
                let off = pixels.iter().zip(&alone).filter(|(a, b)| (0..4).any(|i| a[i].abs_diff(b[i]) > 1)).count();
                assert!(
                    off * 10_000 <= pixels.len(),
                    "{key:?}: {off} pixels differ between its row and it drawn alone"
                );
            }
            let rgba: &[u8] = bytemuck::cast_slice(&pixels);
            let name = match key {
                Key::Tile { scale, tx, ty, .. } => format!("tile_{scale}_{tx}_{ty}"),
                Key::Thumb { .. } | Key::Links { .. } => "thumb".into(),
            };
            write_png(&format!("{out}/{name}.png"), rgba, size[0] as u32, size[1] as u32);
        }

        let job = SnapshotJob {
            doc: 0,
            size: [700, 500],
            pages: vec![
                SnapshotPage { page: 0, x: 20, y: -200, width: 576, height: 432 },
                SnapshotPage { page: 1, x: 20, y: 244, width: 576, height: 432 },
            ],
        };
        let Some(Snapshot { size, qoi }) = render_snapshot(&document, &job) else { panic!("snapshot") };
        let (_, rgba) = qoi::decode_to_vec(&qoi).unwrap();
        println!("snapshot qoi {} KB (raw {} KB)", qoi.len() / 1024, rgba.len() / 1024);
        write_png(&format!("{out}/snapshot.png"), &rgba, size[0], size[1]);
    }
}
