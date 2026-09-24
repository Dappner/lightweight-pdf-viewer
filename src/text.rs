//! Page text as search sees it: folded for matching, with a compact map back to the UTF-16
//! offsets of PDFKit's page string, which is what its selections take.
//!
//! Folding is Unicode's full case folding (so µ matches μ, ß matches ss and ﬁ matches fi),
//! and collapses each run of whitespace into one space, so a phrase matches across line
//! breaks. One source character (or whitespace
//! run) can fold into several bytes; those bytes are atomic: a match may not start or end
//! inside them, so counts, navigation and highlights agree (the query "s" does not match
//! inside "ß", but "ss" does).
//!
//! Matches never overlap and are found left to right.

use std::ops::Range;

use crate::case_folding::FOLDS;

/// Bump when [`PageText::new`] folds differently or [`PageText::encode`] changes, so text
/// saved by an older version is read again. Changes to the folding table are caught by
/// [`rules`] on their own.
const RULES_VERSION: u64 = 1;

/// Marks a run as atomic: all of its folded bytes stand for its whole source span.
const ATOMIC: u32 = 1 << 31;

/// A span of folded text. In a plain run each folded byte stands for one UTF-16 unit of the
/// source (ASCII); an atomic run folds one character, or one whitespace run, as a whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Run {
    /// Start in the folded text, in bytes.
    at: u32,
    /// Start in the source, in UTF-16 units, with [`ATOMIC`] set for atomic runs.
    source: u32,
}

impl Run {
    fn atomic(&self) -> bool {
        self.source & ATOMIC != 0
    }

    fn source(&self) -> u32 {
        self.source & !ATOMIC
    }
}

/// One page's text, folded, with its map back to the source.
pub struct PageText {
    text: String,
    runs: Vec<Run>,
    /// UTF-16 length of the source.
    source_len: u32,
}

/// A search query, folded like page text. Never empty.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Query(String);

impl Query {
    /// `None` for a query with nothing to search for (empty or only whitespace).
    pub fn new(query: &str) -> Option<Self> {
        let text = PageText::new(query).text;
        (!text.trim().is_empty()).then_some(Self(text))
    }
}

/// A fingerprint of how page text is folded and encoded: saved text is only reused while it
/// matches.
pub fn rules() -> u64 {
    static RULES: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *RULES.get_or_init(|| {
        let mut hash = fnv(FNV_OFFSET, &RULES_VERSION.to_le_bytes());
        for (from, to) in FOLDS {
            hash = fnv(hash, &(*from as u32).to_le_bytes());
            hash = fnv(hash, to.as_bytes());
            hash = fnv(hash, &[0]);
        }
        hash
    })
}

pub const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

/// FNV-1a, continuing from `hash`: stable across builds, unlike std's hasher.
pub fn fnv(hash: u64, bytes: &[u8]) -> u64 {
    bytes.iter().fold(hash, |h, &b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3))
}

/// What a non-ASCII character case-folds to, if not itself.
fn fold(c: char) -> Option<&'static str> {
    FOLDS.binary_search_by_key(&c, |&(from, _)| from).ok().map(|i| FOLDS[i].1)
}

impl PageText {
    pub fn new(source: &str) -> Self {
        let mut text = String::with_capacity(source.len());
        let mut runs: Vec<Run> = Vec::new();
        let mut unit = 0u32;
        // The whitespace run being collapsed: where it starts, and whether it is a single
        // ASCII character (one unit folding to one byte, so it can join a plain run).
        let mut space: Option<(u32, bool)> = None;

        fn push(text: &mut String, runs: &mut Vec<Run>, folded: &str, source: u32, atomic: bool) {
            let at = text.len() as u32;
            let continues = !atomic && runs.last().is_some_and(|r| !r.atomic() && at - r.at == source - r.source());
            if !continues {
                runs.push(Run { at, source: source | if atomic { ATOMIC } else { 0 } });
            }
            text.push_str(folded);
        }

        for c in source.chars() {
            if c.is_whitespace() {
                space = Some(match space {
                    Some((start, _)) => (start, false),
                    None => (unit, c.is_ascii()),
                });
            } else {
                if let Some((start, plain)) = space.take() {
                    push(&mut text, &mut runs, " ", start, !plain);
                }
                let mut buf = [0u8; 4];
                if c.is_ascii() {
                    push(&mut text, &mut runs, c.to_ascii_lowercase().encode_utf8(&mut buf), unit, false);
                } else {
                    let folded = fold(c).unwrap_or_else(|| c.encode_utf8(&mut buf));
                    push(&mut text, &mut runs, folded, unit, true);
                }
            }
            unit += c.len_utf16() as u32;
        }
        if let Some((start, plain)) = space {
            push(&mut text, &mut runs, " ", start, !plain);
        }
        text.shrink_to_fit();
        runs.shrink_to_fit();
        Self { text, runs, source_len: unit }
    }

