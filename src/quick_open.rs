//! ⌘K quick open: type to filter recently opened files, ranked by [`History`](crate::history).
//!
//! Every space-separated word of the query must match the file. Matches are grouped by how
//! well their worst word matches (name prefix, then name, then path, then scattered letters
//! of the name); within a group, files keep their frecency order.

use std::path::{Path, PathBuf};

use std::sync::Arc;

use eframe::egui::{
    Align2, Color32, Context, CornerRadius, FontId, Frame, Galley, Id, Key, Margin, Modal, Modifiers, RichText,
    Sense, Stroke, TextEdit, Ui, pos2, text::LayoutJob, vec2,
};

use crate::theme::Palette;

const WIDTH: f32 = 560.0;
const TOP: f32 = 80.0;
const ROW_HEIGHT: f32 = 38.0;
const MAX_ROWS: usize = 12;

pub struct QuickOpen {
    query: String,
    selected: usize,
    /// Candidate files, highest-ranked first.
    files: Vec<PathBuf>,
    /// Shown with `~` in place of the home folder.
    home: Option<PathBuf>,
}

pub enum Outcome {
    Open(PathBuf),
    Dismiss,
}

impl QuickOpen {
    pub fn new(files: Vec<PathBuf>) -> Self {
        Self { query: String::new(), selected: 0, files, home: std::env::var_os("HOME").map(PathBuf::from) }
    }

    pub fn show(&mut self, ctx: &Context, palette: &Palette) -> Option<Outcome> {
        let width = WIDTH.min(ctx.content_rect().width() - 32.0);
        let area = Modal::default_area(Id::new("quick_open")).anchor(Align2::CENTER_TOP, vec2(0.0, TOP));
        let frame = Frame::new()
            .fill(palette.content)
            .stroke(Stroke::new(1.0, palette.separator))
            .corner_radius(CornerRadius::same(10))
            .inner_margin(Margin::same(8));
        let modal = Modal::new(Id::new("quick_open"))
            .area(area)
            .frame(frame)
            .backdrop_color(Color32::from_black_alpha(60))
            .show(ctx, |ui| self.contents(ui, width, palette));
        if modal.should_close() {
            return Some(Outcome::Dismiss);
        }
        modal.inner.map(Outcome::Open)
    }

    fn contents(&mut self, ui: &mut Ui, width: f32, palette: &Palette) -> Option<PathBuf> {
        ui.set_width(width);
        let (down, up, enter) = ui.input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, Key::ArrowDown) || i.consume_key(Modifiers::CTRL, Key::N),
                i.consume_key(Modifiers::NONE, Key::ArrowUp) || i.consume_key(Modifiers::CTRL, Key::P),
                i.consume_key(Modifiers::NONE, Key::Enter),
            )
        });

        let field = ui.add(
            TextEdit::singleline(&mut self.query)
                .hint_text("Open recent file…")
                .font(FontId::proportional(15.0))
                .text_color(palette.text)
                .frame(false)
                .margin(Margin::symmetric(6, 6))
                .desired_width(f32::INFINITY),
        );
        field.request_focus();
        if field.changed() {
            self.selected = 0;
        }

        let matches: Vec<PathBuf> = ranked_matches(&self.files, &self.query).take(MAX_ROWS).cloned().collect();
        if matches.is_empty() {
            let text = if self.files.is_empty() { "Files you open will appear here" } else { "No matching files" };
            ui.add_space(4.0);
            ui.label(RichText::new(text).color(palette.text_weak).size(12.5));
            ui.add_space(4.0);
            return None;
        }
        if down {
            self.selected = (self.selected + 1).min(matches.len() - 1);
        }
        if up {
            self.selected = self.selected.saturating_sub(1);
        }
        self.selected = self.selected.min(matches.len() - 1);

        ui.add_space(4.0);
        let pointer_moved = ui.input(|i| i.pointer.delta() != vec2(0.0, 0.0));
        let mut clicked = None;
        for (i, path) in matches.iter().enumerate() {
            let (rect, response) = ui.allocate_exact_size(vec2(width, ROW_HEIGHT), Sense::click());
            if response.hovered() && pointer_moved {
                self.selected = i;
            }
            if response.clicked() {
                clicked = Some(path.clone());
            }
            let painter = ui.painter();
            if i == self.selected {
                painter.rect_filled(rect, CornerRadius::same(6), palette.accent.gamma_multiply(0.22));
            }
            let text_width = rect.width() - 20.0;
            let name = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
            let folder = path.parent().map(|p| self.tidy(p)).unwrap_or_default();
            let left = rect.left() + 10.0;
            painter.galley(pos2(left, rect.top() + 4.0), line(ui, name, 13.5, palette.text, text_width), palette.text);
            painter.galley(
                pos2(left, rect.top() + 21.0),
                line(ui, folder, 11.0, palette.text_weak, text_width),
                palette.text_weak,
            );
        }
        clicked.or_else(|| enter.then(|| matches[self.selected].clone()))
    }

    fn tidy(&self, folder: &Path) -> String {
        match self.home.as_deref().and_then(|home| folder.strip_prefix(home).ok()) {
            Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
            Some(rest) => format!("~/{}", rest.display()),
            None => folder.display().to_string(),
        }
    }
}

