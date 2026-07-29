//! The visual-row pipeline: each logical line becomes one or more
//! `LineElement` rows, sliced at soft-wrap boundaries. Row heights are a pure
//! function of each row's content (heading scale + top margin), tabulated
//! into the offsets table `RowList` places rows by.
//!
//! Four perf invariants hold this together; each one has cost real frame time
//! when broken:
//!
//! - Output is memoized against `RowsKey` — document revision, caret, mode,
//!   selection span, highlight query, wrap width. Scroll-only frames rebuild
//!   nothing. Anything new that changes row *content* belongs in that key.
//! - A caret-only key change patches at most six lines — the old and new
//!   cursor lines plus the fences their enclosing code blocks reveal — through
//!   `append_line_rows`, spliced into `RowsCache.line_rows`. Visual-mode
//!   sweeps and insert typing still take the full rebuild.
//! - Plain-ASCII monospace lines get their wrap boundaries from
//!   `wrap_columns`, a pure column walk with zero platform shaping. Only what
//!   changes glyph advances — non-ASCII, tabs, bold — falls back to real
//!   shaping, memoized in `ShapeWrapCache` by (text, segments). Decoration
//!   that leaves advances alone must stay off that list: strikethrough paints
//!   a rule and keeps the cheap walk.
//! - `LineElement::height` stays a function of content alone. `RowList`
//!   virtualizes from a prefix-sum offsets table and never lazily measures, so
//!   a height that depends on caret or scroll state invalidates the table.
//!
//! A child module of `editor` so methods can touch private `Editor` state.

use std::collections::HashMap;
use std::rc::Rc;

use gpui::{px, Font, Pixels, Window};

use crate::markdown::{self, line_text, Segment, SpanKind};
use crate::config::LineNumbers;
use crate::theme::Theme;
use crate::vim::Mode;

use super::{
    caret_bytes, fence_block, find_matches, heading_metrics, row_decor, run,
    search_sensitive, segments_to_runs, Editor, Gutter, Highlight, LineCaret, LineElement,
    RowDecor, CODE_MARGIN, CODE_PAD,
};

/// Everything the editor's row list is built from, beyond session constants
/// (font, theme, gutter mode). Equal keys ⇒ identical rows, so render reuses
/// the cached build. `revision` values are process-unique per content state,
/// so buffer switches and `:e` reloads can't collide.
#[derive(PartialEq)]
pub(super) struct RowsKey {
    pub(super) revision: u64,
    pub(super) caret: usize,
    pub(super) mode: Mode,
    /// View posture: nothing reveals, the caret renders on concealed text.
    pub(super) view: bool,
    /// Visual-mode selection span (`None` outside visual mode).
    pub(super) sel: Option<(usize, usize)>,
    /// The query whose matches are highlighted (incsearch preview or lit
    /// hlsearch); empty = none.
    pub(super) q: String,
    pub(super) wrap_width: Option<Pixels>,
}

/// The last row build: what it was built from, its rows, the caret's row,
/// and how many rows each logical line produced — the caret fast path uses
/// the per-line counts to splice single lines instead of rebuilding.
pub(super) struct RowsCache {
    pub(super) key: RowsKey,
    pub(super) rows: Rc<Vec<LineElement>>,
    pub(super) cur_row: usize,
    pub(super) line_rows: Vec<u32>,
    /// Each row's top edge, plus the total content height as the last entry
    /// (`len == rows.len() + 1`) — the `RowList` placement table, also what
    /// click/scroll math resolves y-coordinates against.
    pub(super) offsets: Rc<Vec<Pixels>>,
}

/// Prefix-sum y offsets for `rows` (see `RowsCache::offsets`). Rebuilt
/// whenever the rows do; scroll-only frames reuse the cached table.
// ponytail: O(rows) adds per caret move (the patch path rebuilds it whole);
// splice incrementally if a profile ever blames it.
pub(super) fn row_offsets(rows: &[LineElement], line_h: Pixels) -> Rc<Vec<Pixels>> {
    let mut offsets = Vec::with_capacity(rows.len() + 1);
    let mut y = Pixels::ZERO;
    offsets.push(y);
    for row in rows {
        y += row.height(line_h);
        offsets.push(y);
    }
    Rc::new(offsets)
}

