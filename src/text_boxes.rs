//! Text boxes typed onto pages: a PDF's FreeText annotations, which Preview makes with
//! Tools → Annotate → Text.
//!
//! A page shows the boxes the file has (read with its links) until it is first edited. From
//! then on the tab owns the page's boxes: every change is written to a file of the tab's own
//! (see [`dir`]), so closing the tab or quitting loses nothing, and ⌘S writes them into the
//! PDF. The app draws every box itself, those from the file included; the page renderer
//! draws no annotations, so a box being edited never shows twice.

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
    /// Size and colour, which the field's menu changes along with the text.
    pub size: f32,
    pub color: [u8; 3],
    /// Ask for the keyboard on the next frame.
    pub focus: bool,
}

/// A page's boxes as they were before a change, to undo it.
struct Undo {
    page: u32,
    /// `None` if the page was not yet edited.
    before: Option<Vec<TextBox>>,
}

/// Which file, as it is on disk: a save checks the PDF hasn't changed since editing began.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
struct Fingerprint {
    size: u64,
    mtime: (i64, i64),
}

impl Fingerprint {
    fn of(path: &Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        Some(Self { size: meta.size(), mtime: (meta.mtime(), meta.mtime_nsec()) })
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
}

/// One page's boxes to write into the PDF.
pub struct PageSave {
    pub page: u32,
    /// Each box with its rect as fractions of the page upright.
    pub boxes: Vec<(TextBox, Rect)>,
    /// The page's every box: the file's boxes missing from it were deleted. Otherwise only
    /// adds to the file's.
    pub complete: bool,
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
    /// Boxes the file has, by page, read as pages come into view. Let go with the links when
    /// the tab goes to the background.
    file: HashMap<u32, Vec<TextBox>>,
    /// Every box of each page edited since the last save.
    edited: BTreeMap<u32, Vec<TextBox>>,
    /// Boxes added to pages whose own boxes haven't been read: kept from an earlier session
    /// when the PDF changed since, and merged into [`Self::edited`] once the page is read.
    added: BTreeMap<u32, Vec<TextBox>>,
    /// The PDF as it was when editing began.
    fingerprint: Option<Fingerprint>,
    undo: Vec<Undo>,
    pub selected: Option<BoxRef>,
    pub editing: Option<Editing>,
    /// A box being dragged, and how far so far, as fractions of the page upright. Moved
    /// for good when the drag ends, so the drag is one change to undo.
    pub dragging: Option<(BoxRef, Vec2)>,
    pub saving: Option<Saving>,
}

/// Where unsaved text boxes are kept, a file per PDF.
pub fn dir() -> Option<PathBuf> {
    crate::config::support_file("Annotations")
}

impl TextBoxes {
    /// The boxes of the PDF at `pdf`, with any edits left unsaved in `dir`.
    pub fn load(pdf: &Path, dir: Option<&Path>) -> Self {
        let store = dir.map(|d| d.join(format!("{:016x}.json", fnv(pdf.as_os_str().as_encoded_bytes()))));
        let mut this = Self {
            pdf: pdf.to_owned(),
            store,
            file: HashMap::default(),
            edited: BTreeMap::new(),
            added: BTreeMap::new(),
            fingerprint: None,
            undo: Vec::new(),
            selected: None,
            editing: None,
            dragging: None,
            saving: None,
        };
        let unsaved = this
            .store
            .as_ref()
            .and_then(|s| std::fs::read(s).ok())
            .and_then(|bytes| serde_json::from_slice::<Unsaved>(&bytes).ok())
            .filter(|u| u.pdf == pdf);
        if let Some(unsaved) = unsaved {
            let current = Fingerprint::of(pdf);
            this.added = unsaved.added;
            if unsaved.fingerprint == current {
                this.edited = unsaved.pages;
            } else {
                // The PDF changed since (saved from Preview, say), so its boxes may no longer
                // be the ones the edits changed: only the boxes added here are kept, on top
                // of the file's own.
                for (page, boxes) in unsaved.pages {
                    let new = boxes.into_iter().filter(|b| b.origin.is_none());
                    this.added.entry(page).or_default().extend(new);
                }
                this.added.retain(|_, boxes| !boxes.is_empty());
            }
            this.fingerprint = current;
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

    /// The file's boxes of `page`, read with its links.
    pub fn set_file(&mut self, page: u32, boxes: Vec<TextBox>) {
        if let Some(added) = self.added.remove(&page) {
            let mut all = boxes.clone();
            all.extend(added);
            self.edited.entry(page).or_insert(all);
            self.persist();
        }
        self.file.insert(page, boxes);
    }

    /// Lets go of the file's boxes, read again when their pages are next shown.
    pub fn clear_file(&mut self) {
        self.file.clear();
    }

    /// Edits not yet written into the PDF.
    pub fn unsaved(&self) -> bool {
        !self.edited.is_empty() || !self.added.is_empty()
    }

    /// A box's text is being typed, so the keyboard is the text field's.
    pub fn typing(&self) -> bool {
        self.editing.is_some()
    }

    /// Changes `page`'s boxes, which can be undone, and keeps the change on disk.
    pub fn change<R>(&mut self, page: u32, f: impl FnOnce(&mut Vec<TextBox>) -> R) -> R {
        if self.edited.is_empty() && self.added.is_empty() {
            self.fingerprint = Fingerprint::of(&self.pdf);
        }
        let before = self.edited.get(&page).cloned();
        let file = self.file.get(&page).cloned().unwrap_or_default();
        let boxes = self.edited.entry(page).or_insert(file);
        let result = f(boxes);
        self.undo.push(Undo { page, before });
        self.persist();
        result
    }

    /// Changes one box, marking it changed.
    pub fn change_box(&mut self, r: BoxRef, f: impl FnOnce(&mut TextBox)) {
        self.change(r.page, |boxes| {
            if let Some(b) = boxes.get_mut(r.index) {
                f(b);
                b.changed = true;
            }
        });
    }

    pub fn delete(&mut self, r: BoxRef) {
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

    /// Starts typing into a new box at `at` (see [`TextBox::at`]) on `page`.
    pub fn add(&mut self, page: u32, at: Pos2) {
        self.finish_editing();
        self.selected = None;
        self.editing = Some(Editing {
            page,
            index: None,
            at,
            draft: String::new(),
            size: DEFAULT_SIZE,
            color: COLORS[0].1,
            focus: true,
        });
    }

    /// Starts typing into an existing box.
    pub fn edit(&mut self, r: BoxRef) {
        self.finish_editing();
        let Some(b) = self.get(r) else { return };
        self.editing = Some(Editing {
            page: r.page,
            index: Some(r.index),
            at: b.at(),
            draft: b.text.clone(),
            size: b.size,
            color: b.color,
            focus: true,
        });
        self.selected = Some(r);
    }

    /// Ends typing: keeps the text, size and colour (one change to undo), adding a new box
    /// if it has text, and deletes a box emptied of it.
    pub fn finish_editing(&mut self) {
        let Some(editing) = self.editing.take() else { return };
        let empty = editing.draft.trim().is_empty();
        match editing.index {
            None if empty => {}
            None => {
                let index = self.change(editing.page, |boxes| {
                    let b =
                        TextBox { size: editing.size, color: editing.color, ..TextBox::new(editing.at, editing.draft) };
                    boxes.push(b);
                    boxes.len() - 1
                });
                self.selected = Some(BoxRef { page: editing.page, index });
            }
            Some(index) => {
                let r = BoxRef { page: editing.page, index };
                let unchanged =
                    |b: &TextBox| b.text == editing.draft && b.size == editing.size && b.color == editing.color;
                if empty {
                    self.delete(r);
                } else if self.get(r).is_some_and(|b| !unchanged(b)) {
                    self.change_box(r, |b| {
                        b.text = editing.draft;
                        b.size = editing.size;
                        b.color = editing.color;
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
        let Some(Undo { page, before }) = self.undo.pop() else { return false };
        match before {
            Some(boxes) => self.edited.insert(page, boxes),
            None => self.edited.remove(&page),
        };
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
        let with_rects = |page: u32, boxes: &[TextBox]| boxes.iter().map(|b| (b.clone(), rect_of(page, b))).collect();
        let edited =
            self.edited.iter().map(|(&page, boxes)| PageSave { page, boxes: with_rects(page, boxes), complete: true });
        let added =
            self.added.iter().map(|(&page, boxes)| PageSave { page, boxes: with_rects(page, boxes), complete: false });
        Ok(edited.chain(added).collect())
    }

    /// The PDF now has every edit: start again from what it has.
    pub fn saved(&mut self) {
        self.file.clear();
        self.edited.clear();
        self.added.clear();
        self.undo.clear();
        self.selected = None;
        self.fingerprint = None;
        self.persist();
    }

    /// Writes the unsaved edits to the store, or removes it once there are none.
    fn persist(&self) {
        let Some(store) = &self.store else { return };
        if !self.unsaved() {
            let _ = std::fs::remove_file(store);
            return;
        }
        let unsaved = Unsaved {
            pdf: self.pdf.clone(),
            fingerprint: self.fingerprint,
            pages: self.edited.clone(),
            added: self.added.clone(),
        };
        if let Ok(json) = serde_json::to_vec(&unsaved) {
            crate::config::write_atomically(store, &json);
        }
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

/// FNV-1a: names the file of unsaved edits, the same across launches and Rust versions.
fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| (h ^ b as u64).wrapping_mul(0x0100_0000_01b3))
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
        boxes.add(page, pos2(0.25, 0.5));
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
    fn a_box_typed_into_is_added_and_an_empty_one_is_not() {
        let dir = temp_dir("add");
        let mut boxes = TextBoxes::load(&pdf(&dir), Some(&dir));
        boxes.set_file(0, Vec::new());
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
        boxes.set_file(0, vec![from_file.clone()]);
        boxes.change_box(BoxRef { page: 0, index: 0 }, |b| b.text = "Edited".into());
        type_box(&mut boxes, 0, "Mine");

        std::fs::write(&pdf, b"%PDF-1.4 changed elsewhere").unwrap();
        let mut reopened = TextBoxes::load(&pdf, Some(&dir));
        assert!(reopened.page(0).is_none(), "waits for the file's own boxes");
        let theirs = TextBox { text: "Theirs".into(), ..from_file };
        reopened.set_file(0, vec![theirs]);
        let texts: Vec<&str> = reopened.page(0).unwrap().iter().map(|b| b.text.as_str()).collect();
        assert_eq!(texts, ["Theirs", "Mine"]);
    }

    #[test]
    fn undo_restores_the_page() {
        let dir = temp_dir("undo");
        let mut boxes = TextBoxes::load(&pdf(&dir), Some(&dir));
        boxes.set_file(0, Vec::new());
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
