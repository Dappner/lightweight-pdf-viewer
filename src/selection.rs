//! Selecting text with the pointer: a page's text layer (its text with a rectangle per
//! character), finding where on it a point falls, and selections that can span pages.
//!
//! Everything here is plain geometry on [`TextLayer`]s, which [`crate::text_layers`] reads
//! from PDFKit on a worker thread: the UI thread hit-tests every pointer move without
//! touching PDFKit. Text is indexed in UTF-16 units, as PDFKit's page string is, but carets
//! and words only ever fall between whole characters: never inside a surrogate pair.

use std::ops::Range;

use eframe::egui::{Pos2, Rect};

/// A page's text and where each of its UTF-16 units is, for hit-testing and highlighting.
pub struct TextLayer {
    units: Vec<u16>,
    /// One per unit, as fractions of the displayed page with a top-left origin;
    /// [`Rect::NOTHING`] for units PDFKit has no box for (the spaces and line breaks it
    /// inserts itself).
    rects: Vec<Rect>,
    lines: Vec<Line>,
}

/// A run of units laid out along one line, in text order.
#[derive(Debug)]
struct Line {
    range: Range<u32>,
    /// Union of its units' rectangles.
    rect: Rect,
}

/// A position between two units of a page's text: before unit `index`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Caret {
    pub page: u32,
    pub index: u32,
}

impl Caret {
    /// The end of `page`'s text, whatever its length.
    pub fn page_end(page: u32) -> Self {
        Self { page, index: u32::MAX }
    }
}

/// Selected text from `anchor`, where the selection began, to `focus`, which follows the
/// pointer; either can come first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub anchor: Caret,
    pub focus: Caret,
}

/// What a press selects by: its click count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit {
    Char,
    Word,
    Line,
}

impl Selection {
    pub fn start(&self) -> Caret {
        self.anchor.min(self.focus)
    }

    pub fn end(&self) -> Caret {
        self.anchor.max(self.focus)
    }

    pub fn is_empty(&self) -> bool {
        self.anchor == self.focus
    }

    /// Pages with selected text on them.
    pub fn pages(&self) -> Range<u32> {
        self.start().page..self.end().page + 1
    }

    /// The selected units of `page`, whose text is `len` units long.
    pub fn range_on(&self, page: u32, len: u32) -> Option<Range<u32>> {
        let (start, end) = (self.start(), self.end());
        if page < start.page || page > end.page {
            return None;
        }
        let from = if page == start.page { start.index.min(len) } else { 0 };
        let to = if page == end.page { end.index.min(len) } else { len };
        (from < to).then_some(from..to)
    }
}

impl TextLayer {
    /// `units` is the page's text; `rects` holds one rectangle per unit, as for
    /// [`TextLayer::rects`].
    pub fn new(units: Vec<u16>, rects: Vec<Rect>) -> Self {
        assert_eq!(units.len(), rects.len());
        let lines = lines(&units, &rects);
        Self { units, rects, lines }
    }

    pub fn empty() -> Self {
        Self::new(Vec::new(), Vec::new())
    }

    pub fn len(&self) -> u32 {
        self.units.len() as u32
    }

    /// Bytes held.
    pub fn bytes(&self) -> usize {
        size_of::<Self>()
            + self.units.capacity() * size_of::<u16>()
            + self.rects.capacity() * size_of::<Rect>()
            + self.lines.capacity() * size_of::<Line>()
    }

    /// Whether `p` (fractions of the page) is over a line of text: where the pointer shows
    /// an I-beam.
    pub fn is_over_text(&self, p: Pos2) -> bool {
        self.lines.iter().any(|l| l.rect.contains(p))
    }

    /// The text of `range`.
    pub fn text(&self, range: Range<u32>) -> String {
        String::from_utf16_lossy(&self.units[range.start as usize..range.end as usize])
    }

    /// The units around point `p` that a press selects by `unit`: an empty range at the
    /// nearest caret for [`Unit::Char`], else the word or line nearest `p`.
    pub fn range_at(&self, p: Pos2, unit: Unit) -> Range<u32> {
        let Some(line) = self.line_near(p) else { return 0..0 };
        match unit {
            Unit::Char => {
                let caret = self.caret_in(line, p.x);
                caret..caret
            }
            Unit::Word => self.word_at(self.unit_in(line, p.x)),
            Unit::Line => line.range.clone(),
        }
    }