/// Inputs to `append_line_rows` that are uniform across lines within one
/// build, bundled so the caret fast path can rebuild single lines without
/// rerunning a whole `build_rows` pass.
pub(super) struct RowCtx {
    pub(super) rope: ropey::Rope, // ropey clone is cheap (shared, CoW)
    pub(super) spans: Rc<Vec<Vec<markdown::Span>>>,
    pub(super) theme: Theme,
    pub(super) font: Font,
    pub(super) font_size: Pixels,
    /// Base (scale-1) row height; heading top margins are fractions of it.
    pub(super) line_h: Pixels,
    pub(super) wrap_width: Option<Pixels>,
    /// Columns per row when the font probed monospace; the plain-ASCII
    /// column-walk wrap path.
    pub(super) mono_cols: Option<usize>,
    /// `mono_cols` shrunk by the `CODE_MARGIN + CODE_PAD` text inset — the
    /// column budget for code-band lines, which wrap inside the band's border.
    pub(super) mono_band_cols: Option<usize>,
    pub(super) mode: Mode,
    /// View posture: no line reveals its source, the cursor line included.
    pub(super) view: bool,
    pub(super) cur_line: usize,
    pub(super) cur_col: usize,
    /// Fence lines of the block the caret sits in, revealed along with the
    /// cursor line so the whole block reads as one unit while edited inside.
    pub(super) reveal_fences: [Option<usize>; 2],
    pub(super) sel_span: Option<(usize, usize)>,
    pub(super) search_ranges: Vec<(usize, usize)>,
    pub(super) num_width: usize,
}

/// Wrap boundaries for lines that need real shaping (non-ASCII, tabs, bold
/// spans), cached across row rebuilds. gpui's own layout cache only survives
/// frame to frame — and memoized frames don't shape — so leaning on it meant
/// re-platform-shaping every such line on every caret move (visible j/k
/// stutter). Two generations, rotated once per *full* build: an entry unused
/// for one whole build is dropped. A width change clears everything, since
/// boundaries depend on it.
#[derive(Default)]
pub(super) struct ShapeWrapCache {
    width: Pixels,
    cur: HashMap<(String, Vec<Segment>), Vec<usize>>,
    prev: HashMap<(String, Vec<Segment>), Vec<usize>>,
}

impl ShapeWrapCache {
    /// Start a full build: rotate generations, or clear on width change.
    fn begin(&mut self, width: Pixels) {
        if width != self.width {
            self.width = width;
            self.cur.clear();
            self.prev.clear();
        } else {
            self.prev = std::mem::take(&mut self.cur);
        }
    }

    /// Look up boundaries, promoting a previous-generation hit.
    fn get(&mut self, key: &(String, Vec<Segment>)) -> Option<Vec<usize>> {
        if let Some(v) = self.cur.get(key) {
            return Some(v.clone());
        }
        let v = self.prev.remove(key)?;
        self.cur.insert(key.clone(), v.clone());
        Some(v)
    }
}

impl Editor {
    /// Assemble the per-build inputs shared by every line. `window` shapes
    /// the two one-glyph monospace probes.
    pub(super) fn row_ctx(&mut self, wrap_width: Option<Pixels>, theme: &Theme, window: &mut Window) -> RowCtx {
        let spans = self.spans();
        let rope = self.doc().rope.clone(); // ropey clone is cheap (shared, CoW)
        let mode = self.vim.mode;
        let (cur_line, cur_col) = self.doc().caret_line_col();
        // The selected char-range to highlight, `None` outside visual mode.
        let sel_span: Option<(usize, usize)> =
            mode.is_visual().then(|| self.doc().selection_span(mode == Mode::VisualLine));
        let q = self.search_query();
        let search_ranges = if q.is_empty() {
            Vec::new()
        } else {
            find_matches(&rope, &q, search_sensitive(&q, &self.search_cfg))
        };

        // Shaping font, matching what `LineElement` resolves from the window's
        // text-style cascade so wrap boundaries agree with the painted rows.
        let font = gpui::font(self.font_family.clone());
        let font_size = px(self.font_size);

        // Monospace fast path: when every glyph advances the same, wrap
        // boundaries are a pure column walk — no platform shaping, which is
        // what made opening/editing long-lined docs drag. Probe the font once
        // ('i' and 'M' advance alike ⇒ monospace); the per-line gate in
        // `append_line_rows` keeps the exact shaped path for anything the
        // walk can't promise.
        let mono: Option<(usize, usize)> = wrap_width.and_then(|w| {
            let advance = |s: &'static str| {
                let runs = [run(&font, 1, theme.foreground)];
                window.text_system().shape_line(s.into(), font_size, &runs, None).width
            };
            let (iw, mw) = (advance("i"), advance("M"));
            ((iw - mw).abs() < px(0.01) && iw > Pixels::ZERO).then(|| {
                let cols = |w: Pixels| ((w / iw) as usize).max(1);
                (cols(w), cols(w - (CODE_MARGIN + CODE_PAD) * 2.))
            })
        });

