//! Line-oriented markdown scanner: rope → styled spans, one `Vec` per line, in
//! line-local byte coordinates. The renderer flattens these into text runs; the
//! same spans are the structure model later features (outline, folding,
//! wikilinks, tasks) read from.
//!
//! Deliberately a line scanner, not a full parser — markdown is mostly
//! line-prefix-driven, so this covers the common cases cheaply. `parse` is the
//! seam: tree-sitter can replace its body behind the same signature if nested
//! grammars or incremental-reparse perf ever demand it.

use std::ops::Range;

use ropey::Rope;

/// What a stretch of source text *is*. The renderer maps each kind to a style;
/// structure features read the same kinds. A new kind is one variant here plus a
/// scanner case plus a render mapping — no change to the parse/render seam.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpanKind {
    Heading(u8),
    ListItem,
    BlockQuote,
    /// A ``` / ~~~ fence line.
    CodeFence,
    /// A line inside a fence.
    CodeText,
    /// A line inside leading `---` … `---` frontmatter.
    Frontmatter,
    Strong,
    Code,
    /// Syntactic punctuation (`##`, `**`, backticks, bullets) — rendered muted.
    Marker,
}

/// One styled stretch, in byte coordinates **within a single line** (matching
/// how `LineElement` works — line-local text, no newline). Spans may overlap;
/// `flatten` resolves them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub range: Range<usize>,
    pub kind: SpanKind,
}

/// A flattened, non-overlapping slice of a line. `kind == None` is default
/// (unstyled) text. Segment lengths sum to the line's byte length, which
/// `shape_line` requires or it drops glyphs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
    pub len: usize,
    pub kind: Option<SpanKind>,
}

/// Classify every line of the document into styled spans. Document-level because
/// fenced code and frontmatter span lines, but output is per-line so the
/// renderer grabs line `i`'s spans directly. Lines mirror the renderer's model
/// (`rope.len_lines()`, newline stripped).
pub fn parse(rope: &Rope) -> Vec<Vec<Span>> {
    let mut scan = Scan { in_fence: false, in_frontmatter: false };
    (0..rope.len_lines())
        .map(|i| {
            let text: String = rope.line(i).chars().filter(|&c| c != '\n').collect();
            scan.line(&text, i)
        })
        .collect()
}

struct Scan {
    in_fence: bool,
    in_frontmatter: bool,
}

impl Scan {
    fn line(&mut self, text: &str, idx: usize) -> Vec<Span> {
        let mut spans = Vec::new();
        let len = text.len();
        let trimmed = text.trim_start();
        let indent = len - trimmed.len(); // leading-whitespace bytes (ASCII ws here)

        // Frontmatter: a leading `---` block at the very top of the file.
        if self.in_frontmatter {
            spans.push(Span { range: 0..len, kind: SpanKind::Frontmatter });
            if text.trim() == "---" {
                self.in_frontmatter = false;
            }
            return spans;
        }
        if idx == 0 && text.trim() == "---" {
            self.in_frontmatter = true;
            spans.push(Span { range: 0..len, kind: SpanKind::Frontmatter });
            return spans;
        }

        // Fenced code: ``` / ~~~ toggles; lines between are code text. No inline
        // scan inside — its contents are literal.
        let is_fence = trimmed.starts_with("```") || trimmed.starts_with("~~~");
        if self.in_fence {
            let kind = if is_fence { SpanKind::CodeFence } else { SpanKind::CodeText };
            spans.push(Span { range: 0..len, kind });
            if is_fence {
                self.in_fence = false;
            }
            return spans;
        }
        if is_fence {
            self.in_fence = true;
            spans.push(Span { range: 0..len, kind: SpanKind::CodeFence });
            return spans;
        }

        // Line roles: a whole-line span (the structure model) plus a Marker over
        // the leading punctuation. Inline styling is then scanned over the whole
        // line — the marker prefix has no inline delimiters, so scanning it is
        // harmless and keeps one code path.
        if let Some(level) = heading_level(trimmed) {
            spans.push(Span { range: 0..len, kind: SpanKind::Heading(level) });
            // Marker spans the `#`s plus the one trailing space, so concealing a
            // heading leaves its title flush-left rather than space-indented.
            let marker_end = (indent + level as usize + 1).min(len);
            spans.push(Span { range: indent..marker_end, kind: SpanKind::Marker });
        } else if trimmed.starts_with('>') {
            spans.push(Span { range: 0..len, kind: SpanKind::BlockQuote });
            spans.push(Span { range: indent..indent + 1, kind: SpanKind::Marker });
        } else if let Some(marker_len) = list_marker(trimmed) {
            spans.push(Span { range: 0..len, kind: SpanKind::ListItem });
            spans.push(Span { range: indent..indent + marker_len, kind: SpanKind::Marker });
        }
        scan_inline(text, &mut spans);
        spans
    }
}

