//! Text boxes typed onto pages and highlighted text: a PDF's FreeText and Highlight
//! annotations, which Preview makes with Tools → Annotate → Text and Highlight.
//!
//! A page shows the boxes the file has (read with its links) until it is first edited.
//! Highlights are kept the same way, alongside them. From
//! then on the tab owns the page's boxes: every change is written to a file of the tab's own
//! (see [`dir`]), so closing the tab or quitting loses nothing, and ⌘S writes them into the
//! PDF. Revert to Saved drops them instead. The edits follow the PDF when it's renamed or
//! moved on the same filesystem. The app draws every box itself, those from the file
//! included; the page renderer excludes FreeText and Highlight annotations, so edited
//! annotations never show twice.

use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;

use eframe::egui::ahash::HashMap;
use eframe::egui::epaint::text::{FontInsert, InsertFontFamily};
use eframe::egui::{Color32, Context, FontData, FontFamily, FontId, Galley, Pos2, Rect, Vec2, pos2, vec2};
use serde::{Deserialize, Serialize};

use crate::layout::Rotation;

/// Font sizes offered, in PDF points.
pub const SIZES: [f32; 5] = [9.0, 12.0, 16.0, 24.0, 36.0];
pub const DEFAULT_SIZE: f32 = 12.0;
pub const COLORS: [(&str, [u8; 3]); 3] = [("Black", [0, 0, 0]), ("Red", [214, 34, 34]), ("Blue", [24, 88, 214])];
/// Space between a box's edges and its text, in PDF points, as PDFKit draws FreeText.
pub const PAD: f32 = 2.0;
/// The colour new highlights are: Preview's yellow.
pub const HIGHLIGHT_YELLOW: [u8; 3] = [255, 222, 41];
/// The font text boxes are drawn and saved in: Preview's default for them.
pub const FONT: &str = "Helvetica";
const FONT_FILE: &str = "/System/Library/Fonts/Helvetica.ttc";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TextBox {
    pub text: String,
    /// Top-left corner, as fractions of the page upright: turned as the file has it, before
    /// any rotation the reader adds.
    pub at: [f32; 2],
    /// Font size in PDF points.
    pub size: f32,
    pub color: [u8; 3],
    /// Width in PDF points, edge to edge, that the text wraps at; `None` grows to fit the text.
    pub width: Option<f32>,
    /// Which of the page's text boxes in the file this is, in the file's order, for one read
    /// from it.
    pub origin: Option<u32>,
    /// Changed since it was read from the file. Saving leaves unchanged ones as they are, in
    /// whatever font and style the file gave them.
    #[serde(default)]
    pub changed: bool,
}

impl TextBox {
    pub fn new(at: Pos2, text: String) -> Self {
        Self {
            text,
            at: [at.x, at.y],
            size: DEFAULT_SIZE,
            color: COLORS[0].1,
            width: None,
            origin: None,
            changed: true,
        }
    }

    pub fn at(&self) -> Pos2 {
        pos2(self.at[0], self.at[1])
    }

    pub fn color32(&self) -> Color32 {
        let [r, g, b] = self.color;
        Color32::from_rgb(r, g, b)
    }

    pub fn font(&self, ctx: &Context, scale: f32) -> FontId {
        FontId::new(self.size * scale, family(ctx))
    }

    /// `text` (the box's, or a draft of it) laid out at `scale` screen points per PDF point.
    pub fn galley(&self, ctx: &Context, text: &str, scale: f32) -> Arc<Galley> {
        let wrap = self.width.map_or(f32::INFINITY, |w| (w - 2.0 * PAD).max(1.0) * scale);
        // Before taking the fonts: finding the family reads them too, which would deadlock.
        let font = self.font(ctx, scale);
        ctx.fonts_mut(|f| f.layout(text.to_owned(), font, self.color32(), wrap))
    }

    /// The box's size in PDF points around a galley laid out at `scale`.
    pub fn size_around(&self, galley: &Galley, scale: f32) -> Vec2 {
        let text = galley.size() / scale;
        // An empty box is still a line high and a little wide, so it can be seen and clicked.
        // A point to spare, so PDFKit doesn't wrap a last letter that rounds wider there.
        let width = self.width.unwrap_or(text.x.max(self.size) + 2.0 * PAD + 1.0);
        vec2(width, text.y.max(self.size) + 2.0 * PAD)
    }

    /// The box with `edge` dragged `by` PDF points rightwards, on a page `page_width` points
    /// wide upright, from `width` points wide (see [`Self::size_around`]): wrapping at its new
    /// width, at least a letter wide (or as wide as it was, if narrower), its left edge kept
    /// on the page.
    pub fn resized(&self, edge: Edge, by: f32, width: f32, page_width: f32) -> Self {
        let min = (self.size + 2.0 * PAD).min(width);
        let mut b = self.clone();
        match edge {
            Edge::Right => b.width = Some((width + by).max(min)),
            Edge::Left => {
                let by = by.min(width - min).max(-self.at[0] * page_width);
                b.width = Some(width - by);
                b.at[0] += by / page_width;
            }
        }
        b
    }
}

/// A side of a box, dragged to change how wide it wraps.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Edge {
    Left,
    Right,
}

/// The family boxes are drawn in: Helvetica once loaded (the first time a box is shown, as
/// it costs a couple of MB), the default font until then or if it can't be read.
pub fn family(ctx: &Context) -> FontFamily {
    static LOADING: AtomicBool = AtomicBool::new(false);
    let helvetica = FontFamily::Name(FONT.into());
    if ctx.fonts(|f| f.families().contains(&helvetica)) {
        return helvetica;
    }
    if !LOADING.swap(true, Ordering::Relaxed)
        && let Ok(bytes) = std::fs::read(FONT_FILE)
    {
        // Kept for good, so egui borrows the bytes: given them owned, it copies them each time
        // it rebuilds its fonts (for each window and display scale).
        let bytes: &'static [u8] = Box::leak(bytes.into_boxed_slice());
        // Takes effect on the next frame.
        ctx.add_font(FontInsert::new(
            FONT,
            FontData::from_static(bytes),
            vec![InsertFontFamily { family: helvetica, priority: eframe::egui::epaint::text::FontPriority::Highest }],
        ));
        ctx.request_repaint();
    }
    FontFamily::Proportional
}

/// Screen points per PDF point to lay box text out at: `scale`, or while the zoom is changing,
/// `scale` rounded to a quarter octave, the text then stretched to `scale` as it's drawn. egui keeps glyph images for every pixel size it draws,
/// so text following a pinch frame by frame would fill its glyph texture (17 MB, and as much
/// again on the GPU, for a pinch from 50% to 400%) and make it redraw every glyph in the app.
pub fn glyph_scale(scale: f32, zooming: bool) -> f32 {
    if zooming { ((scale.log2() * 4.0).round() / 4.0).exp2() } else { scale }
}

/// Highlighted text: a PDF's Highlight annotation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Highlight {
    /// A rect per line, `[left, top, right, bottom]` as fractions of the page upright (see
    /// [`TextBox::at`]).
    pub rects: Vec<[f32; 4]>,
    pub color: [u8; 3],
    /// Which of the page's highlights in the file this is, in the file's order, for one read
    /// from it.
    pub origin: Option<u32>,
}

impl Highlight {
    pub fn new(rects: impl IntoIterator<Item = Rect>) -> Self {
        let rects = rects.into_iter().map(|r| [r.min.x, r.min.y, r.max.x, r.max.y]).collect();
        Self { rects, color: HIGHLIGHT_YELLOW, origin: None }
    }

