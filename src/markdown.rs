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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
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
    /// `*text*` / `_text_` — italic. Claims its bytes like `Strong`, so
    /// nothing nested inside it re-scans.
    Emphasis,
    Code,
    /// The visible text of a `[[wikilink]]`, `[text](url)`, or bare URL.
    Link,
    /// A thematic-break line (`---`/`***`/`___`) — concealed to a painted
    /// hairline.
    Rule,
    /// A `[ ]`/`[x]` task box after a list marker; the bool is checked. The
    /// bytes survive conceal (a painted box covers them).
    Task(bool),
    /// Syntactic punctuation (`##`, `**`, backticks, bullets) — rendered muted.
    Marker,
    /// `~~struck~~` content. Unlike every other kind this one claims no bytes:
    /// `flatten` diverts it into `Segment::struck`, so a strike composes with
    /// whatever the text also is (bold, a link, code) instead of replacing it.
    /// Never appears in a `Segment::kind`.
    Strike,
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Segment {
    pub len: usize,
    pub kind: Option<SpanKind>,
    /// Struck through, orthogonal to `kind` — `SpanKind::Strike` and checked
    /// task lines both set it, and it survives whatever kind won the bytes.
    pub struck: bool,
}

/// A document's per-line spans, plus the scanner state entering each line.
///
/// Derefs to `[Vec<Span>]`, so callers index it by line like the plain `Vec` it
/// replaced. The recorded states are what make `reparse_line` possible: they let
/// a one-line edit prove it didn't disturb any other line's classification.
/// `Clone` exists for `Rc::make_mut` in the incremental path — it only fires
/// when the cached parse is *not* uniquely held, which is off the hot path.
#[derive(Clone)]
pub struct Parsed {
    lines: Vec<Vec<Span>>,
    /// State entering line `i`, with `states[lines.len()]` the state after the
    /// last line — so `len == lines.len() + 1`.
    states: Vec<ScanState>,
}

impl std::ops::Deref for Parsed {
    type Target = [Vec<Span>];
    fn deref(&self) -> &Self::Target {
        &self.lines
    }
}

impl Parsed {
    /// Empty spans for every line: a non-markdown buffer renders as plain text.
    pub fn blank(lines: usize) -> Self {
        Parsed { lines: vec![Vec::new(); lines], states: vec![ScanState::default(); lines + 1] }
    }
}

/// Classify every line of the document into styled spans. Document-level because
/// fenced code and frontmatter span lines, but output is per-line so the
/// renderer grabs line `i`'s spans directly. Lines mirror the renderer's model
/// (`rope.len_lines()`, newline stripped).
pub fn parse(rope: &Rope) -> Parsed {
    let n = rope.len_lines();
    let mut lines = Vec::with_capacity(n);
    let mut states = Vec::with_capacity(n + 1);
    let mut scan = ScanState::default();
    for i in 0..n {
        states.push(scan);
        lines.push(scan.line(&line_text(rope, i), i));
    }
    states.push(scan);
    Parsed { lines, states }
}

/// Re-scan only `line`, reusing every other line's spans — the incremental path
/// behind insert-mode typing, where one keystroke changes one line.
///
/// `prev` must be the parse of this rope's state immediately before the edit,
/// and the edit must have left the line count alone (the caller proves both;
/// `Document::single_line_edit` is how the editor does it).
///
/// Returns `false`, having changed nothing, when the edit flipped the scanner
/// state leaving that line — typing a ``` fence or opening frontmatter
/// reclassifies every line below, so the caller must fall back to a full parse
/// and a full row rebuild.
pub fn reparse_line(rope: &Rope, prev: &mut Parsed, line: usize) -> bool {
    if line >= prev.lines.len() || prev.lines.len() != rope.len_lines() {
        return false;
    }
    let mut state = prev.states[line];
    let spans = state.line(&line_text(rope, line), line);
    if state != prev.states[line + 1] {
        return false; // fence/frontmatter boundary moved; everything below shifts
    }
    prev.lines[line] = spans;
    true
}

/// Text of line `i` with its trailing newline dropped — the form every scanner
/// function here takes, and what the editor and document layers feed them.
///
/// Slices the newline off and bulk-copies rather than filtering char by char:
/// the newline is only ever last (`Rope::line` splits on them), and this runs
/// once per line over the whole document on every reparse. A CRLF line keeps
/// its `\r`.
pub fn line_text(rope: &Rope, i: usize) -> String {
    let slice = rope.line(i);
    let n = slice.len_chars();
    let end = if n > 0 && slice.char(n - 1) == '\n' { n - 1 } else { n };
    slice.slice(..end).to_string()
}

/// A `**`/`~~` run opened on an earlier line with no closer yet, so the next
/// line's scan resumes it instead of treating the stray marker as untouched
/// text and an unrelated later `**`/`~~` as a fresh opener.
// ponytail: one slot, not a stack — if a `**` and a `~~` are both left open,
// nested, by the same line, only the later-detected one carries forward; the
// other reverts to pre-fix behavior (formatting lost, not corrupted). Upgrade
// to a small `Vec<OpenRun>` if that combination is ever actually reported.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum OpenRun {
    #[default]
    None,
    Strong,
    Strike,
}

/// What the scanner carries from one line to the next. Fenced code and
/// frontmatter both span lines, so a line's classification depends on this —
/// which is also why an edit that changes it invalidates every line below.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct ScanState {
    in_fence: bool,
    in_frontmatter: bool,
    open_run: OpenRun,
}

impl ScanState {
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
            self.open_run = OpenRun::None; // block boundary: an open run doesn't survive it
            return spans;
        }
        if idx == 0 && text.trim() == "---" {
            self.in_frontmatter = true;
            spans.push(Span { range: 0..len, kind: SpanKind::Frontmatter });
            self.open_run = OpenRun::None;
            return spans;
        }

        // Fenced code: ``` / ~~~ toggles; lines between are code text. No inline
        // scan inside — its contents are literal.
        let is_fence = fence_marker(text).is_some();
        if self.in_fence {
            let kind = if is_fence { SpanKind::CodeFence } else { SpanKind::CodeText };
            spans.push(Span { range: 0..len, kind });
            if is_fence {
                self.in_fence = false;
            }
            self.open_run = OpenRun::None;
            return spans;
        }
        if is_fence {
            self.in_fence = true;
            spans.push(Span { range: 0..len, kind: SpanKind::CodeFence });
            self.open_run = OpenRun::None;
            return spans;
        }

        // Thematic break: the whole line is exactly `---`/`***`/`___` (a `---`
        // on line 0 is frontmatter, handled above). Nothing else scans — the
        // concealed render replaces the text with a hairline.
        if matches!(text.trim(), "---" | "***" | "___") {
            spans.push(Span { range: 0..len, kind: SpanKind::Rule });
            self.open_run = OpenRun::None;
            return spans;
        }

        // A blank line ends the paragraph, so an open run can't reach past it.
        if trimmed.is_empty() {
            self.open_run = OpenRun::None;
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
            // Marker spans the `>` plus the one space after it, so every row of
            // a concealed quote starts flush left — the painted bar's clearance
            // is a paint inset, uniform across a wrapped line's rows.
            let marker_end = (indent + 2).min(len);
            spans.push(Span { range: indent..marker_end, kind: SpanKind::Marker });
        } else if let Some(marker_len) = list_marker(trimmed) {
            spans.push(Span { range: 0..len, kind: SpanKind::ListItem });
            spans.push(Span { range: indent..indent + marker_len, kind: SpanKind::Marker });
            if let Some((at, checked)) = task_box(text) {
                spans.push(Span { range: at..at + 3, kind: SpanKind::Task(checked) });
                // A checked item strikes its text. The range starts past the box
                // — covering the `Task` bytes would rule through the painted
                // checkbox — and stops at the last non-blank, so the line
                // neither begins in the gap after the box nor trails past the
                // text. `- [x]` with nothing after it gets no span.
                if checked {
                    let rest = &text[at + 3..];
                    let body = at + 3 + (rest.len() - rest.trim_start().len());
                    let end = at + 3 + rest.trim_end().len();
                    if body < end {
                        spans.push(Span { range: body..end, kind: SpanKind::Strike });
                    }
                }
            }
        }
        scan_inline(text, &mut spans, &mut self.open_run);
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