    /// Highlight rectangles for `range`: on each line, one around its units in the range.
    pub fn rects(&self, range: Range<u32>) -> impl Iterator<Item = Rect> + '_ {
        self.lines.iter().filter_map(move |line| {
            let from = line.range.start.max(range.start);
            let to = line.range.end.min(range.end);
            let rect = (from..to).fold(Rect::NOTHING, |r, i| r.union(self.rects[i as usize]));
            // Full line height, so a line's highlight is even whatever its letters are.
            rect.is_positive().then(|| Rect::from_x_y_ranges(rect.x_range(), line.rect.y_range()))
        })
    }

    /// The line `p` is level with, nearest it across; otherwise the nearest line above or
    /// below.
    fn line_near(&self, p: Pos2) -> Option<&Line> {
        let level = self
            .lines
            .iter()
            .filter(|l| l.rect.y_range().contains(p.y))
            .min_by(|a, b| distance(a.rect.x_range(), p.x).total_cmp(&distance(b.rect.x_range(), p.x)));
        level.or_else(|| {
            self.lines.iter().min_by(|a, b| {
                let key = |l: &Line| (distance(l.rect.y_range(), p.y), distance(l.rect.x_range(), p.x));
                let (ka, kb) = (key(a), key(b));
                ka.0.total_cmp(&kb.0).then(ka.1.total_cmp(&kb.1))
            })
        })
    }

    /// Characters of `line` that have a box, by their first unit, in order.
    fn boxed(&self, line: &Line) -> impl Iterator<Item = u32> + '_ {
        line.range.clone().filter(|&i| self.rects[i as usize].is_positive() && !self.continues(i))
    }

    /// The caret on `line` nearest `x`: before the first character whose middle is right of
    /// it, or after the line's last character.
    fn caret_in(&self, line: &Line, x: f32) -> u32 {
        let mut last = None;
        for i in self.boxed(line) {
            if self.rects[i as usize].center().x > x {
                return i;
            }
            last = Some(i);
        }
        last.map_or(line.range.start, |i| i + self.char_len(i))
    }

    /// Whether unit `i` is the second half of a surrogate pair.
    fn continues(&self, i: u32) -> bool {
        let i = i as usize;
        i > 0 && i < self.units.len() && is_low(self.units[i]) && is_high(self.units[i - 1])
    }

    /// Units in the character starting at unit `i`.
    fn char_len(&self, i: u32) -> u32 {
        let i = i as usize;
        if i + 1 < self.units.len() && is_high(self.units[i]) && is_low(self.units[i + 1]) { 2 } else { 1 }
    }

    /// The class of the character starting at unit `i`.
    fn class_at(&self, i: u32) -> Class {
        let units = &self.units[i as usize..(i + self.char_len(i)) as usize];
        // A lone surrogate is no letter.
        char::decode_utf16(units.iter().copied()).next().and_then(Result::ok).map_or(Class::Other, class)
    }

    /// The character on `line` nearest `x`, by its first unit.
    fn unit_in(&self, line: &Line, x: f32) -> u32 {
        self.boxed(line)
            .min_by(|&a, &b| {
                let d = |i: u32| distance(self.rects[i as usize].x_range(), x);
                d(a).total_cmp(&d(b))
            })
            .unwrap_or(line.range.start)
    }

    /// The word around the character starting at unit `i`: a run of letters and digits, a
    /// run of whitespace, or a single other character.
    fn word_at(&self, i: u32) -> Range<u32> {
        if i >= self.len() {
            return i..i;
        }
        let c = self.class_at(i);
        let mut end = i + self.char_len(i);
        if c == Class::Other {
            return i..end;
        }
        while end < self.len() && self.class_at(end) == c {
            end += self.char_len(end);
        }
        let mut start = i;
        while start > 0 {
            let before = if self.continues(start - 1) { start - 2 } else { start - 1 };
            if self.class_at(before) != c {
                break;
            }
            start = before;
        }
        start..end
    }
}