    pub fn rects(&self) -> impl Iterator<Item = Rect> + '_ {
        self.rects.iter().map(|&[x0, y0, x1, y1]| Rect::from_min_max(pos2(x0, y0), pos2(x1, y1)))
    }
}

/// How near, as a fraction of the page, highlights on a line must come to touch.
const TOUCH: f32 = 0.002;

/// Highlights `rects` (one a line) in `color` among a page's `highlights`, which never
/// overlap: the ones of that colour it overlaps or touches merge with it, and it trims the
/// others where it covers them. Text already highlighted in that colour is unhighlighted
/// instead. A highlight from the file that changes becomes a new one.
pub fn add_highlight(highlights: &mut Vec<Highlight>, rects: Vec<Rect>, color: [u8; 3]) {
    let same: Vec<Rect> = highlights.iter().filter(|h| h.color == color).flat_map(Highlight::rects).collect();
    let erase = rects.iter().all(|r| covered(*r, &same));
    let mut merged = rects.clone();
    let mut kept = Vec::new();
    for h in highlights.drain(..) {
        let old: Vec<Rect> = h.rects().collect();
        if h.color == color && !erase && old.iter().any(|o| rects.iter().any(|n| touch(*o, *n))) {
            merged.extend(old);
            continue;
        }
        let left: Vec<Rect> = old
            .iter()
            .flat_map(|&o| {
                rects.iter().fold(vec![o], |pieces, &n| pieces.into_iter().flat_map(|p| cut(p, n)).collect())
            })
            .collect();
        if left == old {
            kept.push(h);
        } else if !left.is_empty() {
            kept.push(Highlight { color: h.color, ..Highlight::new(left) });
        }
    }
    if !erase {
        kept.push(Highlight { color, ..Highlight::new(merge_lines(merged)) });
    }
    *highlights = kept;
}

/// Whether `a` and `b` are on one line: their heights overlap by over half the smaller.
fn same_line(a: Rect, b: Rect) -> bool {
    a.max.y.min(b.max.y) - a.min.y.max(b.min.y) > 0.5 * a.height().min(b.height())
}

/// Whether `a` and `b` are on one line and overlap or touch on it.
fn touch(a: Rect, b: Rect) -> bool {
    same_line(a, b) && a.min.x <= b.max.x + TOUCH && b.min.x <= a.max.x + TOUCH
}

/// `r` without what `cover` covers of it on its line: none, one or two pieces.
fn cut(r: Rect, cover: Rect) -> Vec<Rect> {
    if !same_line(r, cover) || cover.max.x <= r.min.x || cover.min.x >= r.max.x {
        return vec![r];
    }
    let left = Rect::from_x_y_ranges(r.min.x..=cover.min.x, r.y_range());
    let right = Rect::from_x_y_ranges(cover.max.x..=r.max.x, r.y_range());
    [left, right].into_iter().filter(|p| p.width() > TOUCH).collect()
}

/// Whether the rects of `cover` on `r`'s line cover it.
fn covered(r: Rect, cover: &[Rect]) -> bool {
    let mut spans: Vec<(f32, f32)> = cover.iter().filter(|c| same_line(r, **c)).map(|c| (c.min.x, c.max.x)).collect();
    spans.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut x = r.min.x;
    for (from, to) in spans {
        if from > x + TOUCH {
            break;
        }
        x = x.max(to);
    }
    x >= r.max.x - TOUCH
}

/// `rects` with those on one line that overlap or touch made one.
fn merge_lines(mut rects: Vec<Rect>) -> Vec<Rect> {
    rects.sort_by(|a, b| a.min.x.total_cmp(&b.min.x));
    let mut lines: Vec<Rect> = Vec::new();
    for r in rects {
        match lines.iter_mut().find(|l| touch(**l, r)) {
            Some(line) => *line = line.union(r),
            None => lines.push(r),
        }
    }
    lines.sort_by(|a, b| a.min.y.total_cmp(&b.min.y));
    lines
}

/// The box at `index` on `page`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BoxRef {
    pub page: u32,
    pub index: usize,
}

/// A box whose text is being typed: an existing one, or (`index` `None`) one not yet added,
/// which is added once it has text.
pub struct Editing {
    pub page: u32,
    pub index: Option<usize>,
    /// For a new box, its top-left corner; see [`TextBox::at`].
    pub at: Pos2,
    pub draft: String,
    /// Size and colour, which the field's menu changes along with the text, and the width and
    /// left edge, which dragging its sides changes.
    pub size: f32,
    pub color: [u8; 3],
    pub width: Option<f32>,
    /// Ask for the keyboard on the next frame.
    pub focus: bool,
}

/// A page's boxes or highlights as they were before a change, to undo it. `None` if the
/// page's were not yet edited.
enum Undo {
    Boxes(u32, Option<Vec<TextBox>>),
    Highlights(u32, Option<Vec<Highlight>>),
    /// Every edit, as it was before [`TextBoxes::revert`].
    Revert(Box<Edits>),
}

/// The edits not yet saved, all of them.
#[derive(Default)]
struct Edits {
    pages: BTreeMap<u32, Vec<TextBox>>,
    added: BTreeMap<u32, Vec<TextBox>>,
    highlights: BTreeMap<u32, Vec<Highlight>>,
    added_highlights: BTreeMap<u32, Vec<Highlight>>,
    fingerprint: Option<Fingerprint>,
}

/// Which file, as it is on disk: a save checks the PDF hasn't changed since editing began.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
struct Fingerprint {
    size: u64,
    mtime: (i64, i64),
    /// Renames on the same filesystem preserve this identity. Old stores omitted it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    file_id: Option<[u64; 2]>,
}

impl Fingerprint {
    fn of(path: &Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        Some(Self {
            size: meta.size(),
            mtime: (meta.mtime(), meta.mtime_nsec()),
            file_id: Some([meta.dev(), meta.ino()]),
        })
    }

    /// Old stores can still be restored at their original path, but cannot identify a move.
    fn matches(&self, current: &Self) -> bool {
        self.size == current.size
            && self.mtime == current.mtime
            && self.file_id.is_none_or(|id| Some(id) == current.file_id)
    }
}

/// What is kept on disk until the PDF is saved.
#[derive(Serialize, Deserialize)]
struct Unsaved {
    pdf: PathBuf,
    fingerprint: Option<Fingerprint>,
    /// Every box of each edited page.
    pages: BTreeMap<u32, Vec<TextBox>>,
    /// Boxes added to pages not yet edited, on top of the file's own.
    added: BTreeMap<u32, Vec<TextBox>>,
    /// The same for highlights.
    #[serde(default)]
    highlights: BTreeMap<u32, Vec<Highlight>>,
    #[serde(default)]
    added_highlights: BTreeMap<u32, Vec<Highlight>>,
}

/// [`Unsaved`], borrowed to be written without copying the edits.
#[derive(Serialize)]
struct UnsavedRef<'a> {
    pdf: &'a Path,
    fingerprint: Option<Fingerprint>,
    pages: &'a BTreeMap<u32, Vec<TextBox>>,
    added: &'a BTreeMap<u32, Vec<TextBox>>,
    highlights: &'a BTreeMap<u32, Vec<Highlight>>,
    added_highlights: &'a BTreeMap<u32, Vec<Highlight>>,
}