        let num_width = rope.len_lines().to_string().len().max(3);
        let reveal_fences = fence_block(&spans, cur_line);
        RowCtx {
            rope,
            spans,
            theme: *theme,
            font,
            font_size,
            line_h: self.line_h(),
            wrap_width,
            mono_cols: mono.map(|(c, _)| c),
            mono_band_cols: mono.map(|(_, c)| c),
            mode,
            view: self.vim.view,
            cur_line,
            cur_col,
            reveal_fences,
            sel_span,
            search_ranges,
            num_width,
        }
    }

    /// One `LineElement` per *visual row* of the buffer, the caret's row
    /// index, and each logical line's row count. With soft-wrap on, a logical
    /// line becomes one element per wrapped row, its display text, styling
    /// segments, highlights, and caret sliced to each row. `LineElement`
    /// stays a single row of known height (`LineElement::height`), which is
    /// what keeps `RowList`'s offset-table virtualization valid.
    ///
    /// Runs only when a `RowsKey` input changed beyond a caret move (render
    /// memoizes and caret moves patch single lines via `append_line_rows`).
    // ponytail: a full rebuild walks the whole doc (conceal + slice, ≈ per
    // edit keystroke). If typing in a huge doc ever bites, cache rows per
    // line keyed on (text, segments) like `ShapeWrapCache`.
    pub(super) fn build_rows(
        &mut self,
        ctx: &RowCtx,
        window: &mut Window,
    ) -> (Vec<LineElement>, usize, Vec<u32>) {
        // Full build = one cache generation for the shaped wrap boundaries.
        self.wrap_cache.begin(ctx.wrap_width.unwrap_or(Pixels::ZERO));
        let line_count = ctx.rope.len_lines();
        let mut rows = Vec::with_capacity(line_count);
        let mut line_rows = Vec::with_capacity(line_count);
        let mut cur_row = 0;
        for i in 0..line_count {
            let base = rows.len();
            if let Some(k) = self.append_line_rows(ctx, i, window, &mut rows) {
                cur_row = base + k;
            }
            line_rows.push((rows.len() - base) as u32);
        }
        (rows, cur_row, line_rows)
    }

    /// Build logical line `i`'s visual rows into `out`, returning the caret's
    /// index within the appended rows when `i` is the cursor line. The whole
    /// per-line pipeline lives here so the caret fast path can redo exactly
    /// the lines that changed.
    pub(super) fn append_line_rows(
        &mut self,
        ctx: &RowCtx,
        i: usize,
        window: &mut Window,
        out: &mut Vec<LineElement>,
    ) -> Option<usize> {
        let base = out.len();
        let mut caret_at = None;
        // Conceal markers on every line but the cursor line, which keeps
        // full source so caret math stays on real document bytes.
        let text = line_text(&ctx.rope, i);
        let line_spans = ctx.spans.get(i).map_or(&[][..], Vec::as_slice);
        let segs = markdown::flatten(text.len(), line_spans);
        // Highlights land in source columns; a concealed line remaps them
        // through the conceal map so they track the display text.
        let selection = ctx.sel_span.and_then(|(lo, hi)| line_highlight(&ctx.rope, i, lo, hi));
        let search: Vec<Highlight> = ctx
            .search_ranges
            .iter()
            .filter_map(|&(lo, hi)| line_highlight(&ctx.rope, i, lo, hi))
            .collect();
        // Heading lines shape larger in a taller row, the revealed cursor
        // line included — its source segments carry `Heading` too, so both
        // branches agree and row heights never change on a caret move (no
        // layout shift as j/k crosses an H1). Scale/pad are pure functions
        // of the segments the wrap cache keys on, so cached boundaries stay
        // consistent. Markdown rendering off is all body size.
        let revealed = !ctx.view && (i == ctx.cur_line || ctx.reveal_fences.contains(&Some(i)));
        let mut cur_col = ctx.cur_col;
        let (text, segments, selection, search, (scale, pad), decor) = if self.render_markdown
            && !revealed
        {
            let c = markdown::conceal(&text, &segs);
            // View posture is the one way the *cursor* line renders
            // concealed; its source caret column maps onto the display text
            // like any highlight (source col → source byte → display byte →
            // display char col).
            if i == ctx.cur_line {
                let d = c.map[caret_bytes(&text, cur_col).0];
                cur_col = c.text[..d].chars().count();
            }
            let selection = selection.and_then(|h| remap_highlight(h, &text, &c));
            let search =
                search.into_iter().filter_map(|h| remap_highlight(h, &text, &c)).collect();
            let metrics = heading_metrics(&c.segments);
            (c.text, c.segments, selection, search, metrics, row_decor(&ctx.spans, i))
        } else {
            // Source view (revealed line, or markdown rendering off): a task
            // box's `[ ]` shows its source bytes styled like any other marker
            // instead of the transparent box span. The code band is the one
            // decoration that survives reveal — code text isn't concealed
            // anyway, and the block should read as one unit while edited.
            let metrics =
                if self.render_markdown { heading_metrics(&segs) } else { (1.0, 0.0) };
            let segs = segs
                .into_iter()
                .map(|s| match s.kind {
                    Some(SpanKind::Task(_)) => Segment { kind: Some(SpanKind::Marker), ..s },
                    _ => s,
                })
                .collect();
            let decor = self
                .render_markdown
                .then(|| row_decor(&ctx.spans, i))
                .flatten()
                .filter(|d| matches!(d, RowDecor::CodeBand { .. }));
            (text, segs, selection, search, metrics, decor)
        };
        // Breathing room above a heading — on the line's first visual row
        // only; wrapped continuation rows keep just the scaled box.
        let pad_top = (ctx.line_h * pad).round();

        // Byte offset where each visual row starts: 0, plus one per wrap
        // boundary (the boundary glyph opens the next row). Plain ASCII
        // lines in a monospace font take the column walk; anything the
        // walk can't promise — non-ASCII (fallback fonts, wide glyphs),
        // tabs, bold spans (a family's bold could differ) — shapes for
        // exact boundaries, cached in `wrap_cache` across rebuilds.
        let row_starts: Vec<usize> = match ctx.wrap_width {
            None => vec![0],
            Some(w) => {
                // Code-band text is inset by CODE_MARGIN + CODE_PAD per side
                // (paint shifts it right); wrap inside the inset width — a
                // reduced column budget on the mono walk, a reduced pixel
                // width when shaping — so wrapped rows stay clear of the
                // band's right border.
                let band = matches!(decor, Some(RowDecor::CodeBand { .. }));
                let w = if band { w - (CODE_MARGIN + CODE_PAD) * 2. } else { w };
                let plain = text.is_ascii()
                    && !text.contains('\t')
                    && segments.iter().all(|s| {
                        !matches!(s.kind, Some(SpanKind::Heading(_)) | Some(SpanKind::Strong))
                    });
                match if band { ctx.mono_band_cols } else { ctx.mono_cols } {
                    Some(cols) if plain => wrap_columns(&text, cols),
                    _ => {
                        let key = (text.clone(), segments.clone());
                        match self.wrap_cache.get(&key) {
                            Some(starts) => starts,
                            None => {
                                let runs = segments_to_runs(
                                    &text,
                                    &segments,
                                    &ctx.font,
                                    ctx.theme.foreground,
                                    &ctx.theme,
                                );
                                let wrapped = window
                                    .text_system()
                                    .shape_text(
                                        text.clone().into(),
                                        ctx.font_size * scale,
                                        &runs,
                                        Some(w),
                                        None,
                                    )
                                    .ok()
                                    .and_then(|lines| lines.into_iter().next());
                                let mut starts = vec![0];
                                if let Some(wl) = wrapped {
                                    starts.extend(wl.wrap_boundaries.iter().map(|b| {
                                        wl.unwrapped_layout.runs[b.run_ix].glyphs[b.glyph_ix]
                                            .index
                                    }));
                                }
                                self.wrap_cache.cur.insert(key, starts.clone());
                                starts
                            }
                        }
                    }
                }
            }
        };
        // Char col where each row starts — highlight and caret columns
        // are char-based, byte offsets index the text slices. ASCII:
        // bytes are cols. Otherwise one pass over the char boundaries
        // (per-row `chars().count()` was quadratic on long lines).
        let (row_cols, line_chars): (Vec<usize>, usize) = if text.is_ascii() {
            (row_starts.clone(), text.len())
        } else {
            let mut cols = Vec::with_capacity(row_starts.len());
            let mut chars = 0;
            let mut ci = text.char_indices().peekable();
            for &b in &row_starts {
                while ci.next_if(|&(cb, _)| cb < b).is_some() {
                    chars += 1;
                }
                cols.push(chars);
            }
            (cols, text.chars().count())
        };
        let last = row_starts.len() - 1;
        // The caret's row: the last row starting at or before its byte (a
        // byte on a boundary belongs to the row the boundary opens).
        let caret_row = (i == ctx.cur_line).then(|| {
            let byte = caret_bytes(&text, cur_col).0;
            row_starts.partition_point(|&b| b <= byte) - 1
        });

        for k in 0..=last {
            let b0 = row_starts[k];
            let b1 = row_starts.get(k + 1).copied().unwrap_or(text.len());
            let c0 = row_cols[k];
            let c1 = row_cols.get(k + 1).copied().unwrap_or(line_chars);
            if caret_row == Some(k) {
                caret_at = Some(out.len() - base);
            }
            // The gutter carries its inputs, not a formatted label — the label
            // resolves in `prepaint` against the shared cursor line, so a caret
            // move never invalidates a cached row (see `Gutter`).
            let gutter = (self.line_numbers != LineNumbers::Off).then(|| Gutter {
                line: i,
                continuation: k > 0,
                width: ctx.num_width,
                relative: self.line_numbers == LineNumbers::Relative,
                cur_line: self.cur_line.clone(),
            });
            out.push(LineElement {
                text: text[b0..b1].to_string().into(),
                segments: slice_segments(&segments, b0, b1),
                caret: (caret_row == Some(k)).then(|| LineCaret {
                    col: cur_col - c0,
                    block: ctx.mode != Mode::Insert,
                }),
                selection: selection.and_then(|h| clip_row_highlight(h, c0, c1, k == last)),
                search: search
                    .iter()
                    .filter_map(|&h| clip_row_highlight(h, c0, c1, k == last))
                    .collect(),
                scroll_x: self.scroll_x.clone(),
                caret_paint: self.caret_paint.clone(),
                gutter,
                follow_h: ctx.wrap_width.is_none(),
                scale,
                pad_top: if k == 0 { pad_top } else { Pixels::ZERO },
                // A wrapped band line closes its border only on its outermost
                // visual rows; middle rows keep the sides running through.
                decor: match decor {
                    Some(RowDecor::CodeBand { top, bottom }) => Some(RowDecor::CodeBand {
                        top: top && k == 0,
                        bottom: bottom && k == last,
                    }),
                    d => d,
                },
            });
        }
        caret_at
    }
}