/// The fence marker opening `line` (newline stripped): its indent plus the run
/// of ``` / ~~~, or `None` when the line isn't a fence. The whole run comes
/// back so a generated closer can match the opener's length. Shared by the span
/// scanner and the smart newline that pairs a closing fence.
pub fn fence_marker(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    let b = trimmed.as_bytes();
    let c = *b.first().filter(|&&c| c == b'`' || c == b'~')?;
    let run = b.iter().take_while(|&&x| x == c).count(); // ASCII: bytes are chars
    (run >= 3).then(|| &line[..(line.len() - trimmed.len()) + run])
}

/// The GFM task box on `line` (newline stripped): byte offset of its `[` and
/// the checked state. A box is `[ ]`/`[x]`/`[X]` directly after a list
/// marker's space, followed by a space or end-of-line. Shared by the span
/// scanner, `Document::toggle_task`, and the editor's checkbox-click test.
pub fn task_box(line: &str) -> Option<(usize, bool)> {
    let trimmed = line.trim_start();
    let at = (line.len() - trimmed.len()) + list_marker(trimmed)? + 1;
    let rest = line.as_bytes().get(at..)?;
    let checked = if rest.starts_with(b"[ ]") {
        false
    } else if rest.starts_with(b"[x]") || rest.starts_with(b"[X]") {
        true
    } else {
        return None;
    };
    (rest.get(3).map_or(true, |&b| b == b' ')).then_some((at, checked))
}

/// Ordered-list item as `(number, digit count)`; `None` for unordered items
/// and non-items.
pub fn ordered_item(line: &str) -> Option<(u64, usize)> {
    let trimmed = line.trim_start();
    let marker_len = list_marker(trimmed)?;
    let digits = marker_len - 1; // marker = digits + '.'/')', or a lone bullet
    let n = trimmed[..digits].parse::<u64>().ok()?; // bullets: empty str, no parse
    Some((n, digits))
}

/// How a smart newline should treat the line under the caret. The vim grammar
/// can't see buffer text, so prefix continuation is decided here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListContinuation {
    /// No carrying prefix — a plain newline.
    Plain,
    /// A line whose prefix carries: a list marker, a blockquote `>`, or both.
    /// `prefix` lands on the next line; `empty` means nothing followed it, so
    /// the caller may clear the prefix instead of repeating it.
    Item { prefix: String, empty: bool },
}

/// Decide how Enter / `o` continues the markdown line `line` (newline
/// stripped). Ordered markers increment (`1.` → `2.`); unordered repeat the
/// bullet. A task box counts as part of the marker: it carries onto the next
/// line always unchecked, and doesn't count as item content. A blockquote `>`
/// carries too, wrapping whatever the quote holds — a nested quote or a list
/// inside one continues as itself.
pub fn line_continuation(line: &str) -> ListContinuation {
    let trimmed = line.trim_start();
    // Blockquote: peel the `>` and the one space after it (when present) and
    // continue the quoted line, so the prefix mirrors the source's own style.
    if trimmed.starts_with('>') {
        let at = (line.len() - trimmed.len())
            + 1
            + usize::from(trimmed.as_bytes().get(1) == Some(&b' '));
        let (head, body) = line.split_at(at);
        return match line_continuation(body) {
            ListContinuation::Item { prefix, empty } => {
                ListContinuation::Item { prefix: format!("{head}{prefix}"), empty }
            }
            ListContinuation::Plain => {
                ListContinuation::Item { prefix: head.to_string(), empty: body.trim().is_empty() }
            }
        };
    }
    let Some(marker_len) = list_marker(trimmed) else {
        return ListContinuation::Plain;
    };
    let indent = &line[..line.len() - trimmed.len()];
    let marker = &trimmed[..marker_len];
    let task = task_box(line).is_some();
    // Content past the marker, its single trailing space, and any task box (all
    // ASCII so far). `- [ ]` with no trailing space ends exactly at the box.
    let content_at = (marker_len + 1 + if task { 3 } else { 0 }).min(trimmed.len());
    let empty = trimmed[content_at..].trim().is_empty();
    let next = match marker[..marker_len - 1].parse::<u64>() {
        Ok(n) => format!("{}{}", n + 1, &marker[marker_len - 1..]), // ordered: bump number
        Err(_) => marker.to_string(),                               // unordered: repeat bullet
    };
    let box_str = if task { "[ ] " } else { "" };
    ListContinuation::Item { prefix: format!("{indent}{next} {box_str}"), empty }
}

/// The closing fence a smart newline should pair with `line`, whose text is
/// `text`: `Some` only when that line opens a fence and the document is still
/// inside one at EOF, so nothing closes it. The closer mirrors the opener's
/// indent and marker run, dropping any language tag.
pub fn fence_to_close(parsed: &Parsed, text: &str, line: usize) -> Option<String> {
    // `states[i]` is the state *before* line `i`, so `[line + 1]` is what this
    // line leaves and `last()` is EOF. The marker is what keeps this off the
    // lines *inside* a fence, which leave `in_fence` set just the same.
    let opens = parsed.states.get(line + 1)?.in_fence;
    let unclosed = parsed.states.last()?.in_fence;
    (opens && unclosed).then(|| fence_marker(text).map(str::to_string)).flatten()
}