/// `1..=6` leading `#`, terminated by a space or end-of-line.
fn heading_level(trimmed: &str) -> Option<u8> {
    let hashes = trimmed.bytes().take_while(|&b| b == b'#').count();
    // `hashes` counts leading '#', so slicing at it stays on a char boundary.
    let after = &trimmed.as_bytes()[hashes..];
    let ok = (1..=6).contains(&hashes) && (after.is_empty() || after[0] == b' ');
    ok.then_some(hashes as u8)
}

/// Byte length of a list marker (`- `, `1.`), or `None`. The marker excludes the
/// trailing space; the bullet/number+punctuation is what's highlighted.
fn list_marker(trimmed: &str) -> Option<usize> {
    let b = trimmed.as_bytes();
    if b.len() >= 2 && matches!(b[0], b'-' | b'*' | b'+') && b[1] == b' ' {
        return Some(1);
    }
    let digits = b.iter().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0
        && b.len() > digits + 1
        && matches!(b[digits], b'.' | b')')
        && b[digits + 1] == b' '
    {
        return Some(digits + 1);
    }
    None
}

/// Whether `line` (newline stripped) is a markdown list item — smart-tab uses
/// this to decide whether Tab shifts the whole line or inserts at the caret.
pub fn is_list_item(line: &str) -> bool {
    list_marker(line.trim_start()).is_some()
}

/// How a smart newline should treat the line under the caret. The vim grammar
/// can't see buffer text, so list continuation is decided here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListContinuation {
    /// Not a list item — a plain newline.
    Plain,
    /// A list item. `prefix` (indent + marker + space) carries onto the next
    /// line; `empty` means the item had no content, so the caller may clear the
    /// marker instead of repeating it.
    Item { prefix: String, empty: bool },
}

/// Decide how Enter / `o` continues a markdown list from `line` (newline
/// stripped). Ordered markers increment (`1.` → `2.`); unordered repeat the
/// bullet. Checkbox state isn't modeled, so `- [ ]` continues as a plain bullet.
pub fn list_continuation(line: &str) -> ListContinuation {
    let trimmed = line.trim_start();
    let Some(marker_len) = list_marker(trimmed) else {
        return ListContinuation::Plain;
    };
    let indent = &line[..line.len() - trimmed.len()];
    let marker = &trimmed[..marker_len];
    // Content past the marker and its single trailing space (all ASCII so far).
    let empty = trimmed[marker_len + 1..].trim().is_empty();
    let next = match marker[..marker_len - 1].parse::<u64>() {
        Ok(n) => format!("{}{}", n + 1, &marker[marker_len - 1..]), // ordered: bump number
        Err(_) => marker.to_string(),                               // unordered: repeat bullet
    };
    ListContinuation::Item { prefix: format!("{indent}{next} "), empty }
}

/// Single left-to-right pass for inline `code` and `**strong**`, emitting a
/// `Marker` for each delimiter and the kind for the inner text. Backtick code is
/// matched first so `**` inside it stays literal. ASCII delimiters only, so
/// scanning raw bytes is safe across multi-byte chars (continuation bytes are
/// ≥ 0x80, never `*` or `` ` ``).
///
// ponytail: bold + code only; single `*`/`_` emphasis, links, escapes, and
// nesting are new SpanKind cases here — model and renderer already handle them.
fn scan_inline(text: &str, out: &mut Vec<Span>) {
    let b = text.as_bytes();
    let n = b.len();
    let mut i = 0;
    while i < n {
        if b[i] == b'`' {
            if let Some(end) = (i + 1..n).find(|&j| b[j] == b'`') {
                out.push(Span { range: i..i + 1, kind: SpanKind::Marker });
                if end > i + 1 {
                    out.push(Span { range: i + 1..end, kind: SpanKind::Code });
                }
                out.push(Span { range: end..end + 1, kind: SpanKind::Marker });
                i = end + 1;
                continue;
            }
        } else if b[i] == b'*' && i + 1 < n && b[i + 1] == b'*' {
            if let Some(end) = find_double_star(b, i + 2) {
                out.push(Span { range: i..i + 2, kind: SpanKind::Marker });
                if end > i + 2 {
                    out.push(Span { range: i + 2..end, kind: SpanKind::Strong });
                }
                out.push(Span { range: end..end + 2, kind: SpanKind::Marker });
                i = end + 2;
                continue;
            }
        }
        i += 1;
    }
}