/// Splits the text into lines: at each line break, and wherever a unit's box no longer
/// overlaps the line vertically or jumps back left of it (a new line or column).
fn lines(units: &[u16], rects: &[Rect]) -> Vec<Line> {
    let mut lines: Vec<Line> = Vec::new();
    let mut current = Line { range: 0..0, rect: Rect::NOTHING };
    let mut last_box: Option<Rect> = None;
    for (i, (&unit, &rect)) in units.iter().zip(rects).enumerate() {
        // The second half of a surrogate pair stays with the first.
        let continues = i > 0 && is_low(unit) && is_high(units[i - 1]);
        let i = i as u32;
        let starts_line = rect.is_positive()
            && !continues
            && last_box.is_some_and(|last| {
                let overlap = rect.y_range().intersection(current.rect.y_range()).span();
                overlap <= 0.0 || rect.left() < last.left() - last.height()
            });
        if starts_line {
            lines.push(std::mem::replace(&mut current, Line { range: i..i, rect: Rect::NOTHING }));
        }
        current.range.end = i + 1;
        if rect.is_positive() {
            current.rect = current.rect.union(rect);
            last_box = Some(rect);
        }
        if is_line_break(unit) {
            lines.push(std::mem::replace(&mut current, Line { range: i + 1..i + 1, rect: Rect::NOTHING }));
            last_box = None;
        }
    }
    lines.push(current);
    lines.retain(|l| l.rect.is_positive());
    lines
}

/// How far `x` is outside `range`; zero inside it.
fn distance(range: eframe::egui::Rangef, x: f32) -> f32 {
    (range.min - x).max(x - range.max).max(0.0)
}

fn is_line_break(unit: u16) -> bool {
    matches!(unit, 0x0A | 0x0D | 0x2028 | 0x2029)
}

fn is_high(unit: u16) -> bool {
    (0xD800..0xDC00).contains(&unit)
}

fn is_low(unit: u16) -> bool {
    (0xDC00..0xE000).contains(&unit)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Word,
    Space,
    Other,
}

