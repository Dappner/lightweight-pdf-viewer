//! A stand-in PDF engine for platforms without CoreGraphics, so the rest of the app (layout,
//! scheduling, caching, snapshots) builds, runs its tests and can be benchmarked anywhere.
//!
//! Any existing file opens as a document of US Letter pages: as many as its page tree's
//! `/Count` says (the largest one, the root's) if it can be read uncompressed, else the number
//! ending its name (`book-800.pdf` has 800), else 300. Pages draw as grey text-like lines on white,
//! which costs time in proportion to the pixels drawn, as rasterizing does. There are no
//! links, outline or text: a document never opens for search, which then finds nothing.

use std::convert::Infallible;
use std::marker::PhantomData;
use std::path::Path;

use eframe::egui::{Rect, Vec2, vec2};

use crate::layout::{Rotation, Rotations};
use crate::renderer::{Link, OutlineEntry};
use crate::text_boxes::{Highlight, PageSave, TextBox};

const LETTER: Vec2 = vec2(612.0, 792.0);

pub fn autoreleasepool<R>(f: impl FnOnce() -> R) -> R {
    f()
}

pub struct Document {
    pages: u32,
    rotations: Rotations,
}

pub struct Page<'a> {
    rotation: Rotation,
    _doc: PhantomData<&'a Document>,
}

impl Document {
    pub fn open(path: &Path, rotations: &Rotations) -> Option<Self> {
        let bytes = std::fs::read(path).ok()?;
        let count = bytes
            .windows(7)
            .enumerate()
            .filter(|(_, w)| *w == b"/Count ")
            .filter_map(|(i, _)| leading_number(&bytes[i + 7..]))
            .max();
        let stem = path.file_stem()?.to_string_lossy();
        let digits = stem.len() - stem.trim_end_matches(|c: char| c.is_ascii_digit()).len();
        let named = stem[stem.len() - digits..].parse().ok();
        let pages = count.or(named).filter(|n| *n > 0).unwrap_or(300);
        Some(Self { pages, rotations: rotations.clone() })
    }

    pub fn page_count(&self) -> u32 {
        self.pages
    }

    pub fn page(&self, index: u32) -> Option<Page<'_>> {
        (index < self.pages).then_some(Page { rotation: self.rotations.of(index), _doc: PhantomData })
    }

    pub fn outline(&self) -> Vec<OutlineEntry> {
        Vec::new()
    }
}

fn leading_number(bytes: &[u8]) -> Option<u32> {
    let digits = bytes.iter().take_while(|b| b.is_ascii_digit()).count();
    std::str::from_utf8(&bytes[..digits]).ok()?.parse().ok()
}

impl Page<'_> {
    pub fn size(&self) -> Vec2 {
        if self.rotation.degrees() % 180 == 90 { vec2(LETTER.y, LETTER.x) } else { LETTER }
    }

    pub fn links(&self) -> Vec<Link> {
        Vec::new()
    }

    pub fn text_boxes(&self) -> Vec<TextBox> {
        Vec::new()
    }

    pub fn highlights(&self) -> Vec<Highlight> {
        Vec::new()
    }
}

pub fn save_annotations(_path: &Path, _pages: &[PageSave]) -> Result<super::SaveOutcome, String> {
    Err("Saving annotations needs macOS".into())
}

pub struct TextDocument {
    _none: Infallible,
}

pub struct TextPage {
    _none: Infallible,
}

impl TextDocument {
    pub fn open(_path: &Path, _rotations: &Rotations) -> Option<Self> {
        None
    }

    pub fn close(self) {
        match self._none {}
    }

    pub fn page(&self, _index: u32) -> Option<TextPage> {
        match self._none {}
    }
}

impl TextPage {
    pub fn rotation(&self) -> Rotation {
        match self._none {}
    }

    pub fn text(&self) -> Option<String> {
        match self._none {}
    }

    pub fn boxes(&self) -> (Vec<u16>, Vec<Rect>) {
        match self._none {}
    }

    pub fn char_top(&self, _index: u32) -> Option<f32> {
        match self._none {}
    }

    pub fn line_rects(&self, _start: u32, _len: u32) -> Vec<Rect> {
        match self._none {}
    }
}

pub struct Bitmap<'a> {
    rgba: std::cell::RefCell<&'a mut [u8]>,
    width: u32,
    height: u32,
}

impl<'a> Bitmap<'a> {
    pub fn new(rgba: &'a mut [u8], width: u32, height: u32) -> Option<Self> {
        assert_eq!(rgba.len(), width as usize * height as usize * 4);
        Some(Self { rgba: std::cell::RefCell::new(rgba), width, height })
    }

    pub fn draw(&self, _page: &Page, page_px: [f64; 2], top_left: [f64; 2]) {
        let mut rgba = self.rgba.borrow_mut();
        let (w, h) = (self.width as i64, self.height as i64);
        let x0 = (top_left[0].round() as i64).clamp(0, w);
        let y0 = (top_left[1].round() as i64).clamp(0, h);
        let x1 = ((top_left[0] + page_px[0]).round() as i64).clamp(0, w);
        let y1 = ((top_left[1] + page_px[1]).round() as i64).clamp(0, h);
        // Lines of "text" every 1/60th of the page, inside a 1/10th margin.
        let line = (page_px[1] / 60.0).max(2.0);
        for y in y0..y1 {
            let py = y as f64 - top_left[1];
            let inked = py > page_px[1] * 0.1 && py < page_px[1] * 0.9 && (py / line).fract() < 0.4;
            let row = &mut rgba[(y * w * 4) as usize..((y + 1) * w * 4) as usize];
            for x in x0..x1 {
                let px = x as f64 - top_left[0];
                let ink = inked && px > page_px[0] * 0.1 && px < page_px[0] * 0.9 && (x / 3) % 4 != 0;
                let v = if ink { 40 } else { 255 };
                row[(x * 4) as usize..(x * 4 + 4) as usize].copy_from_slice(&[v, v, v, 255]);
            }
        }
    }
}
