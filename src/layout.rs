//! Page geometry for a continuously scrolling document, laid out in rows of one page, or two
//! side by side like an open book.
//!
//! All positions are in egui points, relative to the top-left of the scroll content.

use std::ops::Range;

use eframe::egui::{Pos2, Rect, Vec2, pos2, vec2};
use serde::{Deserialize, Serialize};

pub const MARGIN: f32 = 16.0;
pub const GAP: f32 = 12.0;
/// Space between the two pages of a spread.
pub const SPINE: f32 = 4.0;

/// How pages are arranged into rows.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum PageLayout {
    /// One page per row.
    #[default]
    Single,
    /// Two pages per row: 1–2, 3–4, …
    TwoPages,
    /// Two pages per row with the cover on its own, like a printed book: 1, 2–3, 4–5, …
    Book,
}

impl PageLayout {
    fn columns(self) -> usize {
        match self {
            PageLayout::Single => 1,
            PageLayout::TwoPages | PageLayout::Book => 2,
        }
    }

    fn row_of(self, page: usize) -> usize {
        match self {
            PageLayout::Single => page,
            PageLayout::TwoPages => page / 2,
            PageLayout::Book => page.div_ceil(2),
        }
    }

    fn row_start(self, row: usize) -> usize {
        match self {
            PageLayout::Single => row,
            PageLayout::TwoPages => 2 * row,
            PageLayout::Book => (2 * row).saturating_sub(1),
        }
    }

    /// Whether `page` sits right of the spine.
    fn is_right(self, page: usize) -> bool {
        match self {
            PageLayout::Single => false,
            PageLayout::TwoPages => page % 2 == 1,
            PageLayout::Book => page.is_multiple_of(2),
        }
    }
}

pub struct DocLayout {
    /// Page sizes in PDF points.
    sizes: Vec<Vec2>,
    mode: PageLayout,
    /// `tops[r]` is the summed height (PDF points) of all rows before row `r`.
    tops: Vec<f32>,
    max_width: f32,
}

impl DocLayout {
    pub fn new(sizes: Vec<Vec2>, mode: PageLayout) -> Self {
        let max_width = sizes.iter().map(|s| s.x).fold(1.0, f32::max);
        let mut layout = Self { sizes, mode, tops: Vec::new(), max_width };
        layout.set_mode(mode);
        layout
    }

    pub fn mode(&self) -> PageLayout {
        self.mode
    }

    pub fn set_mode(&mut self, mode: PageLayout) {
        self.mode = mode;
        let rows = self.row_count();
        self.tops.clear();
        self.tops.reserve(rows + 1);
        let mut acc = 0.0;
        self.tops.push(acc);
        for row in 0..rows {
            acc += self.row_pages(row).map(|p| self.sizes[p].y).fold(0.0, f32::max);
            self.tops.push(acc);
        }
    }

    pub fn page_count(&self) -> usize {
        self.sizes.len()
    }

    pub fn page_size(&self, page: usize) -> Vec2 {
        self.sizes[page]
    }

    fn row_count(&self) -> usize {
        match self.sizes.len() {
            0 => 0,
            n => self.mode.row_of(n - 1) + 1,
        }
    }

    fn row_pages(&self, row: usize) -> Range<usize> {
        let n = self.sizes.len();
        self.mode.row_start(row).min(n)..self.mode.row_start(row + 1).min(n)
    }

    /// The pages shown in the same row as `page`.
    pub fn spread(&self, page: usize) -> Range<usize> {
        self.row_pages(self.mode.row_of(page))
    }

    /// Width in PDF points of the widest row, if every page were as wide as the widest one.
    /// Spreads are laid out around a centred spine, so this is what must fit on screen.
    fn span(&self) -> f32 {
        self.mode.columns() as f32 * self.max_width
    }

    /// Screen space between pages in a row, which doesn't scale with the zoom.
    fn spine(&self) -> f32 {
        (self.mode.columns() - 1) as f32 * SPINE
    }

    /// The zoom at which the widest row fills `width`, margins included.
    pub fn fit_width(&self, width: f32) -> f32 {
        (width - 2.0 * MARGIN - self.spine()) / self.span()
    }

    pub fn content_size(&self, zoom: f32, viewport_width: f32) -> Vec2 {
        let rows = self.row_count();
        let width = (self.span() * zoom + self.spine() + 2.0 * MARGIN).max(viewport_width);
        let height = 2.0 * MARGIN + self.tops[rows] * zoom + rows.saturating_sub(1) as f32 * GAP;
        vec2(width, height)
    }

    fn row_top(&self, row: usize, zoom: f32) -> f32 {
        MARGIN + self.tops[row] * zoom + row as f32 * GAP
    }