fn class(c: char) -> Class {
    if c.is_alphanumeric() || c == '_' {
        Class::Word
    } else if c.is_whitespace() {
        Class::Space
    } else {
        Class::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::pos2;

    /// Lays out `lines` of text in a grid: each character 0.01 wide and each line 0.02 high,
    /// with a line break (no box) between lines and spaces boxed like letters.
    fn layer(lines: &[&str]) -> TextLayer {
        let mut units = Vec::new();
        let mut rects = Vec::new();
        for (row, line) in lines.iter().enumerate() {
            if row > 0 {
                units.push(b'\n' as u16);
                rects.push(Rect::NOTHING);
            }
            for (col, unit) in line.encode_utf16().enumerate() {
                let min = pos2(0.1 + col as f32 * 0.01, 0.1 + row as f32 * 0.03);
                units.push(unit);
                rects.push(Rect::from_min_size(min, eframe::egui::vec2(0.01, 0.02)));
            }
        }
        TextLayer::new(units, rects)
    }

    /// A point over column `col` (its left quarter) of line `row`, as laid out by [`layer`].
    fn at(row: usize, col: f32) -> Pos2 {
        pos2(0.1 + col * 0.01 + 0.0025, 0.11 + row as f32 * 0.03)
    }

    #[test]
    fn splits_lines_at_breaks_and_jumps() {
        let l = layer(&["one two", "three"]);
        assert_eq!(l.lines.iter().map(|l| l.range.clone()).collect::<Vec<_>>(), vec![0..8, 8..13]);

        // Two columns read one after the other, with no break between them.
        let mut units: Vec<u16> = "ab".encode_utf16().collect();
        let mut rects = vec![
            Rect::from_min_size(pos2(0.5, 0.1), eframe::egui::vec2(0.01, 0.02)),
            Rect::from_min_size(pos2(0.51, 0.1), eframe::egui::vec2(0.01, 0.02)),
        ];
        units.push(b'c' as u16);
        rects.push(Rect::from_min_size(pos2(0.1, 0.1), eframe::egui::vec2(0.01, 0.02)));
        assert_eq!(TextLayer::new(units, rects).lines.len(), 2);
    }

    #[test]
    fn a_superscript_stays_on_its_line() {
        let mut l = layer(&["x2"]);
        l.rects[1] = l.rects[1].translate(eframe::egui::vec2(0.0, -0.008));
        let l = TextLayer::new(l.units, l.rects);
        assert_eq!(l.lines.len(), 1);
    }

    #[test]
    fn carets_fall_between_the_nearest_characters() {
        let l = layer(&["hello", "world"]);
        assert_eq!(l.range_at(at(0, 0.0), Unit::Char), 0..0);
        assert_eq!(l.range_at(at(0, 2.9), Unit::Char), 3..3);
        // Right of a line: its end, before the break.
        assert_eq!(l.range_at(at(0, 40.0), Unit::Char), 5..5);
        assert_eq!(l.range_at(at(1, 1.0), Unit::Char), 7..7);
        // Above all text: the first line; below it: the last.
        assert_eq!(l.range_at(pos2(0.12, 0.0), Unit::Char), 2..2);
        assert_eq!(l.range_at(pos2(0.5, 0.9), Unit::Char), 11..11);
    }

    #[test]
    fn words_and_lines() {
        let l = layer(&["it's a dog-eat-dog world", "next"]);
        assert_eq!(l.text(l.range_at(at(0, 1.0), Unit::Word)), "it");
        assert_eq!(l.text(l.range_at(at(0, 2.0), Unit::Word)), "'");
        assert_eq!(l.text(l.range_at(at(0, 7.0), Unit::Word)), "dog");
        assert_eq!(l.text(l.range_at(at(0, 4.0), Unit::Word)), " ");
        assert_eq!(l.text(l.range_at(at(0, 3.0), Unit::Line)), "it's a dog-eat-dog world\n");
        assert_eq!(l.text(l.range_at(at(1, 9.0), Unit::Word)), "next");
    }

    /// "x😀𝑥𝑦 z": each character one column wide, both halves of a surrogate pair sharing
    /// their character's box, as PDFKit gives them.
    fn surrogates() -> TextLayer {
        let mut units = Vec::new();
        let mut rects = Vec::new();
        for (col, c) in "x😀𝑥𝑦 z".chars().enumerate() {
            let rect = Rect::from_min_size(pos2(0.1 + col as f32 * 0.01, 0.1), eframe::egui::vec2(0.01, 0.02));
            for unit in c.encode_utf16(&mut [0; 2]) {
                units.push(*unit);
                rects.push(rect);
            }
        }
        TextLayer::new(units, rects)
    }

    #[test]
    fn carets_and_words_keep_surrogate_pairs_whole() {
        let l = surrogates();
        assert_eq!(l.lines.len(), 1);
        // Right of the emoji's middle: after the whole pair, not between its halves.
        assert_eq!(l.range_at(at(0, 1.6), Unit::Char), 3..3);
        assert_eq!(l.range_at(at(0, 40.0), Unit::Char), 9..9);
        assert_eq!(l.text(l.range_at(at(0, 1.0), Unit::Word)), "😀");
        // Mathematical letters outside the Basic Multilingual Plane make up a word.
        assert_eq!(l.text(l.range_at(at(0, 3.0), Unit::Word)), "𝑥𝑦");
        assert_eq!(l.text(l.range_at(at(0, 2.0), Unit::Word)), "𝑥𝑦");
    }

    #[test]
    fn highlights_one_rect_per_line() {
        let l = layer(&["hello", "world"]);
        let rects: Vec<Rect> = l.rects(3..8).collect();
        assert_eq!(rects.len(), 2);
        assert!((rects[0].left() - 0.13).abs() < 1e-6 && (rects[0].right() - 0.15).abs() < 1e-6);
        assert!((rects[1].left() - 0.10).abs() < 1e-6 && (rects[1].right() - 0.12).abs() < 1e-6);
        assert_eq!(l.rects(5..6).count(), 0, "the line break alone has no box");
    }

    #[test]
    fn pointer_over_text() {
        let l = layer(&["hello"]);
        assert!(l.is_over_text(at(0, 2.0)));
        assert!(!l.is_over_text(pos2(0.5, 0.5)));
        assert!(!TextLayer::empty().is_over_text(pos2(0.5, 0.5)));
    }

    #[test]
    fn selections_across_pages() {
        let s = Selection { anchor: Caret { page: 3, index: 4 }, focus: Caret { page: 1, index: 10 } };
        assert_eq!(s.pages(), 1..4);
        assert_eq!(s.range_on(0, 100), None);
        assert_eq!(s.range_on(1, 100), Some(10..100));
        assert_eq!(s.range_on(2, 50), Some(0..50));
        assert_eq!(s.range_on(3, 100), Some(0..4));
        assert_eq!(s.range_on(1, 5), None, "the start is past the page's end");

        let all = Selection { anchor: Caret { page: 0, index: 0 }, focus: Caret::page_end(2) };
        assert_eq!(all.range_on(2, 30), Some(0..30));
    }
}