    /// Heap bytes held.
    pub fn bytes(&self) -> usize {
        self.text.capacity() + self.runs.capacity() * size_of::<Run>()
    }

    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
    }

    /// The run containing folded byte `at` (`at` < text length).
    fn run(&self, at: usize) -> &Run {
        &self.runs[self.runs.partition_point(|r| r.at as usize <= at) - 1]
    }

    /// Whether a match may start or end at folded byte `at`.
    fn is_boundary(&self, at: usize) -> bool {
        if at >= self.text.len() {
            return true;
        }
        let run = self.run(at);
        !run.atomic() || run.at as usize == at
    }

    /// The source offset of a boundary.
    fn to_source(&self, at: usize) -> u32 {
        if at >= self.text.len() {
            return self.source_len;
        }
        let run = self.run(at);
        run.source() + (at as u32 - run.at)
    }

    /// Calls `f` with each match of `query`, in folded bytes, left to right.
    pub fn find(&self, query: &Query, mut f: impl FnMut(Range<usize>)) {
        let q = query.0.as_str();
        let mut from = 0;
        while let Some(i) = self.text[from..].find(q) {
            let start = from + i;
            let end = start + q.len();
            if self.is_boundary(start) && self.is_boundary(end) {
                f(start..end);
                from = end;
            } else {
                from = start + self.text[start..].chars().next().map_or(1, char::len_utf8);
            }
        }
    }

    /// Appends the page in the form [`Self::decode`] reads: source length, text length, run
    /// count, the text, then each run's start and source, all little-endian `u32`s.
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.extend(self.source_len.to_le_bytes());
        out.extend((self.text.len() as u32).to_le_bytes());
        out.extend((self.runs.len() as u32).to_le_bytes());
        out.extend(self.text.as_bytes());
        for run in &self.runs {
            out.extend(run.at.to_le_bytes());
            out.extend(run.source.to_le_bytes());
        }
    }

    /// A page written by [`Self::encode`], or `None` if `bytes` aren't one: every run must
    /// lie inside the text and the source, in order, so a damaged file can't make matching
    /// panic or map outside the page.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let u32_at = |i: usize| Some(u32::from_le_bytes(bytes.get(i..i + 4)?.try_into().ok()?));
        let source_len = u32_at(0)?;
        let text_len = u32_at(4)? as usize;
        let run_count = u32_at(8)? as usize;
        let text = std::str::from_utf8(bytes.get(12..12usize.checked_add(text_len)?)?).ok()?;
        let tail = &bytes[12 + text_len..];
        if tail.len() != run_count.checked_mul(8)? {
            return None;
        }
        let runs: Vec<Run> = tail
            .chunks_exact(8)
            .map(|c| Run {
                at: u32::from_le_bytes(c[..4].try_into().unwrap()),
                source: u32::from_le_bytes(c[4..].try_into().unwrap()),
            })
            .collect();
        if runs.is_empty() != text.is_empty() || runs.first().is_some_and(|r| r.at != 0) {
            return None;
        }
        for (i, run) in runs.iter().enumerate() {
            let (end, next_source) = match runs.get(i + 1) {
                Some(next) => (next.at as usize, next.source()),
                None => (text.len(), source_len),
            };
            let len = end.checked_sub(run.at as usize).filter(|&n| n > 0)?;
            // A plain run maps each byte to one unit; an atomic one takes at least one unit.
            let units = if run.atomic() { 1 } else { len as u32 };
            if !text.is_char_boundary(run.at as usize) || run.source().checked_add(units)? > next_source {
                return None;
            }
            if !run.atomic() && !text.as_bytes()[run.at as usize..end].is_ascii() {
                return None;
            }
        }
        Some(Self { text: text.to_owned(), runs, source_len })
    }

    pub fn count(&self, query: &Query) -> u32 {
        let mut n = 0;
        self.find(query, |_| n += 1);
        n
    }

    /// Source ranges (UTF-16 start and length) of matches `range` of `query`, in order.
    pub fn source_ranges(&self, query: &Query, range: Range<u32>) -> Vec<(u32, u32)> {
        let mut out = Vec::new();
        let mut i = 0;
        self.find(query, |m| {
            if range.contains(&i) {
                let start = self.to_source(m.start);
                out.push((start, self.to_source(m.end) - start));
            }
            i += 1;
        });
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The source text of each match of `query` in `source`, as PDFKit would select it.
    fn matches(source: &str, query: &str) -> Vec<String> {
        let page = PageText::new(source);
        let query = Query::new(query).unwrap();
        let units: Vec<u16> = source.encode_utf16().collect();
        page.source_ranges(&query, 0..u32::MAX)
            .into_iter()
            .map(|(start, len)| String::from_utf16(&units[start as usize..(start + len) as usize]).unwrap())
            .collect()
    }

    #[test]
    fn folds_case_and_maps_back() {
        assert_eq!(matches("The cat. THE END", "the"), ["The", "THE"]);
        assert_eq!(matches("aaaa", "aa"), ["aa", "aa"], "matches don't overlap");
    }

    #[test]
    fn collapses_whitespace() {
        assert_eq!(matches("heat\n  transfer", "heat transfer"), ["heat\n  transfer"]);
        assert_eq!(matches("a\u{00A0}b", "a b"), ["a\u{00A0}b"]);
        // The query's whitespace collapses too.
        assert_eq!(matches("a \n b", "a  b"), ["a \n b"]);
    }

    #[test]
    fn expanding_folds_are_atomic() {
        assert_eq!(matches("Straße", "strasse"), ["Straße"]);
        assert_eq!(matches("Straße", "ss"), ["ß"]);
        assert!(matches("Straße", "s").len() == 1, "only the ASCII s, not half of ß");
        assert_eq!(matches("İstanbul", "i̇stanbul"), ["İstanbul"]);
        assert!(matches("İstanbul", "stanbul").len() == 1);
        assert!(matches("İx", "i").is_empty(), "İ folds to i + combining dot; i alone is inside it");
    }

    #[test]
    fn ligatures_expand() {
        assert_eq!(matches("e\u{FB03}cient", "efficient"), ["e\u{FB03}cient"]);
        assert_eq!(matches("\u{FB01}nd", "fi"), ["\u{FB01}"]);
        assert!(matches("\u{FB01}nd", "f").is_empty());
    }

    #[test]
    fn combining_marks_and_accents() {
        // Precomposed é and e + combining acute are different text; each maps to itself.
        assert_eq!(matches("Café cafe\u{301}", "café"), ["Café"]);
        assert_eq!(matches("cafe\u{301}", "cafe\u{301}"), ["cafe\u{301}"]);
        assert_eq!(matches("ÉCOLE", "école"), ["ÉCOLE"]);
    }

    #[test]
    fn non_bmp_characters_take_two_units() {
        assert_eq!(matches("𝑥 = 𝑦 and x", "x"), ["x"]);
        assert_eq!(matches("𝐀𝐁 ab", "ab"), ["ab"]);
        assert_eq!(matches("𝐀𝐁 ab", "𝐀𝐁"), ["𝐀𝐁"]);
    }

    #[test]
    fn full_case_folding() {
        assert_eq!(matches("5 µm and 3 μm", "μm"), ["µm", "μm"], "micro sign folds to mu");
        assert_eq!(matches("5 µm", "µm"), ["µm"]);
        assert_eq!(matches("300 \u{212A}", "300 k"), ["300 \u{212A}"], "Kelvin sign folds to k");
        assert_eq!(matches("ǅ", "ǆ"), ["ǅ"], "title case folds too");
    }

    #[test]
    fn folding_table_is_sorted() {
        assert!(FOLDS.windows(2).all(|w| w[0].0 < w[1].0));
    }

    #[test]
    fn final_sigma() {
        assert_eq!(matches("ΟΔΟΣ οδος", "οδοσ"), ["ΟΔΟΣ", "οδος"]);
    }

    #[test]
    fn match_at_page_end() {
        assert_eq!(matches("ends with word", "word"), ["word"]);
        assert_eq!(matches("trailing space  ", "space "), ["space  "]);
    }

    #[test]
    fn empty_queries_are_rejected() {
        assert!(Query::new("").is_none());
        assert!(Query::new(" \n").is_none());
    }

    #[test]
    fn encodes_and_decodes() {
        for source in ["", "   ", "Hello  WORLD", "Straße ﬁne µ\u{1F600} x", "ΣΑΣ end\n"] {
            let page = PageText::new(source);
            let mut bytes = Vec::new();
            page.encode(&mut bytes);
            let back = PageText::decode(&bytes).expect(source);
            assert_eq!((&back.text, &back.runs, back.source_len), (&page.text, &page.runs, page.source_len));
        }
    }

    #[test]
    fn rejects_damaged_pages() {
        let mut bytes = Vec::new();
        PageText::new("Straße ﬁne text").encode(&mut bytes);
        for len in 0..bytes.len() {
            assert!(PageText::decode(&bytes[..len]).is_none(), "truncated to {len}");
        }
        // Flipping any single byte either still decodes to a page that stays in bounds, or
        // is rejected; matching must never panic.
        let query = Query::new("e").unwrap();
        for i in 0..bytes.len() {
            for bit in 0..8 {
                let mut damaged = bytes.clone();
                damaged[i] ^= 1 << bit;
                if let Some(page) = PageText::decode(&damaged) {
                    for (start, len) in page.source_ranges(&query, 0..u32::MAX) {
                        assert!(start + len <= page.source_len);
                    }
                }
            }
        }
    }

    #[test]
    fn plain_text_is_one_run() {
        let page = PageText::new("Plain ASCII text, one space each.");
        assert_eq!(page.runs.len(), 1);
    }
}