/// Single left-to-right pass for inline `code`, `**strong**`, `*em*`/`_em_`,
/// `~~strike~~`, `[[wikilinks]]`, `[text](url)` links, and bare `http(s)://`
/// URLs, emitting a `Marker` for each delimiter and the kind for the inner
/// text. Positional scanning gives precedence to whatever opens first — a
/// backtick consumes past any `[[` or URL inside it, so code spans stay
/// literal. `~~` is the one arm that resumes inside its own run rather than
/// past it, so strike composes with the emphasis nested in it. `**`/`~~` left
/// unclosed on a line carry via `open_run` into the next one; single-char
/// emphasis is the one inline form that does not — an unclosed `*`/`_` stays
/// literal rather than opening a run that spans lines. ASCII delimiters only,
/// so scanning raw bytes is safe across multi-byte chars (continuation bytes
/// are ≥ 0x80, never a delimiter byte).
fn scan_inline(text: &str, out: &mut Vec<Span>, open_run: &mut OpenRun) {
    let b = text.as_bytes();
    let n = b.len();
    let mut i = 0;
    // Where the open `~~` run closes, so the scan recognizes that delimiter as
    // a closer rather than opening a second run at it — the `~~` arm keeps
    // scanning *inside* its run, unlike the arms that jump past their content.
    let mut strike_close = None;
    // A `**`/`~~` opened on a previous line: find its closer before scanning
    // this line normally. Strong's content isn't rescanned (matching its
    // no-nesting rule below); Strike's is, via the same `strike_close`
    // mechanism the intra-line case below uses.
    match *open_run {
        OpenRun::None => {}
        OpenRun::Strong => match find_double(b, 0, b'*') {
            Some(end) => {
                if end > 0 {
                    out.push(Span { range: 0..end, kind: SpanKind::Strong });
                }
                out.push(Span { range: end..end + 2, kind: SpanKind::Marker });
                *open_run = OpenRun::None;
                i = end + 2;
            }
            None => {
                if n > 0 {
                    out.push(Span { range: 0..n, kind: SpanKind::Strong });
                }
                return;
            }
        },
        OpenRun::Strike => match find_double(b, 0, b'~') {
            Some(end) => {
                if end > 0 {
                    out.push(Span { range: 0..end, kind: SpanKind::Strike });
                }
                out.push(Span { range: end..end + 2, kind: SpanKind::Marker });
                *open_run = OpenRun::None;
                strike_close = Some(end);
            }
            None => {
                if n > 0 {
                    out.push(Span { range: 0..n, kind: SpanKind::Strike });
                }
                // Stays open; still fall into the loop below at i == 0 so any
                // nested span in this line's stretch of the run keeps composing.
            }
        },
    }
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
            match find_double(b, i + 2, b'*') {
                Some(end) => {
                    out.push(Span { range: i..i + 2, kind: SpanKind::Marker });
                    if end > i + 2 {
                        out.push(Span { range: i + 2..end, kind: SpanKind::Strong });
                    }
                    out.push(Span { range: end..end + 2, kind: SpanKind::Marker });
                    i = end + 2;
                    continue;
                }
                None => {
                    // No closer on this line: the run carries into the next.
                    out.push(Span { range: i..i + 2, kind: SpanKind::Marker });
                    if i + 2 < n {
                        out.push(Span { range: i + 2..n, kind: SpanKind::Strong });
                    }
                    *open_run = OpenRun::Strong;
                    return;
                }
            }
        } else if matches!(b[i], b'*' | b'_') && b.get(i + 1) != Some(&b[i]) && can_open(b, i) {
            // Unclosed runs stay literal — no `OpenRun` carry, unlike `**`.
            if let Some(end) = find_emphasis_close(b, i + 1, b[i]) {
                out.push(Span { range: i..i + 1, kind: SpanKind::Marker });
                out.push(Span { range: i + 1..end, kind: SpanKind::Emphasis });
                out.push(Span { range: end..end + 1, kind: SpanKind::Marker });
                i = end + 1;
                continue;
            }
        } else if b[i] == b'~' && i + 1 < n && b[i + 1] == b'~' {
            if strike_close == Some(i) {
                strike_close = None; // this run's closer; its spans are out already
                i += 2;
                continue;
            }
            match find_double(b, i + 2, b'~') {
                Some(end) => {
                    out.push(Span { range: i..i + 2, kind: SpanKind::Marker });
                    if end > i + 2 {
                        out.push(Span { range: i + 2..end, kind: SpanKind::Strike });
                    }
                    out.push(Span { range: end..end + 2, kind: SpanKind::Marker });
                    // Resume just past the opener, not past the run: `~~**x**~~`
                    // needs its inner delimiters scanned, and Strike claims no
                    // bytes so it can't collide with what they claim.
                    strike_close = Some(end);
                    i += 2;
                    continue;
                }
                None => {
                    // No closer on this line: the run carries into the next,
                    // same as `**` — but the rest of the line still scans
                    // (below), since Strike composes with what's nested in it.
                    out.push(Span { range: i..i + 2, kind: SpanKind::Marker });
                    if i + 2 < n {
                        out.push(Span { range: i + 2..n, kind: SpanKind::Strike });
                    }
                    *open_run = OpenRun::Strike;
                    i += 2;
                    continue;
                }
            }
        } else if b[i] == b'[' && i + 1 < n && b[i + 1] == b'[' {
            if let Some(close) = find_double(b, i + 2, b']') {
                let inner = &text[i + 2..close];
                if !inner.is_empty() {
                    // `[[target|alias]]`: conceal `[[target|`, show the alias.
                    // A note-less `[[#Heading]]` also conceals the `#`, so it
                    // reads as the heading's title rather than a tag.
                    let vis = match inner.find('|') {
                        Some(p) => i + 2 + p + 1,
                        None if inner.starts_with('#') => i + 3,
                        None => i + 2,
                    };
                    out.push(Span { range: i..vis, kind: SpanKind::Marker });
                    if close > vis {
                        out.push(Span { range: vis..close, kind: SpanKind::Link });
                    }
                    out.push(Span { range: close..close + 2, kind: SpanKind::Marker });
                    i = close + 2;
                    continue;
                }
            }
        } else if b[i] == b'[' {
            // `[text](url)`: conceal `[` and `](url)`, show the text.
            if let Some(mid) = text[i + 1..].find("](").map(|p| i + 1 + p) {
                if let Some(close) = text[mid + 2..].find(')').map(|p| mid + 2 + p) {
                    if mid > i + 1 && close > mid + 2 {
                        out.push(Span { range: i..i + 1, kind: SpanKind::Marker });
                        out.push(Span { range: i + 1..mid, kind: SpanKind::Link });
                        out.push(Span { range: mid..close + 1, kind: SpanKind::Marker });
                        i = close + 1;
                        continue;
                    }
                }
            }
        } else if b[i] == b'h'
            && (text[i..].starts_with("http://") || text[i..].starts_with("https://"))
        {
            let end = bare_url_end(text, i);
            out.push(Span { range: i..end, kind: SpanKind::Link });
            i = end;
            continue;
        }
        i += 1;
    }
}

fn find_double(b: &[u8], from: usize, ch: u8) -> Option<usize> {
    (from..b.len().saturating_sub(1)).find(|&j| b[j] == ch && b[j + 1] == ch)
}