/// One page's boxes to write into the PDF.
pub struct PageSave {
    pub page: u32,
    /// Each box with its rect as fractions of the page upright.
    pub boxes: Vec<(TextBox, Rect)>,
    /// The page's every box: the file's boxes missing from it were deleted. Otherwise only
    /// adds to the file's.
    pub complete: bool,
    /// Highlights, as `boxes`, `complete` for them.
    pub highlights: Vec<Highlight>,
    pub highlights_complete: bool,
}

impl PageSave {
    /// `page`'s `boxes`, leaving its highlights as they are.
    pub fn boxes(page: u32, boxes: Vec<(TextBox, Rect)>, complete: bool) -> Self {
        Self { page, boxes, complete, highlights: Vec::new(), highlights_complete: false }
    }
}

/// The PDFs whose closed tabs' saves are still running, with the condition their end signals.
static DETACHED: (std::sync::Mutex<Vec<PathBuf>>, std::sync::Condvar) =
    (std::sync::Mutex::new(Vec::new()), std::sync::Condvar::new());

/// Waits for the saves of closed tabs (see [`TextBoxes::detach_saving`]), so quitting
/// doesn't cut one off halfway through writing a PDF.
pub fn wait_for_detached_saves() {
    let mut running = DETACHED.0.lock().unwrap_or_else(|p| p.into_inner());
    while !running.is_empty() {
        running = DETACHED.1.wait(running).unwrap_or_else(|p| p.into_inner());
    }
}

/// Waits for a closed tab's save of `pdf`, so reopening it reads the saved file, and that
/// save's end doesn't remove edits made since.
fn wait_for_detached_save(pdf: &Path) {
    let mut running = DETACHED.0.lock().unwrap_or_else(|p| p.into_inner());
    while running.iter().any(|p| p == pdf) {
        running = DETACHED.1.wait(running).unwrap_or_else(|p| p.into_inner());
    }
}

/// A save under way, or how the last one went.
pub enum Saving {
    Running(Receiver<Result<crate::engine::SaveOutcome, String>>),
    Failed(String),
    /// Finished at this time (egui's clock), shown as saved for a moment.
    Done(f64, crate::engine::SaveOutcome),
}

/// One tab's text boxes.
pub struct TextBoxes {
    pdf: PathBuf,
    /// Where unsaved edits are kept; `None` keeps them in memory only.
    store: Option<PathBuf>,
    /// Boxes the file has, by page, read as pages come into view. Only visible pages are
    /// kept beside the snapshot when the tab goes to the background.
    file: HashMap<u32, Vec<TextBox>>,
    /// Every box of each page edited since the last save.
    edited: BTreeMap<u32, Vec<TextBox>>,
    /// Boxes added to pages whose own boxes haven't been read: kept from an earlier session
    /// when the PDF changed since, and merged into [`Self::edited`] once the page is read.
    added: BTreeMap<u32, Vec<TextBox>>,
    /// Highlights, kept as boxes are.
    file_highlights: HashMap<u32, Vec<Highlight>>,
    edited_highlights: BTreeMap<u32, Vec<Highlight>>,
    added_highlights: BTreeMap<u32, Vec<Highlight>>,
    /// The PDF as it was when editing began.
    fingerprint: Option<Fingerprint>,
    undo: Vec<Undo>,
    pub selected: Option<BoxRef>,
    pub editing: Option<Editing>,
    /// A box being dragged, and how far so far, as fractions of the page upright. Moved
    /// for good when the drag ends, so the drag is one change to undo.
    pub dragging: Option<(BoxRef, Vec2)>,
    /// A box's side being dragged, and how far so far in PDF points, kept as
    /// [`Self::dragging`] is.
    pub resizing: Option<(BoxRef, Edge, f32)>,
    pub saving: Option<Saving>,
}

/// Where unsaved text boxes are kept, a file per PDF.
pub fn dir() -> Option<PathBuf> {
    crate::config::support_file("Annotations")
}

impl TextBoxes {
    /// The boxes of the PDF at `pdf`, with any edits left unsaved in `dir`.
    pub fn load(pdf: &Path, dir: Option<&Path>) -> Self {
        wait_for_detached_save(pdf);
        let store = dir.map(|d| {
            d.join(format!(
                "{:016x}.json",
                crate::text::fnv(crate::text::FNV_OFFSET, pdf.as_os_str().as_encoded_bytes())
            ))
        });
        let read = |s: &Path| std::fs::read(s).ok().and_then(|bytes| serde_json::from_slice::<Unsaved>(&bytes).ok());
        let mut this = Self {
            pdf: pdf.to_owned(),
            store,
            file: HashMap::default(),
            edited: BTreeMap::new(),
            added: BTreeMap::new(),
            file_highlights: HashMap::default(),
            edited_highlights: BTreeMap::new(),
            added_highlights: BTreeMap::new(),
            fingerprint: None,
            undo: Vec::new(),
            selected: None,
            editing: None,
            dragging: None,
            resizing: None,
            saving: None,
        };
        let mut unsaved = this.store.as_deref().and_then(read).filter(|u| u.pdf == pdf);
        let mut moved_from = None;
        if unsaved.is_none()
            && let Some(dir) = dir
            && let Some((edits, source)) = moved_here(dir, pdf, read)
        {
            unsaved = Some(edits);
            moved_from = Some(source);
        }
        if let Some(unsaved) = unsaved {
            let current = Fingerprint::of(pdf);
            this.added = unsaved.added;
            this.added_highlights = unsaved.added_highlights;
            if unsaved.fingerprint.zip(current).is_some_and(|(old, new)| old.matches(&new)) {
                this.edited = unsaved.pages;
                this.edited_highlights = unsaved.highlights;
            } else {
                // The PDF changed since (saved from Preview, say), so its boxes may no longer
                // be the ones the edits changed: only the boxes added here are kept, on top
                // of the file's own.
                for (page, boxes) in unsaved.pages {
                    let new = boxes.into_iter().filter(|b| b.origin.is_none());
                    this.added.entry(page).or_default().extend(new);
                }
                this.added.retain(|_, boxes| !boxes.is_empty());
                for (page, highlights) in unsaved.highlights {
                    let new = highlights.into_iter().filter(|h| h.origin.is_none());
                    this.added_highlights.entry(page).or_default().extend(new);
                }
                this.added_highlights.retain(|_, highlights| !highlights.is_empty());
            }
            this.fingerprint = current;
            if let Some(source) = moved_from
                && this.persist()
            {
                let _ = std::fs::remove_file(source);
            }
        }
        this
    }

    /// The boxes of `page`, or `None` until the file's have been read.
    pub fn page(&self, page: u32) -> Option<&[TextBox]> {
        self.edited.get(&page).or_else(|| self.file.get(&page)).map(Vec::as_slice)
    }

    pub fn get(&self, r: BoxRef) -> Option<&TextBox> {
        self.page(r.page)?.get(r.index)
    }

    /// The highlights of `page`, or `None` until the file's have been read.
    pub fn highlights(&self, page: u32) -> Option<&[Highlight]> {
        self.edited_highlights.get(&page).or_else(|| self.file_highlights.get(&page)).map(Vec::as_slice)
    }

    /// The file's boxes and highlights of `page`, read with its links.
    pub fn set_file(&mut self, page: u32, boxes: Vec<TextBox>, highlights: Vec<Highlight>) {
        self.file.insert(page, boxes);
        self.file_highlights.insert(page, highlights);
        if self.merge_added(page) {
            self.persist();
        }
    }

