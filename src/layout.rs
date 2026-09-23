//! Page geometry for a continuously scrolling, vertically stacked document.
//!
//! All positions are in egui points, relative to the top-left of the scroll content.

use std::ops::Range;

use eframe::egui::{Rect, Vec2, pos2, vec2};

pub const MARGIN: f32 = 16.0;
pub const GAP: f32 = 12.0;

pub struct DocLayout {
    /// Page sizes in PDF points.
    sizes: Vec<Vec2>,
    /// `tops[i]` is the summed height (PDF points) of all pages before `i`.
    tops: Vec<f32>,
    max_width: f32,
}

impl DocLayout {
    pub fn new(sizes: Vec<Vec2>) -> Self {
        let mut tops = Vec::with_capacity(sizes.len() + 1);
        let mut acc = 0.0;
        tops.push(acc);
        for s in &sizes {
            acc += s.y;
            tops.push(acc);
        }
        let max_width = sizes.iter().map(|s| s.x).fold(1.0, f32::max);
        Self { sizes, tops, max_width }
    }

    pub fn page_count(&self) -> usize {
        self.sizes.len()
    }

    pub fn page_size(&self, page: usize) -> Vec2 {
        self.sizes[page]
    }

    pub fn max_width(&self) -> f32 {
        self.max_width
    }

    pub fn content_size(&self, zoom: f32, viewport_width: f32) -> Vec2 {
        let n = self.sizes.len();
        let width = (self.max_width * zoom + 2.0 * MARGIN).max(viewport_width);
        let height = 2.0 * MARGIN + self.tops[n] * zoom + n.saturating_sub(1) as f32 * GAP;
        vec2(width, height)
    }

    fn page_top(&self, page: usize, zoom: f32) -> f32 {
        MARGIN + self.tops[page] * zoom + page as f32 * GAP
    }

    /// Pages are horizontally centered within `content_width`.
    pub fn page_rect(&self, page: usize, zoom: f32, content_width: f32) -> Rect {
        let size = self.sizes[page] * zoom;
        let x = ((content_width - size.x) / 2.0).max(MARGIN);
        Rect::from_min_size(pos2(x, self.page_top(page, zoom)), size)
    }

    /// The last page whose top edge is at or above `y`.
    pub fn page_at(&self, y: f32, zoom: f32) -> usize {
        let (mut lo, mut hi) = (0, self.sizes.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.page_top(mid, zoom) <= y {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo.saturating_sub(1)
    }

    pub fn pages_in(&self, top: f32, bottom: f32, zoom: f32) -> Range<usize> {
        if self.sizes.is_empty() {
            return 0..0;
        }
        self.page_at(top, zoom)..self.page_at(bottom, zoom) + 1
    }
}
