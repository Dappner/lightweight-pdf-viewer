//! Background render thread: rasterizes pages and reads document structure (outline, links)
//! with the [`crate::engine`], and decides what to render next.
//!
//! Every document lives on this one thread. At most one document per window is kept open:
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

pub type DocId = u64;

/// Tile edge length in physical pixels.
// TODO(perf): try 256 and 1024. Smaller tiles land sooner and waste less off-screen area;
// larger ones mean fewer CoreGraphics draws and textures. Compare "to sharp" and cpu in
// `cargo bench --bench session` on an M-series Mac.
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
    /// Premultiplied pixels, in the layout egui uploads, so taking one in on the UI thread
    /// doesn't copy it.
    Image {
        key: Key,
        size: [usize; 2],
        pixels: Vec<Color32>,
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
    wanted: Vec<Key>,
    busy: Vec<Key>,
    done: Vec<Output>,
    paths: HashMap<DocId, PathBuf>,
    /// Close the open documents, releasing CoreGraphics' caches for them.
    release: bool,
    /// How many documents may be open at once: one for each window showing one.
    open_limit: usize,
}

pub struct Renderer {
    shared: Arc<(Mutex<Queue>, Condvar)>,
}

impl Renderer {
    /// `pages_per_open`: see [`crate::memory::Tuning::pages_per_open`].
    pub fn spawn(ctx: egui::Context, pages_per_open: usize) -> Self {
        let shared = Arc::new((Mutex::new(Queue::default()), Condvar::new()));
        let worker_shared = shared.clone();
        std::thread::Builder::new()
            .name("pdf-render".into())
            // TODO(perf): give this thread QOS_CLASS_USER_INITIATED
            // (pthread_set_qos_class_self_np) so macOS keeps it on performance cores; it
            // renders what is on screen. Measure "to sharp" while scrolling on battery.
            // TODO(perf): a second worker with its own Document (prefetch and thumbnails
            // while the first draws visible tiles) would use a second M-series core, at the
            // cost of a second copy of CoreGraphics' per-document caches.
            .spawn(move || worker(&worker_shared, &ctx, pages_per_open))
            .expect("spawn render thread");
        Self { shared }
    }

    fn with_queue(&self, f: impl FnOnce(&mut Queue)) {
        let (lock, cvar) = &*self.shared;
        f(&mut lock.lock().unwrap());
        cvar.notify_one();
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
            q.release = true;
        });
    }

    pub fn snapshot(&self, job: SnapshotJob) {
        self.with_queue(|q| q.control.push_back(Control::Snapshot(job)));
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

    loop {
        let (lock, cvar) = shared;
        let (work, path, limit) = {
            let mut q = lock.lock().unwrap();
            loop {
                if std::mem::take(&mut q.release) {
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
                        q.busy = batch.clone();
                        break (Work::Render(batch), path, q.open_limit.max(1));
                    }
                }
                q = cvar.wait(q).unwrap();
            }
        };

        let doc_id = work.doc();
        let pages = work.pages();
        // CoreGraphics may autorelease internally; drain per job so nothing piles up on
        // this long-lived thread.
        let outputs = engine::autoreleasepool(|| {
            let current = match open.iter().position(|o| o.id == doc_id) {
                Some(i) => Some(open.remove(i)),
                None => {
                    // Close documents over the limit before opening another.
                    let excess = (open.len() + 1).saturating_sub(limit);
                    open.drain(..excess);
                    Document::open(&path).map(|doc| OpenDoc { id: doc_id, doc, pages: HashSet::new() })
                }
            };
            let outputs = match (&current, work) {
                (Some(o), work) => run(&o.doc, doc_id, work),
                (None, Work::Control(Control::Open(doc))) => {
                    vec![Output::Opened { doc, result: Err(format!("Could not open {}", path.display())) }]
                }
                (None, Work::Control(Control::Snapshot(job))) => {
                    vec![Output::Snapshot { doc: job.doc, snapshot: None }]
                }
                (None, Work::Control(Control::Outline(doc))) => vec![Output::Outline { doc, entries: vec![] }],
                (None, Work::Render(batch)) => batch.into_iter().map(Output::Failed).collect(),
            };
            if let Some(mut o) = current {
                o.pages.extend(pages.iter().flatten());
                if pages.is_some() && o.pages.len() <= pages_per_open {
                    open.push(o);
                }
            }
            outputs
        });

        let mut q = lock.lock().unwrap();
        q.done.extend(outputs);
        q.busy.clear();
        drop(q);
        ctx.request_repaint();
    }
}