    /// Puts the boxes and highlights added to `page` on top of the file's, once those have
    /// been read. Whether there were any.
    fn merge_added(&mut self, page: u32) -> bool {
        let mut merged = false;
        if let Some(file) = self.file.get(&page)
            && let Some(added) = self.added.remove(&page)
        {
            let mut all = file.clone();
            all.extend(added);
            self.edited.entry(page).or_insert(all);
            merged = true;
        }
        if let Some(file) = self.file_highlights.get(&page)
            && let Some(added) = self.added_highlights.remove(&page)
        {
            let mut all = file.clone();
            all.extend(added);
            self.edited_highlights.entry(page).or_insert(all);
            merged = true;
        }
        merged
    }

    /// Lets go of the file's boxes and highlights, read again when their pages are next shown.
    pub fn clear_file(&mut self) {
        self.file.clear();
        self.file_highlights.clear();
    }

    /// Keeps only the visible pages' saved annotations beside a background snapshot.
    pub fn retain_file_pages(&mut self, pages: &[u32]) {
        self.file.retain(|page, _| pages.contains(page));
        self.file_highlights.retain(|page, _| pages.contains(page));
    }

    /// Edits not yet written into the PDF.
    pub fn unsaved(&self) -> bool {
        !self.edited.is_empty()
            || !self.added.is_empty()
            || !self.edited_highlights.is_empty()
            || !self.added_highlights.is_empty()
    }

    /// A save is writing the edits into the PDF. Nothing can be changed until it's done: the
    /// save ends by starting again from the file, which would drop a change made meanwhile.
    pub fn saving(&self) -> bool {
        matches!(self.saving, Some(Saving::Running(_)))
    }

    /// A box's text is being typed, so the keyboard is the text field's.
    pub fn typing(&self) -> bool {
        self.editing.is_some()
    }

    /// Changes `page`'s boxes, which can be undone, and keeps the change on disk.
    pub fn change<R>(&mut self, page: u32, f: impl FnOnce(&mut Vec<TextBox>) -> R) -> R {
        if !self.unsaved() {
            self.fingerprint = Fingerprint::of(&self.pdf);
        }
        let before = self.edited.get(&page).cloned();
        let file = self.file.get(&page).cloned().unwrap_or_default();
        let boxes = self.edited.entry(page).or_insert_with(|| file.clone());
        let result = f(boxes);
        if *boxes == *before.as_ref().unwrap_or(&file) {
            if before.is_none() {
                self.edited.remove(&page);
            }
            return result;
        }
        self.undo.push(Undo::Boxes(page, before));
        self.persist();
        result
    }

    /// Changes `page`'s highlights, as [`Self::change`] does its boxes.
    fn change_highlights(&mut self, page: u32, f: impl FnOnce(&mut Vec<Highlight>)) {
        if !self.unsaved() {
            self.fingerprint = Fingerprint::of(&self.pdf);
        }
        let before = self.edited_highlights.get(&page).cloned();
        let file = self.file_highlights.get(&page).cloned().unwrap_or_default();
        let highlights = self.edited_highlights.entry(page).or_insert_with(|| file.clone());
        f(highlights);
        if *highlights == *before.as_ref().unwrap_or(&file) {
            if before.is_none() {
                self.edited_highlights.remove(&page);
            }
            return;
        }
        self.undo.push(Undo::Highlights(page, before));
        self.persist();
    }

    /// Highlights `rects` (see [`Highlight::rects`]) on `page` in `color`, as
    /// [`add_highlight`] does.
    pub fn highlight(&mut self, page: u32, rects: Vec<Rect>, color: [u8; 3]) {
        if !rects.is_empty() && !self.saving() {
            self.change_highlights(page, |highlights| add_highlight(highlights, rects, color));
        }
    }

    pub fn delete_highlight(&mut self, page: u32, index: usize) {
        if self.saving() {
            return;
        }
        self.change_highlights(page, |highlights| {
            if index < highlights.len() {
                highlights.remove(index);
            }
        });
    }

    /// Changes one box, marking it changed.
    pub fn change_box(&mut self, r: BoxRef, f: impl FnOnce(&mut TextBox)) {
        if self.saving() {
            return;
        }
        self.change(r.page, |boxes| {
            if let Some(b) = boxes.get_mut(r.index) {
                f(b);
                b.changed = true;
            }
        });
    }

    pub fn delete(&mut self, r: BoxRef) {
        if self.saving() {
            return;
        }
        self.change(r.page, |boxes| {
            if r.index < boxes.len() {
                boxes.remove(r.index);
            }
        });
        self.selected = None;
        if self.editing.as_ref().is_some_and(|e| e.page == r.page) {
            self.editing = None;
        }
    }

    /// Starts typing into a new box of `size` and `color` at `at` (see [`TextBox::at`]) on
    /// `page`.
    pub fn add(&mut self, page: u32, at: Pos2, size: f32, color: [u8; 3]) {
        if self.saving() {
            return;
        }
        self.finish_editing();
        self.selected = None;
        self.editing =
            Some(Editing { page, index: None, at, draft: String::new(), size, color, width: None, focus: true });
    }

    /// Starts typing into an existing box.
    pub fn edit(&mut self, r: BoxRef) {
        if self.saving() {
            return;
        }
        self.finish_editing();
        let Some(b) = self.get(r) else { return };
        self.editing = Some(Editing {
            page: r.page,
            index: Some(r.index),
            at: b.at(),
            draft: b.text.clone(),
            size: b.size,
            color: b.color,
            width: b.width,
            focus: true,
        });
        self.selected = Some(r);
    }

    /// Ends typing: keeps the text, size, colour and width (one change to undo), adding a new box
    /// if it has text, and deletes a box emptied of it.
    pub fn finish_editing(&mut self) {
        let Some(editing) = self.editing.take() else { return };
        let empty = editing.draft.trim().is_empty();
        match editing.index {
            None if empty => {}
            None => {
                let index = self.change(editing.page, |boxes| {
                    let b = TextBox {
                        size: editing.size,
                        color: editing.color,
                        width: editing.width,
                        ..TextBox::new(editing.at, editing.draft)
                    };
                    boxes.push(b);
                    boxes.len() - 1
                });
                self.selected = Some(BoxRef { page: editing.page, index });
            }
            Some(index) => {
                let r = BoxRef { page: editing.page, index };
                let unchanged = |b: &TextBox| {
                    b.text == editing.draft
                        && b.size == editing.size
                        && b.color == editing.color
                        && b.width == editing.width
                        && b.at() == editing.at
                };
                if empty {
                    self.delete(r);
                } else if self.get(r).is_some_and(|b| !unchanged(b)) {
                    self.change_box(r, |b| {
                        b.text = editing.draft;
                        b.size = editing.size;
                        b.color = editing.color;
                        b.width = editing.width;
                        b.at = [editing.at.x, editing.at.y];
                    });
                }
            }
        }
    }

    /// Stops typing and throws the box away: a new one is never added, an existing one is
    /// deleted.
    pub fn discard_editing(&mut self) {
        let Some(editing) = self.editing.take() else { return };
        if let Some(index) = editing.index {
            self.delete(BoxRef { page: editing.page, index });
        }
    }