fn find_double_star(b: &[u8], from: usize) -> Option<usize> {
    (from..b.len().saturating_sub(1)).find(|&j| b[j] == b'*' && b[j + 1] == b'*')
}

/// Flatten possibly-overlapping spans into a gap-free, non-overlapping sequence
/// covering exactly `line_len` bytes. Higher-priority kinds (markers, then
/// inline, then line roles) win per byte; gaps are `None`. An empty line yields
/// no segments.
pub fn flatten(line_len: usize, spans: &[Span]) -> Vec<Segment> {
    // ponytail: O(line_len × spans) per-byte paint; lines are short. A sweep over
    // sorted boundaries if a pathological line ever shows up in a profile.
    let mut bytes: Vec<Option<SpanKind>> = vec![None; line_len];
    for s in spans {
        for byte in s.range.clone() {
            if byte >= line_len {
                continue;
            }
            bytes[byte] = Some(match bytes[byte] {
                Some(cur) if priority(cur) > priority(s.kind) => cur,
                _ => s.kind,
            });
        }
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i < line_len {
        let kind = bytes[i];
        let mut j = i + 1;
        while j < line_len && bytes[j] == kind {
            j += 1;
        }
        out.push(Segment { len: j - i, kind });
        i = j;
    }
    out
}

/// A line's display form with `Marker` segments dropped (their bytes removed),
/// so markdown renders without its syntax punctuation. `segments` still sum to
/// `text.len()` — the invariant `shape_line` requires.
pub struct Concealed {
    pub text: String,
    pub segments: Vec<Segment>,
    /// Display byte each source byte lands at (`source_len + 1` entries; the
    /// last maps one-past-end). Bytes of dropped markers collapse to the point
    /// of removal, so source-coordinate spans (selection/search highlights)
    /// can be remapped onto the display text.
    pub map: Vec<usize>,
}

/// Drop `Marker` segments from a flattened line, returning the concealed text,
/// the segments that survive, and the source→display byte map. Boundaries fall
/// on char boundaries (markers are all ASCII), so slicing `text` is safe. The
/// cursor line renders from source instead, so this byte shift never reaches
/// caret math.
pub fn conceal(text: &str, segments: &[Segment]) -> Concealed {
    let mut out_text = String::with_capacity(text.len());
    let mut out_segments = Vec::with_capacity(segments.len());
    let mut map = Vec::with_capacity(text.len() + 1);
    let mut byte = 0;
    for seg in segments {
        let end = byte + seg.len;
        let slice = &text[byte..end];
        if seg.kind != Some(SpanKind::Marker) || keep_marker(slice) {
            map.extend((0..seg.len).map(|i| out_text.len() + i));
            out_text.push_str(slice);
            out_segments.push(*seg);
        } else {
            map.extend(std::iter::repeat(out_text.len()).take(seg.len));
        }
        byte = end;
    }
    map.push(out_text.len());
    Concealed { text: out_text, segments: out_segments, map }
}

/// List bullets, ordered-list numbers, and the blockquote bar stay visible when
/// rendering — they're structural prefixes with no rendered substitute yet, so
/// concealing them would orphan the content. Heading `#`, `**`, and backticks
/// are dropped.
fn keep_marker(marker: &str) -> bool {
    let b = marker.as_bytes();
    matches!(marker, "-" | "*" | "+" | ">")
        || (b.len() > 1
            && matches!(b[b.len() - 1], b'.' | b')')
            && b[..b.len() - 1].iter().all(u8::is_ascii_digit))
}

fn priority(k: SpanKind) -> u8 {
    match k {
        SpanKind::Marker => 4,
        SpanKind::Strong | SpanKind::Code => 3,
        SpanKind::CodeFence | SpanKind::CodeText => 2,
        SpanKind::Heading(_) | SpanKind::ListItem | SpanKind::BlockQuote | SpanKind::Frontmatter => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind_at(segs: &[Segment], byte: usize) -> Option<SpanKind> {
        let mut acc = 0;
        for s in segs {
            if byte < acc + s.len {
                return s.kind;
            }
            acc += s.len;
        }
        None
    }

    #[test]
    fn flatten_covers_line_exactly_and_inline_beats_base() {
        // "## **hi**": heading over the whole line, markers on ## and **, strong
        // on "hi". Bytes: 0,1=## 2=space 3,4=** 5,6=hi 7,8=**.
        let line = "## **hi**";
        let spans = vec![
            Span { range: 0..line.len(), kind: SpanKind::Heading(2) },
            Span { range: 0..2, kind: SpanKind::Marker },
            Span { range: 3..5, kind: SpanKind::Marker },
            Span { range: 5..7, kind: SpanKind::Strong },
            Span { range: 7..9, kind: SpanKind::Marker },
        ];
        let segs = flatten(line.len(), &spans);
        // Gap-free, exact coverage — the invariant shape_line depends on.
        assert_eq!(segs.iter().map(|s| s.len).sum::<usize>(), line.len());
        assert_eq!(kind_at(&segs, 0), Some(SpanKind::Marker)); // '#'
        assert_eq!(kind_at(&segs, 2), Some(SpanKind::Heading(2))); // ' '
        assert_eq!(kind_at(&segs, 5), Some(SpanKind::Strong)); // 'h'
        assert_eq!(kind_at(&segs, 8), Some(SpanKind::Marker)); // '*'
    }

    #[test]
    fn empty_line_has_no_segments() {
        assert!(flatten(0, &[]).is_empty());
    }

    #[test]
    fn conceal_drops_markers_and_keeps_invariant() {
        let line = "## **hi** there";
        let segs = flatten(line.len(), &parse(&Rope::from_str(line))[0]);
        let c = conceal(line, &segs);
        // `## ` and the `**` pairs gone; content (bold + plain) kept.
        assert_eq!(c.text, "hi there");
        // Segments still cover the concealed text exactly — the shape_line rule.
        assert_eq!(c.segments.iter().map(|s| s.len).sum::<usize>(), c.text.len());
        // Map: kept bytes land at their display positions, dropped marker bytes
        // collapse to the removal point. "## **hi** there" → "hi there".
        assert_eq!(c.map.len(), line.len() + 1);
        assert_eq!(c.map[0], 0); // dropped '#'
        assert_eq!(c.map[5], 0); // 'h'
        assert_eq!(c.map[9], 2); // ' ' after the closing `**`
        assert_eq!(c.map[10], 3); // 't'
        assert_eq!(c.map[line.len()], c.text.len());
    }

    #[test]
    fn conceal_keeps_list_and_quote_markers() {
        for line in ["- item", "> quote", "1. first", "2) second"] {
            let segs = flatten(line.len(), &parse(&Rope::from_str(line))[0]);
            assert_eq!(conceal(line, &segs).text, line);
        }
    }

    #[test]
    fn scanner_classifies_heading_strong_and_fence() {
        let rope = Rope::from_str("# Title **x**\n```\ncode\n```\n");
        let spans = parse(&rope);
        assert!(spans[0].iter().any(|s| matches!(s.kind, SpanKind::Heading(1))));
        assert!(spans[0].iter().any(|s| s.kind == SpanKind::Strong));
        assert!(spans[1].iter().any(|s| s.kind == SpanKind::CodeFence));
        assert!(spans[2].iter().any(|s| s.kind == SpanKind::CodeText));
        assert!(spans[3].iter().any(|s| s.kind == SpanKind::CodeFence));
    }

    #[test]
    fn list_continuation_cases() {
        use ListContinuation::*;
        assert_eq!(list_continuation("plain text"), Plain);
        assert_eq!(
            list_continuation("  - item"),
            Item { prefix: "  - ".into(), empty: false }
        );
        // Ordered markers increment and keep their punctuation.
        assert_eq!(
            list_continuation("3) item"),
            Item { prefix: "4) ".into(), empty: false }
        );
        // An empty item is flagged so Enter can clear it.
        assert_eq!(
            list_continuation("- "),
            Item { prefix: "- ".into(), empty: true }
        );
    }

    #[test]
    fn list_and_blockquote_markers() {
        let rope = Rope::from_str("- item\n> quote\n1. first\n");
        let spans = parse(&rope);
        assert!(spans[0].iter().any(|s| s.kind == SpanKind::ListItem));
        assert!(spans[1].iter().any(|s| s.kind == SpanKind::BlockQuote));
        assert!(spans[2].iter().any(|s| s.kind == SpanKind::ListItem));
    }
}