fn run(doc: &Document, doc_id: DocId, work: Work) -> Vec<Output> {
    match work {
        Work::Control(Control::Open(_)) => {
            let sizes = (0..doc.page_count()).map(|i| doc.page(i).map_or(vec2(612.0, 792.0), |p| p.size())).collect();
            vec![Output::Opened { doc: doc_id, result: Ok(sizes) }]
        }
        Work::Control(Control::Snapshot(job)) => {
            vec![Output::Snapshot { doc: job.doc, snapshot: render_snapshot(doc, &job) }]
        }
        Work::Control(Control::Outline(_)) => vec![Output::Outline { doc: doc_id, entries: doc.outline() }],
        Work::Render(batch) => {
            let Some(page) = doc.page(batch[0].page()) else {
                return batch.into_iter().map(Output::Failed).collect();
            };
            batch
                .into_iter()
                .map(|key| match key {
                    Key::Links { doc: d, page: p } => Output::Links { doc: d, page: p, links: page.links() },
                    _ => render_key(&page, key).unwrap_or(Output::Failed(key)),
                })
                .collect()
        }
    }
}

// TODO(perf): each tile draws the whole page clipped to the tile, so a page of N tiles has
// its content stream walked N times. Drawing a row of the batch's tiles into one bitmap
// and slicing it (a few MB transient) may be cheaper; how much depends on how well
// CoreGraphics culls by clip, so measure on macOS before adopting (the synthetic engine's
// cost is per pixel and cannot show it).
fn render_key(page: &Page, key: Key) -> Option<Output> {
    let size = page.size();
    let (page_px, origin, w, h) = match key {
        Key::Tile { scale, tx, ty, .. } => {
            let page_px = page_pixels(size, scale);
            let origin = [tx * TILE, ty * TILE];
            let w = TILE.min(page_px[0].checked_sub(origin[0])?);
            let h = TILE.min(page_px[1].checked_sub(origin[1])?);
            (page_px, origin, w, h)
        }
        Key::Thumb { width, .. } => {
            let h = thumb_height(size, width);
            ([width, h], [0, 0], width, h)
        }
        Key::Links { .. } => return None,
    };
    let mut pixels: Vec<Color32> = bytemuck::allocation::zeroed_vec((w * h) as usize);
    Bitmap::new(bytemuck::cast_slice_mut(&mut pixels), w, h)?.draw(
        page,
        [page_px[0] as f64, page_px[1] as f64],
        [-(origin[0] as f64), -(origin[1] as f64)],
    );
    Some(Output::Image { key, size: [w as usize, h as usize], pixels })
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

    fn write_png(path: &str, rgba: &[u8], w: u32, h: u32) {
        let mut enc = png::Encoder::new(std::fs::File::create(path).unwrap(), w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.write_header().unwrap().write_image_data(rgba).unwrap();
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

        for key in [
            Key::Tile { doc: 0, page: 1, scale: scale_key(0.8), tx: 0, ty: 0 },
            Key::Tile { doc: 0, page: 1, scale: scale_key(2.0), tx: 1, ty: 1 },
            Key::Thumb { doc: 0, page: 1, width: 108 },
        ] {
            let Some(Output::Image { key, size, pixels }) = render_key(&page, key) else { panic!("render {key:?}") };
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