/// Word-ish byte: ASCII alphanumeric, or any UTF-8 lead/continuation byte
/// (so `naïve_case` counts as intraword the same as `snake_case`).
fn word_byte(b: Option<&u8>) -> bool {
    b.is_some_and(|c| c.is_ascii_alphanumeric() || *c >= 0x80)
}

/// CommonMark-lite flanking. An emphasis delimiter opens only when what
/// follows isn't whitespace or end-of-line — which is also what keeps a `* `
/// list bullet and a lone `*` in prose (`2 * 3`) from opening a run. `_`
/// additionally refuses to open inside a word, so `snake_case` stays literal.
fn can_open(b: &[u8], i: usize) -> bool {
    b.get(i + 1).is_some_and(|c| !c.is_ascii_whitespace())
        && !(b[i] == b'_' && i > 0 && word_byte(b.get(i - 1)))
}

/// Mirror of `can_open`: a closer can't be preceded by whitespace, and `_`
/// can't close intraword.
fn can_close(b: &[u8], i: usize) -> bool {
    i > 0 && !b[i - 1].is_ascii_whitespace() && !(b[i] == b'_' && word_byte(b.get(i + 1)))
}

/// The closing delimiter for a run opened with `ch`, skipping doubled
/// delimiters — `**`/`__` belong to strong (or to literal text), never to a
/// single-char run.
fn find_emphasis_close(b: &[u8], from: usize, ch: u8) -> Option<usize> {
    let mut j = from;
    while j < b.len() {
        if b[j] == ch {
            if b.get(j + 1) == Some(&ch) {
                j += 2;
                continue;
            }
            if can_close(b, j) {
                return Some(j);
            }
        }
        j += 1;
    }
    None
}

/// End of a bare URL starting at `start`: runs to ASCII whitespace, then
/// trailing punctuation is dropped. `)` stays — URLs contain parens
/// (Wikipedia), so `(see https://x.com)` grabbing the `)` is the accepted wart.
fn bare_url_end(text: &str, start: usize) -> usize {
    let b = text.as_bytes();
    let mut end = (start..b.len()).find(|&j| b[j].is_ascii_whitespace()).unwrap_or(b.len());
    while end > start
        && matches!(b[end - 1], b'.' | b',' | b';' | b':' | b'!' | b'?' | b'"' | b'\'')
    {
        end -= 1;
    }
    end
}

/// The wikilink under `char_col` in `line` (newline stripped), if any, as
/// `(note, heading)`. The full `[[...]]` span counts, brackets included;
/// `|alias` is dropped. Either half can be absent: `[[note]]` targets a note's
/// start, `[[note#Sec]]` a heading in it, `[[#Sec]]` a heading in the document
/// the link itself lives in (empty note name). A link with neither is `None`.
pub fn wikilink_at(line: &str, char_col: usize) -> Option<(String, Option<String>)> {
    let byte = line.char_indices().nth(char_col).map_or(line.len(), |(b, _)| b);
    let mut from = 0;
    while let Some(open) = line[from..].find("[[").map(|p| from + p) {
        let close = line[open + 2..].find("]]").map(|p| open + 2 + p)?;
        if byte < close + 2 {
            if byte < open {
                return None; // caret sits before this link; links don't nest
            }
            let body = line[open + 2..close].split('|').next().unwrap_or("");
            let (note, heading) = match body.split_once('#') {
                Some((n, h)) => (n.trim(), Some(h.trim()).filter(|h| !h.is_empty())),
                None => (body.trim(), None),
            };
            if note.is_empty() && heading.is_none() {
                return None;
            }
            return Some((note.to_string(), heading.map(str::to_string)));
        }
        from = close + 2;
    }
    None
}

/// A heading line's title: the text after the `#` markers, trimmed. `None` if
/// the line isn't a heading. Inline markup stays in — `## **Bold**` titles
/// itself `**Bold**`.
pub fn heading_text(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    // `heading_level` counts leading ASCII `#`, so the slice is on a boundary.
    let level = heading_level(trimmed)? as usize;
    Some(trimmed[level..].trim())
}

/// The external-link destination under `char_col`, if any: a `[text](url)`
/// span (whole thing counts, brackets and parens included) or a bare
/// `http(s)://` run. Scheme-less destinations get `https://` prepended — the
/// result is ready for `open_url`.
pub fn url_at(line: &str, char_col: usize) -> Option<String> {
    let byte = line.char_indices().nth(char_col).map_or(line.len(), |(b, _)| b);
    // `[text](url)` spans first — a caret inside one never falls through.
    let mut from = 0;
    while let Some(open) = line[from..].find('[').map(|p| from + p) {
        let Some(mid) = line[open + 1..].find("](").map(|p| open + 1 + p) else {
            break;
        };
        let Some(close) = line[mid + 2..].find(')').map(|p| mid + 2 + p) else {
            break;
        };
        if byte <= close {
            if byte < open {
                break; // caret before this link; a bare URL may still sit under it
            }
            let url = line[mid + 2..close].trim();
            if url.is_empty() {
                return None;
            }
            return Some(if url.contains("://") {
                url.to_string()
            } else {
                format!("https://{url}")
            });
        }
        from = close + 1;
    }
    // Bare `http(s)://` run under the caret.
    let mut from = 0;
    while let Some(start) = line[from..].find("http").map(|p| from + p) {
        if line[start..].starts_with("http://") || line[start..].starts_with("https://") {
            let end = bare_url_end(line, start);
            if byte >= start && byte < end {
                return Some(line[start..end].to_string());
            }
            from = end.max(start + 4);
        } else {
            from = start + 4;
        }
    }
    None
}