/// Which columns of line `i` fall inside the selection char-range `[lo, hi)`.
/// `to_eol` is set when the range reaches into this line's newline, so the
/// highlight should fill past the last char (selected blank space / joined line).
fn line_highlight(rope: &ropey::Rope, i: usize, lo: usize, hi: usize) -> Option<Highlight> {
    let line_start = rope.line_to_char(i);
    let line = rope.line(i);
    let total = line.len_chars(); // includes a trailing '\n' if present
    // Indexed, not iterated: this runs once per selection span and once per
    // search match per line, and `Chars::last()` walks the whole line.
    let content = if total > 0 && line.char(total - 1) == '\n' { total - 1 } else { total };
    let a = lo.max(line_start);
    let b = hi.min(line_start + total); // clamp to past-the-newline
    if a >= b {
        return None;
    }
    Some(Highlight {
        start_col: a - line_start,
        end_col: (b - line_start).min(content),
        to_eol: b > line_start + content,
    })
}

/// The only difference between two row keys is where the caret sits: same
/// content, same mode, no selection, no highlighted search. Then only the old
/// and new cursor lines can render differently (conceal swap, caret quad,
/// gutter emphasis), so the cached rows can be patched instead of rebuilt.
pub(super) fn caret_only_change(old: &RowsKey, new: &RowsKey) -> bool {
    old.revision == new.revision
        && old.wrap_width == new.wrap_width
        && old.mode == new.mode
        && old.view == new.view
        && old.sel.is_none()
        && new.sel.is_none()
        && old.q.is_empty()
        && new.q.is_empty()
}