    /// Undoes the last change. `false` if there was none.
    pub fn undo(&mut self) -> bool {
        if self.saving() {
            return false;
        }
        match self.undo.pop() {
            None => return false,
            Some(Undo::Boxes(page, Some(boxes))) => drop(self.edited.insert(page, boxes)),
            Some(Undo::Boxes(page, None)) => drop(self.edited.remove(&page)),
            Some(Undo::Highlights(page, Some(highlights))) => drop(self.edited_highlights.insert(page, highlights)),
            Some(Undo::Highlights(page, None)) => drop(self.edited_highlights.remove(&page)),
            Some(Undo::Revert(edits)) => {
                let Edits { pages, added, highlights, added_highlights, fingerprint } = *edits;
                (self.edited, self.added) = (pages, added);
                (self.edited_highlights, self.added_highlights) = (highlights, added_highlights);
                self.fingerprint = fingerprint;
                // Pages read since the revert show the file's own: put the added back on them.
                let pages: Vec<u32> = self.added.keys().chain(self.added_highlights.keys()).copied().collect();
                for page in pages {
                    self.merge_added(page);
                }
            }
        }
        self.selected = None;
        self.editing = None;
        self.persist();
        true
    }

    /// The pages to write into the PDF, each box with its rect from `rect_of`; or why it
    /// can't be saved.
    pub fn save_job(&self, rect_of: impl Fn(u32, &TextBox) -> Rect) -> Result<Vec<PageSave>, String> {
        if self.fingerprint.is_some() && Fingerprint::of(&self.pdf) != self.fingerprint {
            return Err("The file changed on disk since you started editing it".into());
        }
        let mut pages: BTreeMap<u32, PageSave> = BTreeMap::new();
        fn page(pages: &mut BTreeMap<u32, PageSave>, page: u32) -> &mut PageSave {
            pages.entry(page).or_insert_with(|| PageSave::boxes(page, Vec::new(), false))
        }
        let with_rects = |page: u32, boxes: &[TextBox]| boxes.iter().map(|b| (b.clone(), rect_of(page, b))).collect();
        for (complete, boxes) in [(true, &self.edited), (false, &self.added)] {
            for (&p, boxes) in boxes {
                let save = page(&mut pages, p);
                save.boxes = with_rects(p, boxes);
                save.complete = complete;
            }
        }
        for (complete, highlights) in [(true, &self.edited_highlights), (false, &self.added_highlights)] {
            for (&p, highlights) in highlights {
                let save = page(&mut pages, p);
                save.highlights = highlights.clone();
                save.highlights_complete = complete;
            }
        }
        Ok(pages.into_values().collect())
    }

    /// The PDF now has every edit: start again from what it has.
    pub fn saved(&mut self) {
        self.clear_file();
        self.edited.clear();
        self.added.clear();
        self.edited_highlights.clear();
        self.added_highlights.clear();
        self.undo.clear();
        self.selected = None;
        self.fingerprint = None;
        self.persist();
    }

    /// Drops every edit not yet saved, back to what the PDF has. Undone as one change.
    pub fn revert(&mut self) {
        if self.saving() {
            return;
        }
        self.editing = None;
        if !self.unsaved() {
            return;
        }
        let edits = Edits {
            pages: std::mem::take(&mut self.edited),
            added: std::mem::take(&mut self.added),
            highlights: std::mem::take(&mut self.edited_highlights),
            added_highlights: std::mem::take(&mut self.added_highlights),
            fingerprint: self.fingerprint.take(),
        };
        self.undo.push(Undo::Revert(Box::new(edits)));
        self.selected = None;
        self.dragging = None;
        self.resizing = None;
        self.persist();
    }

    /// Waits for a save under way to finish, as when the tab closes: once the PDF has the
    /// edits, the store must go, or they'd be added again on top of the file's own.
    pub fn finish_saving(&mut self) {
        let Some(Saving::Running(result)) = &self.saving else { return };
        match result.recv() {
            Ok(Ok(_)) => {
                self.saving = None;
                self.saved();
            }
            Ok(Err(e)) => self.saving = Some(Saving::Failed(e)),
            Err(_) => self.saving = Some(Saving::Failed("The save stopped unexpectedly".into())),
        }
    }

    /// Lets a save under way finish without waiting, as when the tab closes: the save
    /// goes on in the background, and once the PDF has the edits the store goes, or they'd
    /// be added again on top of the file's own. If the save fails the store stays, so the
    /// edits are offered again when the PDF is next opened. [`wait_for_detached_saves`]
    /// holds the quit until they're done.
    pub fn detach_saving(&mut self) {
        let Some(Saving::Running(result)) = self.saving.take() else { return };
        let store = self.store.clone();
        let pdf = self.pdf.clone();
        DETACHED.0.lock().unwrap_or_else(|p| p.into_inner()).push(pdf.clone());
        let done = move || {
            let mut running = DETACHED.0.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(i) = running.iter().position(|p| *p == pdf) {
                running.swap_remove(i);
            }
            DETACHED.1.notify_all();
        };
        let spawned = std::thread::Builder::new().name("save-wait".into()).spawn({
            let done = done.clone();
            move || {
                if let (Ok(Ok(_)), Some(store)) = (result.recv(), store) {
                    let _ = std::fs::remove_file(store);
                }
                done();
            }
        });
        if spawned.is_err() {
            // The edits stay in the store, but the save must not hold the quit forever.
            done();
        }
    }

    /// Writes the unsaved edits to the store, or removes it once there are none.
    fn persist(&self) -> bool {
        let Some(store) = &self.store else { return true };
        if !self.unsaved() {
            return std::fs::remove_file(store).is_ok() || !store.exists();
        }
        let unsaved = UnsavedRef {
            pdf: &self.pdf,
            fingerprint: self.fingerprint,
            pages: &self.edited,
            added: &self.added,
            highlights: &self.edited_highlights,
            added_highlights: &self.added_highlights,
        };
        serde_json::to_vec(&unsaved).is_ok_and(|json| crate::config::write_atomically(store, &json))
    }
}

/// `b`'s rect as fractions of the page upright, `upright` PDF points in size.
pub fn rect_on_page(ctx: &Context, b: &TextBox, upright: Vec2) -> Rect {
    // Laid out large, so rounding to pixels doesn't shift the measure.
    const SCALE: f32 = 4.0;
    let size = b.size_around(&b.galley(ctx, &b.text, SCALE), SCALE);
    Rect::from_min_size(b.at(), size / upright)
}

/// Where `at` (fractions of the page upright) is on the page turned by `rotation`.
pub fn turn(at: Pos2, rotation: Rotation) -> Pos2 {
    match rotation.degrees() {
        90 => pos2(1.0 - at.y, at.x),
        180 => pos2(1.0 - at.x, 1.0 - at.y),
        270 => pos2(at.y, 1.0 - at.x),
        _ => at,
    }
}

/// The inverse of [`turn`]: where a point of the turned page is on the page upright.
pub fn unturn(at: Pos2, rotation: Rotation) -> Pos2 {
    match rotation.degrees() {
        90 => pos2(at.y, 1.0 - at.x),
        180 => pos2(1.0 - at.x, 1.0 - at.y),
        270 => pos2(1.0 - at.y, at.x),
        _ => at,
    }
}