/// Flatten possibly-overlapping spans into a gap-free, non-overlapping sequence
/// covering exactly `line_len` bytes. Higher-priority kinds (markers, then
/// inline, then line roles) win per byte; gaps are `None`. An empty line yields
/// no segments.
pub fn flatten(line_len: usize, spans: &[Span]) -> Vec<Segment> {
    // ponytail: O(line_len × spans) per-byte paint; lines are short. A sweep over
    // sorted boundaries if a pathological line ever shows up in a profile.
    let mut bytes: Vec<Option<SpanKind>> = vec![None; line_len];
    let mut struck = vec![false; line_len];
    for s in spans {
        for byte in s.range.clone() {
            if byte >= line_len {
                continue;
            }
            // Strike is a decoration, not a claim on the byte: it sets the bit
            // and leaves the winning kind (and its color/weight) alone.
            if s.kind == SpanKind::Strike {
                struck[byte] = true;
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
        let (kind, st) = (bytes[i], struck[i]);
        let mut j = i + 1;
        while j < line_len && bytes[j] == kind && struck[j] == st {
            j += 1;
        }
        out.push(Segment { len: j - i, kind, struck: st });
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

/// Drop `Marker` segments (plus rule and fence lines) from a flattened line,
/// returning the concealed text, the segments that survive, and the
/// source→display byte map. Boundaries fall
/// on char boundaries (markers are all ASCII), so slicing `text` is safe. The
/// cursor line renders from source instead, so this byte shift never reaches
/// caret math.
pub fn conceal(text: &str, segments: &[Segment]) -> Concealed {
    let mut out_text = String::with_capacity(text.len());
    let mut out_segments = Vec::with_capacity(segments.len());
    let mut map = Vec::with_capacity(text.len() + 1);
    let mut byte = 0;
    // A bullet only survives concealment on a line that actually is a list
    // item, and only as its leading marker — an emphasis `*` mid-line has
    // the same one-byte slice and must drop.
    let bullet_line = is_list_item(text);
    for seg in segments {
        let end = byte + seg.len;
        let slice = &text[byte..end];
        let drop = match seg.kind {
            Some(SpanKind::Marker) => {
                !(bullet_line && out_text.trim().is_empty() && keep_marker(slice))
            }
            // Rule and fence lines vanish (fence includes any language tag);
            // the renderer paints a hairline / the code band on the blank row.
            Some(SpanKind::Rule | SpanKind::CodeFence) => true,
            _ => false,
        };
        if drop {
            map.extend(std::iter::repeat(out_text.len()).take(seg.len));
        } else {
            map.extend((0..seg.len).map(|i| out_text.len() + i));
            out_text.push_str(slice);
            out_segments.push(*seg);
        }
        byte = end;
    }
    map.push(out_text.len());
    Concealed { text: out_text, segments: out_segments, map }
}

/// List bullets and ordered-list numbers stay visible when rendering —
/// structural prefixes with no rendered substitute. Heading `#`, `**`,
/// backticks, and the blockquote `>` (replaced by a painted bar) are dropped.
/// The caller also requires the line to be a list item and the marker to sit
/// in the line's leading position — a bare string match can't tell a leading
/// bullet from an emphasis `*` that happens to have the same one-byte slice.
fn keep_marker(marker: &str) -> bool {
    let b = marker.as_bytes();
    matches!(marker, "-" | "*" | "+")
        || (b.len() > 1
            && matches!(b[b.len() - 1], b'.' | b')')
            && b[..b.len() - 1].iter().all(u8::is_ascii_digit))
}

fn priority(k: SpanKind) -> u8 {
    match k {
        // Never ranked — `flatten` diverts Strike into the struck bit first.
        SpanKind::Strike => 0,
        SpanKind::Marker | SpanKind::Task(_) => 4,
        SpanKind::Strong | SpanKind::Emphasis | SpanKind::Code | SpanKind::Link => 3,
        SpanKind::CodeFence | SpanKind::CodeText => 2,
        SpanKind::Heading(_)
        | SpanKind::ListItem
        | SpanKind::BlockQuote
        | SpanKind::Frontmatter
        | SpanKind::Rule => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg_at(segs: &[Segment], byte: usize) -> Segment {
        let mut acc = 0;
        for s in segs {
            if byte < acc + s.len {
                return *s;
            }
            acc += s.len;
        }
        Segment { len: 0, kind: None, struck: false }
    }

    fn kind_at(segs: &[Segment], byte: usize) -> Option<SpanKind> {
        seg_at(segs, byte).kind
    }

    #[test]
    fn reparse_line_reuses_the_parse_and_bails_when_state_moves() {
        // A plain text edit: local, and every other line's spans are reused.
        let before = Rope::from_str("# a\ntext\n```\ncode\n```\ntail\n");
        let mut p = parse(&before);
        let after = Rope::from_str("# a\ntext **b**\n```\ncode\n```\ntail\n");
        assert!(reparse_line(&after, &mut p, 1));
        assert!(p[1].iter().any(|s| s.kind == SpanKind::Strong));
        assert_eq!(p[3][0].kind, SpanKind::CodeText); // untouched, still in-fence

        // Typing a fence where there wasn't one flips every line below from
        // plain text to code, so the local path must refuse and change nothing.
        let before = Rope::from_str("a\nb\nc\n");
        let mut p = parse(&before);
        let snapshot = p[1].clone();
        let after = Rope::from_str("a\n```\nc\n");
        assert!(!reparse_line(&after, &mut p, 1));
        assert_eq!(p[1], snapshot);

        // Removing a fence is the same hazard in reverse.
        let before = Rope::from_str("a\n```\nc\n```\n");
        let mut p = parse(&before);
        let after = Rope::from_str("a\nx\nc\n```\n");
        assert!(!reparse_line(&after, &mut p, 1));

        // Opening frontmatter on line 0 reclassifies the lines after it.
        let before = Rope::from_str("a\nb\n");
        let mut p = parse(&before);
        let after = Rope::from_str("---\nb\n");
        assert!(!reparse_line(&after, &mut p, 0));

        // A line-count change invalidates the index mapping outright.
        let before = Rope::from_str("a\nb\nc\n");
        let mut p = parse(&before);
        let after = Rope::from_str("a\nb\n");
        assert!(!reparse_line(&after, &mut p, 1));

        // An out-of-range line is refused rather than panicking.
        let rope = Rope::from_str("a\n");
        let mut p = parse(&rope);
        assert!(!reparse_line(&rope, &mut p, 99));
    }

    #[test]
    fn incremental_parse_matches_a_full_one() {
        // The whole point: the fast path must be indistinguishable from the
        // slow one. Edit each line of a document with mixed structure and
        // compare against a from-scratch parse.
        let lines = ["# head", "- [ ] task", "plain **bold** text", "> quote", "`code`", "tail"];
        for i in 0..lines.len() {
            let before = Rope::from_str(&format!("{}\n", lines.join("\n")));
            let mut edited: Vec<&str> = lines.to_vec();
            let replacement = "changed *text* here";
            edited[i] = replacement;
            let after = Rope::from_str(&format!("{}\n", edited.join("\n")));

            let mut incremental = parse(&before);
            assert!(reparse_line(&after, &mut incremental, i), "line {i}");
            let full = parse(&after);
            for l in 0..full.len() {
                assert_eq!(incremental[l], full[l], "line {i} edited, line {l} differs");
            }
        }
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
    fn conceal_keeps_list_markers_drops_quote_marker() {
        for line in ["- item", "1. first", "2) second"] {
            let segs = flatten(line.len(), &parse(&Rope::from_str(line))[0]);
            assert_eq!(conceal(line, &segs).text, line);
        }
        // The `>` and its space conceal — a painted bar replaces them, and the
        // text's clearance from it is a paint inset (see `QUOTE_PAD`).
        let line = "> quote";
        let segs = flatten(line.len(), &parse(&Rope::from_str(line))[0]);
        assert_eq!(conceal(line, &segs).text, "quote");
    }

    #[test]
    fn rule_lines_scan_and_conceal_to_empty() {
        let spans = parse(&Rope::from_str("x\n---\n***\n___\n"));
        for i in 1..4 {
            assert_eq!(spans[i][0].kind, SpanKind::Rule);
        }
        // A `---` on line 0 stays frontmatter, not a rule.
        assert_eq!(parse(&Rope::from_str("---\n"))[0][0].kind, SpanKind::Frontmatter);
        // Concealed, a rule row has no text — the renderer paints the hairline.
        let segs = flatten(3, &spans[1]);
        assert_eq!(conceal("---", &segs).text, "");
    }

    #[test]
    fn fence_lines_conceal_to_empty_code_text_stays() {
        let spans = parse(&Rope::from_str("```rust\ncode\n```\n"));
        let segs = flatten(7, &spans[0]);
        assert_eq!(conceal("```rust", &segs).text, "");
        let segs = flatten(4, &spans[1]);
        assert_eq!(conceal("code", &segs).text, "code");
    }

    #[test]
    fn task_boxes_scan_and_survive_conceal() {
        let spans = parse(&Rope::from_str("- [ ] milk\n- [x] done\n1. [X] num\n- [x]tight\n"));
        assert!(spans[0].contains(&Span { range: 2..5, kind: SpanKind::Task(false) }));
        assert!(spans[1].contains(&Span { range: 2..5, kind: SpanKind::Task(true) }));
        assert!(spans[2].contains(&Span { range: 3..6, kind: SpanKind::Task(true) }));
        // No space after the box — not a task (GFM).
        assert!(!spans[3].iter().any(|s| matches!(s.kind, SpanKind::Task(_))));
        // The box bytes stay in the concealed text; the painted box covers them.
        let line = "- [ ] milk";
        let segs = flatten(line.len(), &parse(&Rope::from_str(line))[0]);
        let c = conceal(line, &segs);
        assert_eq!(c.text, line);
        assert!(c.segments.iter().any(|s| s.kind == Some(SpanKind::Task(false))));
    }

    #[test]
    fn checked_tasks_strike_their_text_only() {
        // "- [x] done  ": box at 2..5, text "done" at 6..10, trailing blanks.
        let line = "- [x] done  ";
        let segs = flatten(line.len(), &parse(&Rope::from_str(line))[0]);
        assert!(seg_at(&segs, 6).struck); // 'd'
        assert!(!seg_at(&segs, 5).struck); // gap after the box
        assert!(!seg_at(&segs, 10).struck); // trailing blank
        // The box itself is never struck — a rule there crosses the painted
        // checkbox drawn over these bytes.
        assert!(!seg_at(&segs, 3).struck);

        // Unchecked items are untouched; so is a box with no text after it.
        for line in ["- [ ] todo", "- [x]"] {
            let segs = flatten(line.len(), &parse(&Rope::from_str(line))[0]);
            assert!(!segs.iter().any(|s| s.struck), "{line}");
        }

        // Strike composes: the wikilink keeps its Link kind and gains the rule.
        let line = "- [x] read [[note]]";
        let segs = flatten(line.len(), &parse(&Rope::from_str(line))[0]);
        assert_eq!(kind_at(&segs, 13), Some(SpanKind::Link)); // 'n' of note
        assert!(seg_at(&segs, 13).struck);
    }

    #[test]
    fn task_box_positions_and_state() {
        assert_eq!(task_box("- [ ] a"), Some((2, false)));
        assert_eq!(task_box("  - [x] a"), Some((4, true)));
        assert_eq!(task_box("1. [X] a"), Some((3, true)));
        assert_eq!(task_box("- [ ]"), Some((2, false))); // box ends the line
        assert_eq!(task_box("- [y] a"), None);
        assert_eq!(task_box("- [ ]x"), None); // needs a space after the box
        assert_eq!(task_box("[ ] a"), None); // needs a list marker
    }

    #[test]
    fn wikilinks_style_and_conceal() {
        let line = "see [[note]] and [[a/b|B]]";
        let segs = flatten(line.len(), &parse(&Rope::from_str(line))[0]);
        // Target/alias render as Link; brackets (and `target|`) conceal.
        assert_eq!(kind_at(&segs, 6), Some(SpanKind::Link)); // 'n' of note
        assert_eq!(kind_at(&segs, 23), Some(SpanKind::Link)); // 'B'
        assert_eq!(conceal(line, &segs).text, "see note and B");
        // A same-document heading link renders as the bare title.
        let line = "see [[#Payroll]]";
        let segs = flatten(line.len(), &parse(&Rope::from_str(line))[0]);
        assert_eq!(conceal(line, &segs).text, "see Payroll");
        // Inside a code span `[[x]]` stays literal.
        let line = "`[[x]]`";
        let segs = flatten(line.len(), &parse(&Rope::from_str(line))[0]);
        assert!(!segs.iter().any(|s| s.kind == Some(SpanKind::Link)));
    }

    #[test]
    fn markdown_links_and_bare_urls_style_and_conceal() {
        let line = "see [GPUI](https://gpui.rs) now";
        let segs = flatten(line.len(), &parse(&Rope::from_str(line))[0]);
        assert_eq!(kind_at(&segs, 5), Some(SpanKind::Link)); // 'G'
        assert_eq!(conceal(line, &segs).text, "see GPUI now");
        // Degenerate forms stay literal text.
        for line in ["[x]()", "[](y)", "[[]]"] {
            let segs = flatten(line.len(), &parse(&Rope::from_str(line))[0]);
            assert!(!segs.iter().any(|s| s.kind == Some(SpanKind::Link)));
        }
        // A bare URL is styled to its trimmed extent (trailing comma dropped).
        let spans = &parse(&Rope::from_str("go to https://a.com, ok"))[0];
        assert!(spans.contains(&Span { range: 6..19, kind: SpanKind::Link }));
    }

    #[test]
    fn wikilink_at_hit_zones_and_stripping() {
        let at = |line: &str, col: usize| {
            wikilink_at(line, col).map(|(n, h)| (n, h.unwrap_or_default()))
        };
        let line = "see [[a/b|B]] end";
        // Anywhere on the span — brackets, target, alias — yields the target.
        for col in [4, 7, 10, 12] {
            assert_eq!(at(line, col), Some(("a/b".into(), String::new())));
        }
        // `#heading` splits off; an alias after it doesn't reach either half.
        assert_eq!(at("x [[note#a sec]]", 5), Some(("note".into(), "a sec".into())));
        assert_eq!(at("[[note#sec|S]]", 2), Some(("note".into(), "sec".into())));
        assert_eq!(at("[[#4.6 Payroll]]", 2), Some((String::new(), "4.6 Payroll".into())));
        assert!(at(line, 0).is_none()); // before the link
        assert!(at(line, 15).is_none()); // after the link
        assert!(at("[[]] x", 1).is_none()); // empty target
        assert!(at("[[#]] x", 1).is_none()); // empty on both sides of the `#`
    }

    #[test]
    fn heading_text_strips_markers() {
        let payroll = "4.6 Payroll build functions";
        assert_eq!(heading_text(&format!("### {payroll}")), Some(payroll));
        assert_eq!(heading_text("  #  Spaced  "), Some("Spaced"));
        assert_eq!(heading_text("#"), Some(""));
        assert_eq!(heading_text("#no space"), None);
        assert_eq!(heading_text("####### too deep"), None);
        assert_eq!(heading_text("not a heading"), None);
    }

    #[test]
    fn url_at_markdown_and_bare() {
        let line = "a [x](https://a.com) b";
        // The whole [text](url) span counts, brackets and parens included.
        for col in [2, 3, 8, 19] {
            assert_eq!(url_at(line, col).as_deref(), Some("https://a.com"));
        }
        assert!(url_at(line, 0).is_none());
        assert!(url_at(line, 21).is_none());
        // Scheme-less destination gets https:// prepended.
        assert_eq!(url_at("[repo](github.com/foo)", 1).as_deref(), Some("https://github.com/foo"));
        // Bare URL under the caret; trailing punctuation excluded.
        let line = "see https://a.com. end";
        assert_eq!(url_at(line, 6).as_deref(), Some("https://a.com"));
        assert!(url_at(line, 17).is_none()); // the trailing dot is off-link
        // A bare URL before a later [x](y) link on the same line still resolves.
        assert_eq!(url_at("https://a.com [x](y)", 3).as_deref(), Some("https://a.com"));
        assert!(url_at("[x]()", 1).is_none()); // empty destination
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
    fn strike_composes_with_nested_spans_and_stops_at_its_run() {
        //  0123456789..                     1        2
        //  ~~**bold**~~ and ~~plain~~   →  byte 13 = 'a' of "and"
        let line = "~~**bold**~~ and ~~plain~~";
        let segs = flatten(line.len(), &parse(&Rope::from_str(line))[0]);
        // Strike sets the bit without claiming bytes, so the nested `**` still
        // wins its kind: "bold" is Strong *and* struck.
        assert_eq!(kind_at(&segs, 4), Some(SpanKind::Strong));
        assert!(seg_at(&segs, 4).struck);
        // Text between two runs stays unstruck — the closing `~~` must not read
        // as an opener for the next one.
        assert!(!seg_at(&segs, 13).struck);
        assert!(seg_at(&segs, 19).struck && kind_at(&segs, 19).is_none()); // "plain"
        // `~~` and the inner `**` conceal; the strike rides the surviving text.
        let c = conceal(line, &segs);
        assert_eq!(c.text, "bold and plain");
        assert_eq!(c.segments.iter().map(|s| s.len).sum::<usize>(), c.text.len());
        assert!(seg_at(&c.segments, 0).struck); // 'b' of bold
        assert!(!seg_at(&c.segments, 5).struck); // 'a' of and
    }

    #[test]
    fn unclosed_strike_opens_a_run_instead_of_staying_literal() {
        // No closer anywhere in the document: like an unclosed `**`, the run
        // strikes forward from the opener rather than staying literal.
        for line in ["~~oops", "a ~~ b"] {
            let segs = flatten(line.len(), &parse(&Rope::from_str(line))[0]);
            assert!(segs.iter().any(|s| s.struck), "{line}");
        }
    }

    #[test]
    fn strike_run_carries_across_lines_without_leaking_into_the_gap() {
        // Same shape as the `**` case below, but for `~~`, proving the
        // generalized `OpenRun` carry (not a `**`-only special case).
        let rope = Rope::from_str("~~ab\ncd~~ ef ~~gh\nij~~\n");
        let spans = parse(&rope);

        assert_eq!(
            spans[0],
            vec![
                Span { range: 0..2, kind: SpanKind::Marker },
                Span { range: 2..4, kind: SpanKind::Strike },
            ]
        );
        assert_eq!(
            spans[1],
            vec![
                Span { range: 0..2, kind: SpanKind::Strike }, // "cd"
                Span { range: 2..4, kind: SpanKind::Marker },
                Span { range: 8..10, kind: SpanKind::Marker },
                Span { range: 10..12, kind: SpanKind::Strike }, // "gh"
            ]
        );
        assert_eq!(
            spans[2],
            vec![
                Span { range: 0..2, kind: SpanKind::Strike }, // "ij"
                Span { range: 2..4, kind: SpanKind::Marker },
            ]
        );
    }

    #[test]
    fn strike_run_open_on_this_line_still_scans_nested_spans() {
        // "~~a **b": the strike opens and never closes, but the nested "**b"
        // bold opener inside it must still get its own Marker — composition
        // doesn't stop just because the run is heading into the next line.
        let line = "~~a **b";
        let spans = &parse(&Rope::from_str(line))[0];
        assert!(spans.iter().any(|s| s.kind == SpanKind::Strike));
        assert!(spans.iter().any(|s| s.kind == SpanKind::Marker && s.range == (4..6)));
    }

    #[test]
    fn unclosed_strike_run_stops_at_a_block_boundary() {
        let rope = Rope::from_str("~~open\n\nplain\n");
        let spans = parse(&rope);
        assert!(spans[0].iter().any(|s| s.kind == SpanKind::Strike));
        assert!(spans[2].iter().all(|s| s.kind != SpanKind::Strike));
    }

    #[test]
    fn strong_run_carries_across_lines_without_leaking_into_the_gap() {
        // Span 1 opens on line 0, closes on line 1 ("cd"); span 2 opens later
        // on line 1 and closes on line 2 ("gh"..."ij"). The plain " ef "
        // between them must stay unstyled — the orphaned closer of span 1 is
        // not a fresh opener paired with span 2's opener.
        let rope = Rope::from_str("**ab\ncd** ef **gh\nij**\n");
        let spans = parse(&rope);

        assert_eq!(
            spans[0],
            vec![
                Span { range: 0..2, kind: SpanKind::Marker },
                Span { range: 2..4, kind: SpanKind::Strong },
            ]
        );
        assert_eq!(
            spans[1],
            vec![
                Span { range: 0..2, kind: SpanKind::Strong }, // "cd"
                Span { range: 2..4, kind: SpanKind::Marker },
                Span { range: 8..10, kind: SpanKind::Marker },
                Span { range: 10..12, kind: SpanKind::Strong }, // "gh"
            ]
        );
        assert_eq!(
            spans[2],
            vec![
                Span { range: 0..2, kind: SpanKind::Strong }, // "ij"
                Span { range: 2..4, kind: SpanKind::Marker },
            ]
        );
    }

    #[test]
    fn unclosed_strong_run_stops_at_a_block_boundary() {
        let rope = Rope::from_str("**open\n\nplain\n");
        let spans = parse(&rope);
        assert!(spans[0].iter().any(|s| s.kind == SpanKind::Strong)); // dangling run still marks its own line
        assert!(spans[2].iter().all(|s| s.kind != SpanKind::Strong)); // blank line resets it
    }

    #[test]
    fn line_continuation_cases() {
        use ListContinuation::*;
        assert_eq!(line_continuation("plain text"), Plain);
        assert_eq!(
            line_continuation("  - item"),
            Item { prefix: "  - ".into(), empty: false }
        );
        // Ordered markers increment and keep their punctuation.
        assert_eq!(
            line_continuation("3) item"),
            Item { prefix: "4) ".into(), empty: false }
        );
        // An empty item is flagged so Enter can clear it.
        assert_eq!(
            line_continuation("- "),
            Item { prefix: "- ".into(), empty: true }
        );
        // Tasks continue as tasks, always unchecked; the box isn't content.
        assert_eq!(
            line_continuation("- [x] done"),
            Item { prefix: "- [ ] ".into(), empty: false }
        );
        assert_eq!(
            line_continuation("  2. [ ] a"),
            Item { prefix: "  3. [ ] ".into(), empty: false }
        );
        for line in ["- [ ] ", "- [ ]"] {
            assert_eq!(
                line_continuation(line),
                Item { prefix: "- [ ] ".into(), empty: true },
                "{line}"
            );
        }
        // A blockquote prefix carries, mirroring the source's own spacing, and
        // wraps whatever it holds: a nested quote or a list inside it.
        assert_eq!(
            line_continuation("> quote"),
            Item { prefix: "> ".into(), empty: false }
        );
        assert_eq!(line_continuation(">no space"), Item { prefix: ">".into(), empty: false });
        assert_eq!(
            line_continuation("  >> deep"),
            Item { prefix: "  >> ".into(), empty: false }
        );
        assert_eq!(
            line_continuation("> - [ ] task"),
            Item { prefix: "> - [ ] ".into(), empty: false }
        );
        // Empty at every level, so Enter can clear it.
        for line in ["> ", ">", "> - "] {
            assert!(
                matches!(line_continuation(line), Item { empty: true, .. }),
                "{line}"
            );
        }
    }

    #[test]
    fn fence_marker_and_pairing() {
        // The marker is the indent plus the whole run; a language tag is not part
        // of it, and two delimiters are not a fence.
        assert_eq!(fence_marker("```"), Some("```"));
        assert_eq!(fence_marker("```rust"), Some("```"));
        assert_eq!(fence_marker("~~~"), Some("~~~"));
        assert_eq!(fence_marker("````js"), Some("````"));
        assert_eq!(fence_marker("  ```"), Some("  ```"));
        for line in ["``", "`` `", "~~text~~", "plain", ""] {
            assert_eq!(fence_marker(line), None, "{line}");
        }

        // An opener with nothing closing it is the one case that pairs.
        let p = parse(&Rope::from_str("a\n```rust\n"));
        assert_eq!(fence_to_close(&p, "```rust", 1), Some("```".into()));
        // An indented opener pairs at its own indent.
        let p = parse(&Rope::from_str("- item\n  ```\n"));
        assert_eq!(fence_to_close(&p, "  ```", 1), Some("  ```".into()));
        // A closed block: neither of its fences pairs.
        let p = parse(&Rope::from_str("```\ncode\n```\nb\n"));
        assert_eq!(fence_to_close(&p, "```", 0), None);
        assert_eq!(fence_to_close(&p, "```", 2), None);
        // A line *inside* an unclosed block leaves `in_fence` set too, so the
        // marker is what rules it out.
        let p = parse(&Rope::from_str("```\ncode\n"));
        assert_eq!(fence_to_close(&p, "code", 1), None);
        // Frontmatter is frontmatter: a fence inside it never opened one.
        let p = parse(&Rope::from_str("---\n```\n---\nb\n"));
        assert_eq!(fence_to_close(&p, "```", 1), None);
    }

    #[test]
    fn list_and_blockquote_markers() {
        let rope = Rope::from_str("- item\n> quote\n1. first\n");
        let spans = parse(&rope);
        assert!(spans[0].iter().any(|s| s.kind == SpanKind::ListItem));
        assert!(spans[1].iter().any(|s| s.kind == SpanKind::BlockQuote));
        assert!(spans[2].iter().any(|s| s.kind == SpanKind::ListItem));
    }

    #[test]
    fn emphasis_scans_and_conceals() {
        for line in ["a *em* b", "a _em_ b"] {
            let segs = flatten(line.len(), &parse(&Rope::from_str(line))[0]);
            assert_eq!(kind_at(&segs, 3), Some(SpanKind::Emphasis), "{line}");
            assert_eq!(kind_at(&segs, 2), Some(SpanKind::Marker), "{line}");
            let c = conceal(line, &segs);
            assert_eq!(c.text, "a em b", "{line}");
            assert_eq!(c.segments.iter().map(|s| s.len).sum::<usize>(), c.text.len());
        }
    }

    #[test]
    fn emphasis_delimiters_need_flanking_text() {
        // Each of these must produce no Emphasis span at all: a list bullet,
        // an intraword underscore, arithmetic, and a doubled delimiter.
        for line in ["* item", "snake_case_name", "2*3 * 4", "__bold__", "a * b"] {
            let spans = &parse(&Rope::from_str(line))[0];
            assert!(spans.iter().all(|s| s.kind != SpanKind::Emphasis), "{line}");
        }
        // `**` still wins over the single-char arm.
        let spans = &parse(&Rope::from_str("**b**"))[0];
        assert!(spans.iter().any(|s| s.kind == SpanKind::Strong));
        assert!(spans.iter().all(|s| s.kind != SpanKind::Emphasis));
    }

    #[test]
    fn unclosed_emphasis_stays_literal_and_does_not_carry() {
        // Unlike `**`/`~~`, a dangling `*` marks nothing — not even the rest
        // of its own line — and the next line is untouched.
        let spans = parse(&Rope::from_str("*open\nplain\n"));
        assert!(spans[0].iter().all(|s| s.kind != SpanKind::Emphasis));
        assert!(spans[1].iter().all(|s| s.kind != SpanKind::Emphasis));
    }

    #[test]
    fn two_stray_asterisks_on_one_line_do_emphasize() {
        // The accepted false positive, and what CommonMark does with the same
        // input: both delimiters clear the flanking test, so "3 and 4" goes
        // italic. Bounded to the line — no carry means it can't run on.
        let line = "2*3 and 4*5";
        let segs = flatten(line.len(), &parse(&Rope::from_str(line))[0]);
        assert_eq!(kind_at(&segs, 2), Some(SpanKind::Emphasis));
        // A closer touching whitespace on its inner side still can't close,
        // which is what keeps `2*3 * 4` and `*.rs and *.md` literal.
        for line in ["2*3 * 4", "*.rs and *.md"] {
            let spans = &parse(&Rope::from_str(line))[0];
            assert!(spans.iter().all(|s| s.kind != SpanKind::Emphasis), "{line}");
        }
    }

    #[test]
    fn conceal_keeps_a_bullet_but_drops_emphasis_stars() {
        // Both markers are the one-byte slice "*"; only the leading bullet
        // on a list line survives.
        let line = "* *em* x";
        let segs = flatten(line.len(), &parse(&Rope::from_str(line))[0]);
        assert_eq!(conceal(line, &segs).text, "* em x");
    }

    #[test]
    fn emphasis_composes_with_strike() {
        // `~~` resumes inside its own run, so the nested emphasis still scans
        // and carries the struck bit.
        let line = "~~*em*~~";
        let segs = flatten(line.len(), &parse(&Rope::from_str(line))[0]);
        assert_eq!(kind_at(&segs, 3), Some(SpanKind::Emphasis));
        assert!(seg_at(&segs, 3).struck);
    }
}