    /// Single pages are centred within `content_width`; spreads meet at its centre. Pages
    /// shorter than their row are centred vertically.
    pub fn page_rect(&self, page: usize, zoom: f32, content_width: f32) -> Rect {
        let row = self.mode.row_of(page);
        let size = self.sizes[page] * zoom;
        let row_height = (self.tops[row + 1] - self.tops[row]) * zoom;
        let y = self.row_top(row, zoom) + (row_height - size.y) / 2.0;
        let x = match self.mode {
            PageLayout::Single => ((content_width - size.x) / 2.0).max(MARGIN),
            _ if self.mode.is_right(page) => content_width / 2.0 + SPINE / 2.0,
            _ => content_width / 2.0 - SPINE / 2.0 - size.x,
        };
        Rect::from_min_size(pos2(x, y), size)
    }

    /// The last row whose top edge is at or above `y`.
    fn row_at(&self, y: f32, zoom: f32) -> usize {
        let (mut lo, mut hi) = (0, self.row_count());
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.row_top(mid, zoom) <= y {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo.saturating_sub(1)
    }

    /// The first page of the last row whose top edge is at or above `y`.
    pub fn page_at(&self, y: f32, zoom: f32) -> usize {
        self.mode.row_start(self.row_at(y, zoom)).min(self.sizes.len().saturating_sub(1))
    }

    /// The page of the row at `point.y` that is horizontally closest to `point`.
    pub fn page_near(&self, point: Pos2, zoom: f32, content_width: f32) -> usize {
        let pages = self.row_pages(self.row_at(point.y, zoom));
        let distance = |page: &usize| {
            let rect = self.page_rect(*page, zoom, content_width);
            (rect.left() - point.x).max(point.x - rect.right()).max(0.0)
        };
        pages.clone().min_by(|a, b| distance(a).total_cmp(&distance(b))).unwrap_or(pages.start)
    }

    pub fn pages_in(&self, top: f32, bottom: f32, zoom: f32) -> Range<usize> {
        if self.sizes.is_empty() {
            return 0..0;
        }
        self.row_pages(self.row_at(top, zoom)).start..self.row_pages(self.row_at(bottom, zoom)).end
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LETTER: Vec2 = vec2(612.0, 792.0);

    fn spreads(mode: PageLayout, pages: usize) -> Vec<Range<usize>> {
        let layout = DocLayout::new(vec![LETTER; pages], mode);
        (0..layout.row_count()).map(|r| layout.row_pages(r)).collect()
    }

    #[test]
    fn pairs_pages_into_rows() {
        assert_eq!(spreads(PageLayout::Single, 3), [0..1, 1..2, 2..3]);
        assert_eq!(spreads(PageLayout::TwoPages, 5), [0..2, 2..4, 4..5]);
        assert_eq!(spreads(PageLayout::Book, 5), [0..1, 1..3, 3..5]);
        assert_eq!(spreads(PageLayout::Book, 4), [0..1, 1..3, 3..4]);
        assert_eq!(spreads(PageLayout::Book, 1), vec![0..1]);
        assert!(spreads(PageLayout::TwoPages, 0).is_empty());
    }

    #[test]
    fn spreads_meet_at_the_spine() {
        let layout = DocLayout::new(vec![LETTER; 4], PageLayout::Book);
        let (zoom, width) = (1.0, 2000.0);
        let cover = layout.page_rect(0, zoom, width);
        let left = layout.page_rect(1, zoom, width);
        let right = layout.page_rect(2, zoom, width);
        assert_eq!(cover.left(), width / 2.0 + SPINE / 2.0, "the cover sits right of the spine");
        assert_eq!(left.right() + SPINE, right.left());
        assert_eq!(left.top(), right.top());
        assert_eq!(left.top(), cover.bottom() + GAP);
    }

    #[test]
    fn centres_short_pages_in_their_row() {
        let layout = DocLayout::new(vec![LETTER, vec2(612.0, 392.0)], PageLayout::TwoPages);
        let tall = layout.page_rect(0, 1.0, 2000.0);
        let short = layout.page_rect(1, 1.0, 2000.0);
        assert_eq!(short.center().y, tall.center().y);
        assert_eq!(layout.content_size(1.0, 0.0).y, 2.0 * MARGIN + 792.0);
    }

    #[test]
    fn fit_width_fits_the_widest_spread() {
        let layout = DocLayout::new(vec![LETTER; 3], PageLayout::TwoPages);
        let zoom = layout.fit_width(1000.0);
        assert!((layout.content_size(zoom, 0.0).x - 1000.0).abs() < 0.01);
    }

    #[test]
    fn finds_pages_by_position() {
        let layout = DocLayout::new(vec![LETTER; 5], PageLayout::Book);
        let width = layout.content_size(1.0, 0.0).x;
        let second_row = layout.page_rect(1, 1.0, width);
        assert_eq!(layout.page_at(second_row.center().y, 1.0), 1);
        assert_eq!(layout.pages_in(second_row.top(), second_row.bottom(), 1.0), 1..3);
        let right = layout.page_rect(2, 1.0, width);
        assert_eq!(layout.page_near(right.center(), 1.0, width), 2);
        assert_eq!(layout.page_near(pos2(0.0, second_row.center().y), 1.0, width), 1);
        assert_eq!(layout.spread(4), 3..5);
    }
}