/// Greedy word wrap for plain ASCII text in a monospace font: row-start byte
/// offsets for rows of at most `cols` chars, breaking after the last space in
/// the row, or mid-word when one word overruns a whole row. Only valid where
/// byte == char == column — the caller gates on ASCII.
// ponytail: a break can leave a space at a row edge — cosmetic, vim-like.
fn wrap_columns(text: &str, cols: usize) -> Vec<usize> {
    let cols = cols.max(1);
    let bytes = text.as_bytes();
    let mut starts = vec![0];
    let (mut row_start, mut last_space) = (0usize, None);
    let mut i = 0;
    while i < bytes.len() {
        if i - row_start == cols {
            let next = last_space.map_or(i, |s: usize| s + 1);
            starts.push(next);
            row_start = next;
            last_space = None;
            i = next;
            continue;
        }
        if bytes[i] == b' ' {
            last_space = Some(i);
        }
        i += 1;
    }
    starts
}

/// Slice a line's styling segments down to the byte range `[b0, b1)` of one
/// wrapped visual row. Kinds and decoration are kept; lengths clip to the range.
fn slice_segments(segments: &[Segment], b0: usize, b1: usize) -> Vec<Segment> {
    let mut out = Vec::new();
    let mut pos = 0;
    for seg in segments {
        let (s, e) = (pos, pos + seg.len);
        pos = e;
        let (a, b) = (s.max(b0), e.min(b1));
        if a < b {
            out.push(Segment { len: b - a, ..*seg });
        }
    }
    out
}