/// One line of text, cut off with `…` past `width`.
fn line(ui: &Ui, text: String, size: f32, color: Color32, width: f32) -> Arc<Galley> {
    let mut job = LayoutJob::simple_singleline(text, FontId::proportional(size), color);
    job.wrap.max_width = width;
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    ui.painter().layout_job(job)
}

/// The files matching `query`, best first.
fn ranked_matches<'a>(files: &'a [PathBuf], query: &str) -> impl Iterator<Item = &'a PathBuf> {
    let words: Vec<String> = query.to_lowercase().split_whitespace().map(str::to_owned).collect();
    let mut hits: Vec<(u8, &PathBuf)> = files.iter().filter_map(|p| Some((match_tier(p, &words)?, p))).collect();
    // Stable, so files within a tier stay in frecency order.
    hits.sort_by_key(|(tier, _)| *tier);
    hits.into_iter().map(|(_, p)| p)
}

/// How well `path` matches every word (lower is better), or `None` if some word doesn't match.
fn match_tier(path: &Path, words: &[String]) -> Option<u8> {
    let full = path.to_string_lossy().to_lowercase();
    let name = path.file_name().map_or_else(|| full.clone(), |n| n.to_string_lossy().to_lowercase());
    words.iter().try_fold(0, |worst, word| {
        let tier = if name.starts_with(word.as_str()) {
            0
        } else if name.contains(word.as_str()) {
            1
        } else if full.contains(word.as_str()) {
            2
        } else if is_subsequence(word, &name) {
            3
        } else {
            return None;
        };
        Some(worst.max(tier))
    })
}

fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut rest = haystack.chars();
    needle.chars().all(|c| rest.any(|h| h == c))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(files: &[&str], query: &str) -> Vec<String> {
        let files: Vec<PathBuf> = files.iter().map(PathBuf::from).collect();
        ranked_matches(&files, query).map(|p| p.display().to_string()).collect()
    }

    #[test]
    fn empty_query_keeps_frecency_order() {
        assert_eq!(find(&["/b.pdf", "/a.pdf"], ""), ["/b.pdf", "/a.pdf"]);
    }

    #[test]
    fn better_matches_come_first() {
        let files = ["/papers/xattention.pdf", "/attention/notes.pdf", "/Attention Is All.pdf", "/a_t_t_n.pdf"];
        assert_eq!(
            find(&files, "atten"),
            ["/Attention Is All.pdf", "/papers/xattention.pdf", "/attention/notes.pdf"]
        );
        assert_eq!(find(&files, "attn"), ["/papers/xattention.pdf", "/Attention Is All.pdf", "/a_t_t_n.pdf"]);
    }

    #[test]
    fn every_word_must_match() {
        let files = ["/ml/attention.pdf", "/bio/attention.pdf", "/ml/resnet.pdf"];
        assert_eq!(find(&files, "ml att"), ["/ml/attention.pdf"]);
        assert!(find(&files, "zzz").is_empty());
    }
}