/// Edits kept for a PDF that has since been renamed or moved to `pdf`: kept for a file no
/// longer where it was, with the same filesystem identity and metadata. The caller keeps
/// the old store until its replacement has been written successfully.
fn moved_here(dir: &Path, pdf: &Path, read: impl Fn(&Path) -> Option<Unsaved>) -> Option<(Unsaved, PathBuf)> {
    let here = Fingerprint::of(pdf)?;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let store = entry.path();
        if store.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let Some(unsaved) = read(&store) else { continue };
        if here.file_id.is_some() && unsaved.fingerprint == Some(here) && !unsaved.pdf.exists() {
            return Some((Unsaved { pdf: pdf.to_owned(), ..unsaved }, store));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lightweight-pdf-text-boxes-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn pdf(dir: &Path) -> PathBuf {
        let path = dir.join("doc.pdf");
        std::fs::write(&path, b"%PDF-1.4").unwrap();
        path
    }

    fn type_box(boxes: &mut TextBoxes, page: u32, text: &str) {
        boxes.add(page, pos2(0.25, 0.5), DEFAULT_SIZE, COLORS[0].1);
        boxes.editing.as_mut().unwrap().draft = text.into();
        boxes.finish_editing();
    }

    #[test]
    fn boxes_lay_out_and_grow_with_their_text() {
        let ctx = Context::default();
        let _ = ctx.run(Default::default(), |ctx| {
            let b = TextBox::new(pos2(0.0, 0.0), String::new());
            let short = b.size_around(&b.galley(ctx, "Hi", 1.0), 1.0);
            let long = b.size_around(&b.galley(ctx, "Hi there, a longer line", 1.0), 1.0);
            let two_lines = b.size_around(&b.galley(ctx, "Hi\nthere", 1.0), 1.0);
            assert!(long.x > short.x && two_lines.y > short.y, "{short:?} {long:?} {two_lines:?}");
            assert!(short.y >= DEFAULT_SIZE + 2.0 * PAD);
        });
    }

    /// egui's glyph texture at its largest during a pinch from 50% to 400% and back over a
    /// box of text, a frame per 1% of zoom at 2 pixels per point.
    fn atlas_peak_during_pinch(rounded: bool) -> usize {
        let ctx = Context::default();
        let b = TextBox::new(pos2(0.0, 0.0), String::new());
        let text = "The quick brown fox jumps over the lazy dog 0123456789";
        // Two frames, so Helvetica is loaded.
        for _ in 0..2 {
            let _ = ctx.run(Default::default(), |ctx| drop(b.galley(ctx, text, 1.0)));
        }
        let zooms: Vec<f32> = (0..210).map(|i| 0.5 * 1.01f32.powi(i)).collect();
        let mut peak = 0;
        for zoom in zooms.iter().chain(zooms.iter().rev()) {
            let scale = glyph_scale(zoom * 2.0, rounded);
            let _ = ctx.run(Default::default(), |ctx| drop(b.galley(ctx, text, scale)));
            let [w, h] = ctx.fonts(|f| f.font_image_size());
            peak = peak.max(w * h * 4);
        }
        peak
    }

    #[test]
    fn rounding_text_while_zooming_keeps_the_glyph_texture_small() {
        let (exact, rounded) = (atlas_peak_during_pinch(false), atlas_peak_during_pinch(true));
        eprintln!("glyph texture peak: {:.1} MB exact, {:.1} MB rounded", exact as f64 / 1e6, rounded as f64 / 1e6);
        assert!(rounded * 4 <= exact, "{rounded} vs {exact} bytes");
    }

    #[test]
    fn a_pinch_draws_text_at_few_sizes() {
        let mut sizes: Vec<u32> =
            (0..210).map(|i| glyph_scale(1.0 * 1.01f32.powi(i), true)).map(|s| (s * 1000.0).round() as u32).collect();
        sizes.dedup();
        // Quarter octaves over three octaves, not one per frame.
        assert!(sizes.len() <= 13, "{} sizes for an 8x pinch", sizes.len());
        assert_eq!(glyph_scale(1.37, false), 1.37);
        assert!((glyph_scale(1.37, true) / 1.37 - 1.0).abs() < 0.1);
    }

    #[test]
    fn turning_round_trips() {
        let at = pos2(0.2, 0.7);
        let mut rotation = Rotation::default();
        for _ in 0..4 {
            assert!((unturn(turn(at, rotation), rotation) - at).length() < 1e-6);
            rotation = rotation.clockwise();
        }
        // A quarter turn clockwise takes the top-left corner to the top-right.
        assert_eq!(turn(pos2(0.0, 0.0), Rotation::default().clockwise()), pos2(1.0, 0.0));
    }

    #[test]
    fn dragging_a_side_sets_the_width_and_keeps_the_other_side() {
        // 12 pt text, 100 points wide, its left edge 50 points in on a 500 point page.
        let b = TextBox::new(pos2(0.1, 0.5), "text".into());
        let right = b.resized(Edge::Right, -30.0, 100.0, 500.0);
        assert_eq!((right.width, right.at), (Some(70.0), b.at));
        let left = b.resized(Edge::Left, 30.0, 100.0, 500.0);
        assert_eq!(left.width, Some(70.0));
        assert!((left.at[0] - 0.16).abs() < 1e-6);
        // No narrower than a letter, and the left edge stays on the page.
        assert_eq!(b.resized(Edge::Right, -200.0, 100.0, 500.0).width, Some(16.0));
        let narrowest = b.resized(Edge::Left, 200.0, 100.0, 500.0);
        assert_eq!(narrowest.width, Some(16.0));
        assert!((narrowest.at[0] - 0.268).abs() < 1e-6);
        let widest = b.resized(Edge::Left, -200.0, 100.0, 500.0);
        assert_eq!((widest.width, widest.at[0]), (Some(150.0), 0.0));
    }

    #[test]
    fn resizing_while_typing_is_kept_with_the_text() {
        let dir = temp_dir("resize");
        let mut boxes = TextBoxes::load(&pdf(&dir), Some(&dir));
        boxes.set_file(0, vec![TextBox::new(pos2(0.1, 0.1), "a long line".into())], Vec::new());
        let r = BoxRef { page: 0, index: 0 };
        boxes.edit(r);
        let editing = boxes.editing.as_mut().unwrap();
        editing.width = Some(40.0);
        editing.at.x = 0.2;
        boxes.finish_editing();
        let b = boxes.get(r).unwrap();
        assert_eq!((b.width, b.at[0], b.changed), (Some(40.0), 0.2, true));
        assert!(boxes.undo());
        assert_eq!(boxes.get(r).unwrap().width, None);
    }

    #[test]
    fn a_box_typed_into_is_added_and_an_empty_one_is_not() {
        let dir = temp_dir("add");
        let mut boxes = TextBoxes::load(&pdf(&dir), Some(&dir));
        boxes.set_file(0, Vec::new(), Vec::new());
        type_box(&mut boxes, 0, "   ");
        assert!(!boxes.unsaved());
        type_box(&mut boxes, 0, "Hello");
        assert_eq!(boxes.page(0).unwrap()[0].text, "Hello");
        assert_eq!(boxes.selected, Some(BoxRef { page: 0, index: 0 }));
    }

    #[test]
    fn edits_survive_reopening_until_saved() {
        let dir = temp_dir("persist");
        let pdf = pdf(&dir);
        let mut boxes = TextBoxes::load(&pdf, Some(&dir));
        type_box(&mut boxes, 2, "Note");
        let reopened = TextBoxes::load(&pdf, Some(&dir));
        assert_eq!(reopened.page(2).unwrap()[0].text, "Note");
        boxes.saved();
        assert!(TextBoxes::load(&pdf, Some(&dir)).page(2).is_none());
    }

    #[test]
    fn a_changed_pdf_keeps_only_added_boxes_on_top_of_its_own() {
        let dir = temp_dir("changed");
        let pdf = pdf(&dir);
        let mut boxes = TextBoxes::load(&pdf, Some(&dir));
        let from_file = TextBox { origin: Some(0), changed: false, ..TextBox::new(pos2(0.1, 0.1), "File".into()) };
        boxes.set_file(0, vec![from_file.clone()], Vec::new());
        boxes.change_box(BoxRef { page: 0, index: 0 }, |b| b.text = "Edited".into());
        type_box(&mut boxes, 0, "Mine");

        std::fs::write(&pdf, b"%PDF-1.4 changed elsewhere").unwrap();
        let mut reopened = TextBoxes::load(&pdf, Some(&dir));
        assert!(reopened.page(0).is_none(), "waits for the file's own boxes");
        let theirs = TextBox { text: "Theirs".into(), ..from_file };
        reopened.set_file(0, vec![theirs], Vec::new());
        let texts: Vec<&str> = reopened.page(0).unwrap().iter().map(|b| b.text.as_str()).collect();
        assert_eq!(texts, ["Theirs", "Mine"]);
    }

    #[test]
    fn undo_restores_the_page() {
        let dir = temp_dir("undo");
        let mut boxes = TextBoxes::load(&pdf(&dir), Some(&dir));
        boxes.set_file(0, Vec::new(), Vec::new());
        type_box(&mut boxes, 0, "One");
        boxes.edit(BoxRef { page: 0, index: 0 });
        let editing = boxes.editing.as_mut().unwrap();
        editing.draft = "One more".into();
        editing.size = 24.0;
        boxes.finish_editing();
        assert_eq!(boxes.page(0).unwrap()[0].size, 24.0);
        assert!(boxes.undo(), "text and size together");
        assert_eq!(boxes.page(0).unwrap()[0].text, "One");
        assert_eq!(boxes.page(0).unwrap()[0].size, DEFAULT_SIZE);
        assert!(boxes.undo());
        assert!(boxes.page(0).unwrap().is_empty(), "back to the file's boxes");
        assert!(!boxes.unsaved());
        assert!(!boxes.undo());
    }

    #[test]
    fn highlights_never_overlap() {
        let line = |x0: f32, x1: f32| Rect::from_min_max(pos2(x0, 0.1), pos2(x1, 0.12));
        type Spans = Vec<(Vec<(f32, f32)>, [u8; 3])>;
        let spans = |hs: &[Highlight]| -> Spans {
            hs.iter().map(|h| (h.rects().map(|r| (r.min.x, r.max.x)).collect(), h.color)).collect()
        };
        let (yellow, green) = (HIGHLIGHT_YELLOW, [110, 214, 96]);
        let from_file =
            Highlight { origin: Some(0), ..Highlight::new([Rect::from_min_max(pos2(0.1, 0.5), pos2(0.3, 0.52))]) };
        let mut hs = vec![from_file.clone()];

        add_highlight(&mut hs, vec![line(0.1, 0.3)], yellow);
        add_highlight(&mut hs, vec![line(0.25, 0.5)], yellow);
        assert_eq!(hs[0], from_file, "one elsewhere is left alone");
        assert_eq!(spans(&hs[1..]), [(vec![(0.1, 0.5)], yellow)], "the same colour merges");

        add_highlight(&mut hs, vec![line(0.2, 0.3)], green);
        assert_eq!(spans(&hs[1..]), [(vec![(0.1, 0.2), (0.3, 0.5)], yellow), (vec![(0.2, 0.3)], green)]);

        add_highlight(&mut hs, vec![line(0.3, 0.4)], yellow);
        assert_eq!(spans(&hs[1..]), [(vec![(0.1, 0.2), (0.4, 0.5)], yellow), (vec![(0.2, 0.3)], green)], "erased");

        add_highlight(&mut hs, vec![line(0.0, 0.6)], green);
        assert_eq!(spans(&hs[1..]), [(vec![(0.0, 0.6)], green)], "covers the others");
        let mut file = vec![from_file];
        add_highlight(&mut file, vec![Rect::from_min_max(pos2(0.2, 0.5), pos2(0.3, 0.52))], [0, 0, 255]);
        assert_eq!(file[0].origin, None, "a trimmed one from the file is saved anew");
    }

    #[test]
    fn highlights_persist_undo_and_save_apart_from_boxes() {
        let dir = temp_dir("highlights");
        let pdf = pdf(&dir);
        let mut boxes = TextBoxes::load(&pdf, Some(&dir));
        let theirs =
            Highlight { origin: Some(0), ..Highlight::new([Rect::from_min_max(pos2(0.1, 0.1), pos2(0.5, 0.12))]) };
        boxes.set_file(0, Vec::new(), vec![theirs]);
        boxes.highlight(0, vec![Rect::from_min_max(pos2(0.1, 0.2), pos2(0.6, 0.22))], HIGHLIGHT_YELLOW);
        type_box(&mut boxes, 1, "Note");
        assert_eq!(boxes.highlights(0).unwrap().len(), 2);
        assert_eq!(boxes.highlights(0).unwrap()[1].color, HIGHLIGHT_YELLOW);

        let reopened = TextBoxes::load(&pdf, Some(&dir));
        assert_eq!(reopened.highlights(0).unwrap().len(), 2);

        let job = boxes.save_job(|_, _| Rect::NOTHING).unwrap();
        assert_eq!(job.len(), 2);
        assert!(job[0].highlights_complete && job[0].highlights.len() == 2 && job[0].boxes.is_empty());
        assert!(!job[0].complete, "page 0's boxes are left as they are");
        assert!(job[1].complete && !job[1].highlights_complete && job[1].highlights.is_empty());

        assert!(boxes.undo(), "the box");
        assert!(boxes.undo(), "the highlight");
        assert_eq!(boxes.highlights(0).unwrap().len(), 1);
        assert!(!boxes.unsaved());
    }

    #[test]
    fn revert_drops_the_edits_and_undo_brings_them_back() {
        let dir = temp_dir("revert");
        let pdf = pdf(&dir);
        let mut boxes = TextBoxes::load(&pdf, Some(&dir));
        let from_file = TextBox { origin: Some(0), changed: false, ..TextBox::new(pos2(0.1, 0.1), "File".into()) };
        boxes.set_file(0, vec![from_file], Vec::new());
        type_box(&mut boxes, 0, "Mine");
        boxes.highlight(1, vec![Rect::from_min_max(pos2(0.1, 0.2), pos2(0.6, 0.22))], HIGHLIGHT_YELLOW);

        boxes.revert();
        assert!(!boxes.unsaved());
        assert_eq!(boxes.page(0).unwrap().len(), 1, "the file's own box stays");
        assert!(!TextBoxes::load(&pdf, Some(&dir)).unsaved(), "nothing kept on disk");

        assert!(boxes.undo(), "the revert, as one change");
        assert_eq!(boxes.page(0).unwrap()[1].text, "Mine");
        assert_eq!(boxes.highlights(1).unwrap().len(), 1);
        assert!(TextBoxes::load(&pdf, Some(&dir)).unsaved(), "kept on disk again");
        assert!(boxes.undo(), "the highlight");
    }

    #[test]
    fn edits_follow_a_renamed_pdf() {
        let dir = temp_dir("renamed");
        let pdf = pdf(&dir);
        let mut boxes = TextBoxes::load(&pdf, Some(&dir));
        type_box(&mut boxes, 0, "Note");
        let renamed = dir.join("renamed.pdf");
        std::fs::rename(&pdf, &renamed).unwrap();

        assert_eq!(TextBoxes::load(&renamed, Some(&dir)).page(0).unwrap()[0].text, "Note");
        assert_eq!(TextBoxes::load(&renamed, Some(&dir)).page(0).unwrap()[0].text, "Note", "now kept under its name");
        std::fs::write(&pdf, b"%PDF-1.4").unwrap();
        assert!(!TextBoxes::load(&pdf, Some(&dir)).unsaved(), "not under the old one");
    }

    #[test]
    fn matching_metadata_does_not_move_edits_to_a_different_file() {
        let dir = temp_dir("different-file");
        let original = pdf(&dir);
        let mut boxes = TextBoxes::load(&original, Some(&dir));
        type_box(&mut boxes, 0, "Private note");
        let source_store = boxes.store.clone().unwrap();
        let other = dir.join("other.pdf");
        std::fs::write(&other, b"%PDF-1.5").unwrap();
        let modified = std::fs::metadata(&original).unwrap().modified().unwrap();
        std::fs::File::open(&other).unwrap().set_modified(modified).unwrap();
        std::fs::remove_file(&original).unwrap();

        assert!(!TextBoxes::load(&other, Some(&dir)).unsaved());
        assert!(source_store.exists(), "unrelated opening must retain the original notes");
    }

    #[test]
    fn a_failed_migration_keeps_the_original_store_for_retry() {
        let dir = temp_dir("migration-failure");
        let original = pdf(&dir);
        let mut boxes = TextBoxes::load(&original, Some(&dir));
        type_box(&mut boxes, 0, "Note");
        let source_store = boxes.store.clone().unwrap();
        let renamed = dir.join("renamed.pdf");
        std::fs::rename(&original, &renamed).unwrap();
        let target_store = dir.join(format!(
            "{:016x}.json",
            crate::text::fnv(crate::text::FNV_OFFSET, renamed.as_os_str().as_encoded_bytes())
        ));
        std::fs::create_dir(&target_store).unwrap();

        assert_eq!(TextBoxes::load(&renamed, Some(&dir)).page(0).unwrap()[0].text, "Note");
        assert!(source_store.exists(), "failed replacement must not delete the source");
        std::fs::remove_dir(&target_store).unwrap();
        assert_eq!(TextBoxes::load(&renamed, Some(&dir)).page(0).unwrap()[0].text, "Note");
        assert!(!source_store.exists(), "successful replacement finishes the migration");
        assert!(target_store.is_file());
    }

    #[test]
    fn legacy_stores_restore_edits_at_the_original_path() {
        let dir = temp_dir("legacy-identity");
        let pdf = pdf(&dir);
        let mut boxes = TextBoxes::load(&pdf, Some(&dir));
        let b = TextBox { origin: Some(0), changed: false, ..TextBox::new(pos2(0.1, 0.1), "File".into()) };
        boxes.set_file(0, vec![b], Vec::new());
        boxes.change_box(BoxRef { page: 0, index: 0 }, |b| b.text = "Edited".into());
        let store = boxes.store.clone().unwrap();
        let mut json: serde_json::Value = serde_json::from_slice(&std::fs::read(&store).unwrap()).unwrap();
        json["fingerprint"].as_object_mut().unwrap().remove("file_id");
        std::fs::write(store, serde_json::to_vec(&json).unwrap()).unwrap();

        assert_eq!(TextBoxes::load(&pdf, Some(&dir)).page(0).unwrap()[0].text, "Edited");
    }

    #[test]
    fn snapshot_keeps_only_visible_file_annotations() {
        let dir = temp_dir("snapshot-annotations");
        let mut boxes = TextBoxes::load(&pdf(&dir), None);
        for page in 0..3 {
            boxes.set_file(
                page,
                vec![TextBox::new(pos2(0.1, 0.1), "File".into())],
                vec![Highlight::new([Rect::from_min_max(pos2(0.1, 0.2), pos2(0.4, 0.3))])],
            );
        }
        boxes.retain_file_pages(&[1]);
        assert!(boxes.page(0).is_none() && boxes.highlights(0).is_none());
        assert_eq!(boxes.page(1).unwrap()[0].text, "File");
        assert_eq!(boxes.highlights(1).unwrap().len(), 1);
        assert!(boxes.page(2).is_none() && boxes.highlights(2).is_none());
    }

    #[test]
    fn closing_during_a_save_waits_for_it_and_drops_what_it_saved() {
        let dir = temp_dir("closing");
        let pdf = pdf(&dir);
        let mut boxes = TextBoxes::load(&pdf, Some(&dir));
        type_box(&mut boxes, 0, "Note");
        let (done, result) = std::sync::mpsc::channel();
        boxes.saving = Some(Saving::Running(result));
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            let _ = done.send(Ok(crate::engine::SaveOutcome::Saved));
        });
        boxes.finish_saving();
        assert!(!TextBoxes::load(&pdf, Some(&dir)).unsaved(), "not added again on top of the file's");
    }

    #[test]
    fn reopening_during_a_closed_tabs_save_waits_for_it() {
        let dir = temp_dir("reopening");
        let pdf = pdf(&dir);
        let mut boxes = TextBoxes::load(&pdf, Some(&dir));
        type_box(&mut boxes, 0, "Note");
        let (done, result) = std::sync::mpsc::channel();
        boxes.saving = Some(Saving::Running(result));
        boxes.detach_saving();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            let _ = done.send(Ok(crate::engine::SaveOutcome::Saved));
        });
        let mut reopened = TextBoxes::load(&pdf, Some(&dir));
        assert!(!reopened.unsaved(), "the saved edits aren't offered again");
        type_box(&mut reopened, 0, "Later");
        wait_for_detached_saves();
        assert!(TextBoxes::load(&pdf, Some(&dir)).unsaved(), "the save's end kept the newer edit");
    }

    #[test]
    fn nothing_changes_while_a_save_runs() {
        let dir = temp_dir("locked");
        let pdf = pdf(&dir);
        let mut boxes = TextBoxes::load(&pdf, Some(&dir));
        boxes.set_file(0, Vec::new(), Vec::new());
        type_box(&mut boxes, 0, "Saving");
        let (done, result) = std::sync::mpsc::channel();
        boxes.saving = Some(Saving::Running(result));

        let r = BoxRef { page: 0, index: 0 };
        boxes.add(0, pos2(0.5, 0.5), DEFAULT_SIZE, COLORS[0].1);
        assert!(boxes.editing.is_none());
        boxes.change_box(r, |b| b.text = "Changed".into());
        boxes.highlight(0, vec![Rect::from_min_max(pos2(0.1, 0.2), pos2(0.6, 0.22))], HIGHLIGHT_YELLOW);
        boxes.delete(r);
        assert!(!boxes.undo());
        boxes.revert();
        assert_eq!(boxes.page(0).unwrap()[0].text, "Saving");
        assert!(boxes.highlights(0).unwrap().is_empty());

        done.send(Ok(crate::engine::SaveOutcome::Saved)).unwrap();
        boxes.finish_saving();
        assert!(!boxes.unsaved());
    }

    #[test]
    fn saving_is_refused_once_the_pdf_changed_on_disk() {
        let dir = temp_dir("refuse");
        let pdf = pdf(&dir);
        let mut boxes = TextBoxes::load(&pdf, Some(&dir));
        type_box(&mut boxes, 0, "Hi");
        assert!(boxes.save_job(|_, _| Rect::NOTHING).is_ok());
        std::fs::write(&pdf, b"%PDF-1.4 changed").unwrap();
        assert!(boxes.save_job(|_, _| Rect::NOTHING).is_err());
    }
}