/// Clip a line-level column highlight to one visual row's char range
/// `[c0, c1)`, re-based to row-local columns. `to_eol` (fill past the last
/// char) only survives on the line's last row, where the line actually ends;
/// `None` when nothing of the span lands on this row.
fn clip_row_highlight(h: Highlight, c0: usize, c1: usize, last_row: bool) -> Option<Highlight> {
    let to_eol = h.to_eol && last_row;
    let a = h.start_col.max(c0);
    let b = h.end_col.min(c1).max(a);
    if a >= b && !to_eol {
        return None;
    }
    Some(Highlight { start_col: a - c0, end_col: b - c0, to_eol })
}

/// Remap a source-column highlight onto a concealed line: char col → source
/// byte, through the conceal map, → display char col. `None` when the span was
/// entirely concealed away (nothing visible to highlight).
fn remap_highlight(h: Highlight, source: &str, c: &markdown::Concealed) -> Option<Highlight> {
    let d0 = c.map[caret_bytes(source, h.start_col).0];
    let d1 = c.map[caret_bytes(source, h.end_col).0];
    if d0 >= d1 && !h.to_eol {
        return None;
    }
    Some(Highlight {
        start_col: c.text[..d0].chars().count(),
        end_col: c.text[..d1].chars().count(),
        to_eol: h.to_eol,
    })
}

#[cfg(test)]
mod tests {
    use super::{clip_row_highlight, remap_highlight, slice_segments, wrap_columns, Highlight};
    use crate::markdown::{self, SpanKind};
    use ropey::Rope;

    #[test]
    fn wrap_columns_breaks_words_and_walls() {
        // Word break: "hello worl|d…" overflows at 10; the row breaks after
        // the space, so row 2 starts at 'w'.
        assert_eq!(wrap_columns("hello world foo", 10), vec![0, 6]);
        // No spaces: hard breaks every `cols`.
        assert_eq!(wrap_columns("aaaaaaaaaaaa", 5), vec![0, 5, 10]);
        // Exact fit and empty: single row.
        assert_eq!(wrap_columns("aaaaa", 5), vec![0]);
        assert_eq!(wrap_columns("", 5), vec![0]);
        // cols 0 clamps to 1 instead of looping forever.
        assert_eq!(wrap_columns("ab", 0), vec![0, 1]);
    }

    #[test]
    fn slice_segments_clips_to_row_range() {
        use markdown::Segment;
        let segs = vec![
            Segment { len: 3, kind: Some(SpanKind::Marker), struck: false },
            Segment { len: 5, kind: None, struck: true },
        ];
        // Row [2, 6): one byte of the marker, three of the body. A clipped
        // segment keeps its decoration, so strike survives a wrap boundary.
        assert_eq!(
            slice_segments(&segs, 2, 6),
            vec![
                Segment { len: 1, kind: Some(SpanKind::Marker), struck: false },
                Segment { len: 3, kind: None, struck: true },
            ]
        );
        // A row past the segments' end is unstyled.
        assert!(slice_segments(&segs, 8, 12).is_empty());
    }

    #[test]
    fn clip_row_highlight_splits_across_rows() {
        // A line wrapped at col 10; selection [4, 14) reaching the newline.
        let h = Highlight { start_col: 4, end_col: 14, to_eol: true };
        // First row [0,10): local [4,10), not at line end → no to_eol fill.
        assert!(matches!(
            clip_row_highlight(h, 0, 10, false),
            Some(Highlight { start_col: 4, end_col: 10, to_eol: false })
        ));
        // Last row [10,16): local [0,4), keeps the to_eol fill.
        assert!(matches!(
            clip_row_highlight(h, 10, 16, true),
            Some(Highlight { start_col: 0, end_col: 4, to_eol: true })
        ));
        // A span the row misses entirely.
        let short = Highlight { start_col: 0, end_col: 3, to_eol: false };
        assert!(clip_row_highlight(short, 10, 16, true).is_none());
        // Zero-width span at EOL survives through to_eol (linewise selection
        // covering a wrapped line's newline).
        let eol = Highlight { start_col: 14, end_col: 14, to_eol: true };
        assert!(matches!(
            clip_row_highlight(eol, 10, 16, true),
            Some(Highlight { start_col: 4, end_col: 4, to_eol: true })
        ));
    }

    #[test]
    fn highlight_remaps_onto_concealed_text() {
        let line = "**templates** more";
        let segs = markdown::flatten(line.len(), &markdown::parse(&Rope::from_str(line))[0]);
        let c = markdown::conceal(line, &segs);
        assert_eq!(c.text, "templates more");
        // "templates" sits at source cols 2..11; concealed it starts the line.
        let h = Highlight { start_col: 2, end_col: 11, to_eol: false };
        let r = remap_highlight(h, line, &c).unwrap();
        assert_eq!((r.start_col, r.end_col), (0, 9));
        // A span entirely inside a dropped marker has nothing visible left.
        let h = Highlight { start_col: 0, end_col: 2, to_eol: false };
        assert!(remap_highlight(h, line, &c).is_none());

        // Display columns are chars, not bytes, on multibyte lines.
        let line = "**é** x";
        let segs = markdown::flatten(line.len(), &markdown::parse(&Rope::from_str(line))[0]);
        let c = markdown::conceal(line, &segs);
        assert_eq!(c.text, "é x");
        let h = Highlight { start_col: 2, end_col: 3, to_eol: false }; // the é
        let r = remap_highlight(h, line, &c).unwrap();
        assert_eq!((r.start_col, r.end_col), (0, 1));
    }
}
