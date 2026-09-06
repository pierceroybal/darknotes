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
//!   `append_line_rows`, spliced into `RowsCache.line_rows`. A *one-line edit*
//!   patches the same way: `Editor::spans_for_render` reparses the single line
//!   and says so, which is what keeps insert-mode typing off the full rebuild.
//!   Anything wider — a multi-line edit, an undo, a fence boundary moving —
//!   still rebuilds everything.
//! - A full rebuild reuses per-line work through `LineForms`: text, segments,
//!   heading metrics, decoration, and wrap boundaries are cached per line, so a
//!   rebuild driven by a selection sweep or a search keystroke re-derives only
//!   the lines that actually carry a highlight. Nothing caret- or
//!   selection-dependent may enter that cache.
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
//! Folding rides on the same structure: a line hidden by a closed fold emits
//! zero rows, which every row↔line walk in the editor already skips. Its header
//! renders as the heading it is — the fold shows only in the gutter, resolved
//! at paint time — so nothing about a fold reaches the row caches.
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
    caret_bytes, fence_block, heading_metrics, row_decor, run, segments_to_runs, Editor,
    Gutter, Highlight, LineCaret, LineElement, RowDecor, CODE_MARGIN, CODE_PAD, QUOTE_PAD,
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
    /// `Document::folds_gen`. A fold toggle moves neither the caret nor the
    /// content, so without it `za` would render as a cache hit and nothing
    /// would happen on screen.
    pub(super) folds: u64,
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
    pub(super) spans: Rc<markdown::Parsed>,
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
    /// `mono_cols` shrunk by `QUOTE_PAD` — the budget for blockquote lines,
    /// whose text is inset past the painted bar.
    pub(super) mono_quote_cols: Option<usize>,
    pub(super) mode: Mode,
    /// View posture: no line reveals its source, the cursor line included.
    pub(super) view: bool,
    pub(super) cur_line: usize,
    pub(super) cur_col: usize,
    /// Fence lines of the block the caret sits in, revealed along with the
    /// cursor line so the whole block reads as one unit while edited inside.
    pub(super) reveal_fences: [Option<usize>; 2],
    pub(super) sel_span: Option<(usize, usize)>,
    /// Logical line range `sel_span` covers, inclusive — so a line can rule
    /// itself out of the selection without touching the rope. Derived once per
    /// build; a two-line selection otherwise cost a rope walk per document line.
    pub(super) sel_lines: Option<(usize, usize)>,
    /// Match ranges in ascending order, as `find_matches` returns them. Both
    /// bounds ascend (every match is the query's length), which is what lets a
    /// line binary-search its own slice of them.
    pub(super) search_ranges: Rc<Vec<(usize, usize)>>,
    pub(super) num_width: usize,
    /// Closed folds as inclusive `(header, last_hidden)` line ranges, sorted
    /// and disjoint — see `Document::folds`.
    pub(super) folds: Vec<(usize, usize)>,
}

impl RowCtx {
    /// The closed fold covering `line`, if any. Ranges are sorted and disjoint,
    /// so the last one starting at or before `line` is the only candidate.
    pub(super) fn fold_at(&self, line: usize) -> Option<(usize, usize)> {
        let i = self.folds.partition_point(|&(s, _)| s <= line).checked_sub(1)?;
        let (start, end) = self.folds[i];
        (line <= end).then_some((start, end))
    }

    /// `line`'s fold header, or `line` itself when no closed fold covers it.
    pub(super) fn fold_start(&self, line: usize) -> usize {
        self.fold_at(line).map_or(line, |(start, _)| start)
    }

    fn display_index(&self, line: usize) -> usize {
        display_index(&self.folds, line)
    }
}

/// `line`'s index in the fold-collapsed sequence — what relative line numbers
/// count in, so a closed fold counts once and a line it hides shares its
/// header's index. Folds are few and sorted, so a walk beats carrying a
/// per-line table.
pub(super) fn display_index(folds: &[(usize, usize)], line: usize) -> usize {
    let hidden: usize = folds
        .iter()
        .take_while(|&&(start, _)| start < line)
        .map(|&(start, end)| end.min(line) - start)
        .sum();
    line - hidden
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
    cur: HashMap<WrapKey, Vec<usize>>,
    prev: HashMap<WrapKey, Vec<usize>>,
}

/// Display text, its segments, and the row's text-inset class (0 none, 1 code
/// band, 2 quote). The inset is part of the key because the same text at two
/// insets wraps at two different widths — `> **bold**` and `**bold**` conceal
/// to identical text and segments.
type WrapKey = (String, Vec<Segment>, u8);

/// Everything about a row build that depends only on the line's text and spans:
/// its concealed display form, heading metrics, block decoration, and wrap
/// boundaries. Deliberately excludes anything caret-, selection-, or
/// search-dependent, which is what makes it reusable across those changes.
pub(super) struct LineForm {
    text: String,
    segments: Vec<Segment>,
    metrics: (f32, f32),
    decor: Option<RowDecor>,
    row_starts: Vec<usize>,
}

/// `LineForm` per logical line, so a rebuild driven by something other than
/// content — entering visual mode, dragging a selection, typing a search query,
/// an undo, a multi-line edit — skips re-flattening and re-concealing the whole
/// document. Those all still take `Plan::Full`, and before this they paid the
/// per-line pipeline for every line each time.
///
/// Valid for one `(revision, wrap width)` pair. A width change invalidates
/// everything, since boundaries depend on it — so this does *not* help a resize.
/// A one-line edit invalidates one entry, on the same proof `spans_for_render`
/// uses (`Document::single_line_edit` against *this* cache's revision, so a
/// cache that fell more than one edit behind is discarded rather than trusted).
#[derive(Default)]
pub(super) struct LineForms {
    revision: u64,
    wrap_width: Option<Pixels>,
    forms: Vec<Option<LineForm>>,
}

impl LineForms {
    /// The generation this cache holds, for the caller's staleness proof.
    pub(super) fn revision(&self) -> u64 {
        self.revision
    }

    /// Ready the cache for a build, keeping whatever stays valid: everything but
    /// `dirty` when the caller proved the change was confined to that line,
    /// nothing when the revision moved otherwise or the width changed.
    pub(super) fn begin(
        &mut self,
        revision: u64,
        width: Option<Pixels>,
        lines: usize,
        dirty: Option<usize>,
    ) {
        let keep = self.wrap_width == width
            && self.forms.len() == lines
            && (dirty.is_some() || self.revision == revision);
        if keep {
            if let Some(line) = dirty {
                // The neighbours go too: a code band's `top`/`bottom` corners
                // come from whether the *adjacent* lines are in-band, so an edit
                // that changes this line's role restyles the rows either side.
                // Three entries instead of one, and no subtle argument to get
                // wrong later.
                for l in line.saturating_sub(1)..=line + 1 {
                    if let Some(slot) = self.forms.get_mut(l) {
                        *slot = None;
                    }
                }
            }
        } else {
            self.forms.clear();
            self.forms.resize_with(lines, || None);
        }
        self.revision = revision;
        self.wrap_width = width;
    }

    /// Move line `i`'s form out for use. Taken rather than borrowed so the
    /// caller can hold it while touching other caches; `put` returns it.
    fn take(&mut self, i: usize) -> Option<LineForm> {
        self.forms.get_mut(i)?.take()
    }

    fn put(&mut self, i: usize, form: LineForm) {
        if let Some(slot) = self.forms.get_mut(i) {
            *slot = Some(form);
        }
    }
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
    fn get(&mut self, key: &WrapKey) -> Option<Vec<usize>> {
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
    /// the two one-glyph monospace probes. `edited_line` is
    /// `Editor::spans_for_render`'s verdict — `Some` only when the reparse
    /// stayed local, which is the same proof the per-line form cache needs.
    pub(super) fn row_ctx(
        &mut self,
        wrap_width: Option<Pixels>,
        theme: &Theme,
        window: &mut Window,
        edited_line: Option<usize>,
    ) -> RowCtx {
        let spans = self.spans();
        let rope = self.doc().rope.clone(); // ropey clone is cheap (shared, CoW)
        let mode = self.vim.mode;
        let (cur_line, cur_col) = self.doc().caret_line_col();
        // The selected char-range to highlight, `None` outside visual mode.
        let sel_span: Option<(usize, usize)> =
            mode.is_visual().then(|| self.doc().selection_span(mode == Mode::VisualLine));
        // Its line span, so lines outside it skip the per-line rope walk. `end`
        // is exclusive, so its line may sit one past the selection; that only
        // means one extra line runs the check, which then finds nothing.
        let sel_lines = sel_span.map(|(lo, hi)| {
            let len = rope.len_chars();
            (rope.char_to_line(lo.min(len)), rope.char_to_line(hi.min(len)))
        });
        let q = self.search_query();
        // Shared with the incsearch jump through the memo — see `search_matches`.
        let search_ranges = if q.is_empty() {
            Rc::new(Vec::new())
        } else {
            self.search_matches(&q)
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
        let mono: Option<(usize, usize, usize)> = wrap_width.and_then(|w| {
            let advance = |s: &'static str| {
                let runs = [run(&font, 1, theme.foreground)];
                window.text_system().shape_line(s.into(), font_size, &runs, None).width
            };
            let (iw, mw) = (advance("i"), advance("M"));
            ((iw - mw).abs() < px(0.01) && iw > Pixels::ZERO).then(|| {
                let cols = |w: Pixels| ((w / iw) as usize).max(1);
                (cols(w), cols(w - (CODE_MARGIN + CODE_PAD) * 2.), cols(w - QUOTE_PAD))
            })
        });

        let num_width = rope.len_lines().to_string().len().max(3);
        let folds = self.doc().folds().to_vec();
        let reveal_fences = fence_block(&spans, cur_line);
        // Ready the per-line form cache for this build. Both proofs are needed:
        // `edited_line` says the *parse* stayed local (a typed ``` moves a fence
        // boundary and restyles every line below it), and `single_line_edit`
        // says this cache sits at the revision immediately before that edit —
        // the two caches can be at different revisions, since a click hit-test
        // or a fold op advances the parse alone.
        let revision = self.doc().revision();
        let forms_dirty = self
            .doc()
            .single_line_edit(self.line_forms.revision())
            .map(|e| e.line)
            .filter(|&l| edited_line == Some(l));
        self.line_forms.begin(revision, wrap_width, rope.len_lines(), forms_dirty);
        RowCtx {
            rope,
            spans,
            theme: *theme,
            font,
            font_size,
            line_h: self.line_h(),
            wrap_width,
            mono_cols: mono.map(|(c, ..)| c),
            mono_band_cols: mono.map(|(_, c, _)| c),
            mono_quote_cols: mono.map(|(.., c)| c),
            mode,
            view: self.vim.view,
            cur_line,
            cur_col,
            reveal_fences,
            sel_span,
            sel_lines,
            search_ranges,
            num_width,
            folds,
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
        // A line hidden by a closed fold emits no rows at all, so
        // `line_rows[i] == 0`. Every row↔line walk in the editor already
        // advances past zero-row lines, and `sum(line_rows[..line])` maps a
        // hidden line onto its header's row — both directions round-trip
        // untouched. The header renders below like any other heading; the
        // gutter's `▸` is the only thing that marks it.
        let fold = ctx.fold_at(i);
        if fold.is_some_and(|(start, _)| i > start) {
            return None;
        }
        // A caret parked inside this fold — incsearch preview drags it there
        // while typing — has no row of its own, so the header row answers for
        // it. Without this `cur_row` keeps its initial 0 and the view snaps to
        // the top of the document. Both exits below report it.
        let caret_in_fold =
            fold.filter(|&(start, end)| (start..=end).contains(&ctx.cur_line)).map(|_| 0);

        // Order matters here: everything the cache check needs is computed
        // first, and it is all O(1)-ish. `line_text` and `flatten` sit *below*
        // the check, because they are the two most expensive steps and a cache
        // hit must not pay for them.
        let revealed = !ctx.view && (i == ctx.cur_line || ctx.reveal_fences.contains(&Some(i)));
        // Highlights land in source columns; a concealed line remaps them
        // through the conceal map so they track the display text.
        //
        // Both are gated on the lines they can actually touch. Testing every
        // line meant a two-line visual selection walked the rope once per line
        // of the document, and an hlsearch query cost O(lines × matches).
        let selection = ctx
            .sel_lines
            .filter(|&(first, last)| i >= first && i <= last)
            .and(ctx.sel_span)
            .and_then(|(lo, hi)| line_highlight(&ctx.rope, i, lo, hi));
        let search: Vec<Highlight> = if ctx.search_ranges.is_empty() {
            Vec::new()
        } else {
            let start = ctx.rope.line_to_char(i);
            let end = start + ctx.rope.line(i).len_chars();
            ranges_touching(&ctx.search_ranges, start, end)
                .iter()
                .filter_map(|&(lo, hi)| line_highlight(&ctx.rope, i, lo, hi))
                .collect()
        };
        // A line whose form is reusable: it renders concealed, and nothing about
        // it depends on the caret, the selection, or a search hit. That holds for
        // all but a handful of lines in any build, which is what makes a visual
        // sweep or a search keystroke cheap despite taking `Plan::Full`.
        let cacheable = self.render_markdown
            && !revealed
            && i != ctx.cur_line
            && selection.is_none()
            && search.is_empty();
        if let Some(form) = cacheable.then(|| self.line_forms.take(i)).flatten() {
            let LineForm { text, segments, metrics: (scale, pad), decor, row_starts } = form;
            let pad_top = (ctx.line_h * pad).round();
            self.emit_rows(
                ctx, i, &text, &segments, &row_starts, None, &[], None, scale, pad_top, decor,
                out,
            );
            self.line_forms.put(
                i,
                LineForm { text, segments, metrics: (scale, pad), decor, row_starts },
            );
            // Never the cursor line, so the only caret it can own is one hidden
            // inside the fold it heads.
            return caret_in_fold;
        }

        // Cache miss (or an uncacheable line): the full per-line pipeline.
        // Conceal markers on every line but the cursor line, which keeps
        // full source so caret math stays on real document bytes.
        let text = line_text(&ctx.rope, i);
        let line_spans = ctx.spans.get(i).map_or(&[][..], Vec::as_slice);
        let segs = markdown::flatten(text.len(), line_spans);
        let mut cur_col = ctx.cur_col;
        // Heading lines shape larger in a taller row, the revealed cursor
        // line included — its source segments carry `Heading` too, so both
        // branches agree and row heights never change on a caret move (no
        // layout shift as j/k crosses an H1). Scale/pad are pure functions
        // of the segments the wrap cache keys on, so cached boundaries stay
        // consistent. Markdown rendering off is all body size.
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
                // A decorated row's text is inset by paint — a code band by
                // CODE_MARGIN + CODE_PAD per side, a quote by QUOTE_PAD on the
                // left — so wrap inside the inset width: a reduced column
                // budget on the mono walk, a reduced pixel width when shaping.
                // Otherwise wrapped rows spill past the band's right border or
                // the pane edge.
                let (w, mono_cols, inset) = match decor {
                    Some(RowDecor::CodeBand { .. }) => {
                        (w - (CODE_MARGIN + CODE_PAD) * 2., ctx.mono_band_cols, 1)
                    }
                    Some(RowDecor::QuoteBar) => (w - QUOTE_PAD, ctx.mono_quote_cols, 2),
                    _ => (w, ctx.mono_cols, 0),
                };
                let plain = text.is_ascii()
                    && !text.contains('\t')
                    && segments.iter().all(|s| {
                        !matches!(
                            s.kind,
                            Some(SpanKind::Heading(_) | SpanKind::Strong | SpanKind::Emphasis)
                        )
                    });
                match mono_cols {
                    Some(cols) if plain => wrap_columns(&text, cols),
                    _ => {
                        let key = (text.clone(), segments.clone(), inset);
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
        let caret_at = self.emit_rows(
            ctx,
            i,
            &text,
            &segments,
            &row_starts,
            selection,
            &search,
            (i == ctx.cur_line).then_some(cur_col),
            scale,
            pad_top,
            decor,
            out,
        );
        // Only the caret/selection-independent lines are worth keeping, and
        // only they are safe to: a cached form carries no highlight or caret.
        if cacheable {
            self.line_forms
                .put(i, LineForm { text, segments, metrics: (scale, pad), decor, row_starts });
        }
        // `caret_at` already holds the real row when the caret is on the header
        // itself; otherwise the header answers for a caret hidden in its fold.
        caret_at.or(caret_in_fold)
    }

    /// Slice one line's display form into visual rows and push them onto `out`,
    /// returning the caret's index among them when `cur_col` places it here.
    ///
    /// Split out so the `LineForms` fast path and the full pipeline emit rows
    /// identically — the row geometry is the part that must not drift between
    /// them.
    #[allow(clippy::too_many_arguments)]
    fn emit_rows(
        &self,
        ctx: &RowCtx,
        i: usize,
        text: &str,
        segments: &[Segment],
        row_starts: &[usize],
        selection: Option<Highlight>,
        search: &[Highlight],
        // `Some` only on the cursor line, in this line's display coordinates.
        cur_col: Option<usize>,
        scale: f32,
        pad_top: Pixels,
        decor: Option<RowDecor>,
        out: &mut Vec<LineElement>,
    ) -> Option<usize> {
        let base = out.len();
        let mut caret_at = None;
        // Char col where each row starts — highlight and caret columns
        // are char-based, byte offsets index the text slices. ASCII:
        // bytes are cols. Otherwise one pass over the char boundaries
        // (per-row `chars().count()` was quadratic on long lines).
        let (row_cols, line_chars): (Vec<usize>, usize) = if text.is_ascii() {
            (row_starts.to_vec(), text.len())
        } else {
            let mut cols = Vec::with_capacity(row_starts.len());
            let mut chars = 0;
            let mut ci = text.char_indices().peekable();
            for &b in row_starts {
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
        let caret_row = cur_col.map(|col| {
            let byte = caret_bytes(text, col).0;
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
                rel: ctx.display_index(i),
                // Only a fold header reaches here — hidden lines emit no rows.
                folded: ctx.fold_at(i).is_some(),
                continuation: k > 0,
                width: ctx.num_width,
                relative: self.line_numbers == LineNumbers::Relative,
                cur_line: self.cur_line.clone(),
                cur_rel: self.cur_rel.clone(),
            });
            out.push(LineElement {
                text: text[b0..b1].to_string().into(),
                segments: slice_segments(segments, b0, b1),
                caret: (caret_row == Some(k)).then(|| LineCaret {
                    col: cur_col.unwrap_or(0) - c0,
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

/// The slice of `ranges` that can overlap the char range `[start, end)`.
///
/// `ranges` ascends in both bounds (see `RowCtx::search_ranges`), so everything
/// overlapping is one contiguous window and the rest is never looked at —
/// scanning them all cost O(lines × matches) per build, which an hlsearch query
/// on a large note turns into millions of rope reads.
fn ranges_touching(ranges: &[(usize, usize)], start: usize, end: usize) -> &[(usize, usize)] {
    let from = ranges.partition_point(|&(_, e)| e <= start);
    let len = ranges[from..].iter().take_while(|&&(s, _)| s < end).count();
    &ranges[from..from + len]
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
/// and new cursor lines can render differently (conceal swap, caret quad), so
/// the cached rows can be patched instead of rebuilt.
pub(super) fn caret_only_change(old: &RowsKey, new: &RowsKey) -> bool {
    old.revision == new.revision && content_change_is_local(old, new)
}

/// Everything but the content matches, so a content change that is *known* to be
/// confined to a few lines can be patched too. The caller supplies that
/// knowledge — `Editor::spans_for_render` returning the edited line — because a
/// row key can't express it: `revision` says the content differs, not how much.
///
/// The two highlight sources are treated differently, and the asymmetry is the
/// point:
///
/// - **Search: an unchanged query is enough.** Matches are content-addressed —
///   a line's match columns are a function of that line's text and the query,
///   both unchanged — so every cached row stays correct and only the patched
///   lines need recomputing. Requiring an *empty* query instead meant
///   `hlsearch = true` forced a full rebuild on every `j`, `k` and `n`.
/// - **Selection: it must be absent, not merely equal.** `sel` is an absolute
///   char span. Were the content to shift under an unchanged span, each line's
///   *local* highlight columns would move while its cached row kept the old
///   ones. Visual-mode edits leave visual (so the mode differs and this never
///   fires today), but "unreachable" is a poor thing to rely on.
pub(super) fn content_change_is_local(old: &RowsKey, new: &RowsKey) -> bool {
    old.wrap_width == new.wrap_width
        && old.mode == new.mode
        && old.view == new.view
        && old.sel.is_none()
        && new.sel.is_none()
        && old.q == new.q
        // A fold toggle hides or reveals a span of lines and renumbers every
        // relative label below it — nothing local about it.
        && old.folds == new.folds
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
    use super::{
        clip_row_highlight, remap_highlight, slice_segments, wrap_columns, Highlight, LineForm,
        LineForms,
    };
    use crate::markdown::{self, SpanKind};
    use gpui::{px, Pixels};
    use ropey::Rope;

    #[test]
    fn a_lit_search_still_allows_the_patch_path() {
        use super::{caret_only_change, content_change_is_local, RowsKey};
        use crate::vim::Mode;
        let key = |revision: u64, caret: usize, q: &str, sel| RowsKey {
            revision,
            caret,
            mode: Mode::Normal,
            view: false,
            sel,
            q: q.to_string(),
            wrap_width: Some(px(400.)),
            folds: 0,
        };

        // No search: a caret move patches, as before.
        assert!(caret_only_change(&key(1, 0, "", None), &key(1, 9, "", None)));
        // hlsearch lit on an unchanged query: still patchable. Requiring an
        // *empty* query here forced a full rebuild on every j/k/n.
        assert!(caret_only_change(&key(1, 0, "widget", None), &key(1, 9, "widget", None)));
        // A changed query re-highlights arbitrary lines: no patch.
        assert!(!caret_only_change(&key(1, 0, "widge", None), &key(1, 9, "widget", None)));
        // A one-line edit with the query unchanged is patchable (revision moves,
        // which `caret_only_change` rejects but `content_change_is_local` allows).
        assert!(content_change_is_local(&key(1, 0, "widget", None), &key(2, 1, "widget", None)));
        assert!(!caret_only_change(&key(1, 0, "widget", None), &key(2, 1, "widget", None)));
        // A selection must be absent, not merely equal — `sel` is an absolute
        // span, so content shifting under it would move every line's local
        // columns while cached rows kept the old ones.
        let sel = Some((3, 7));
        assert!(!content_change_is_local(&key(1, 0, "", sel), &key(2, 1, "", sel)));
        // A fold toggle moves neither caret nor content, so only the fold
        // generation separates the two keys — and it must force a rebuild, or
        // `za` would render as a cache hit and nothing would happen.
        let mut folded = key(1, 0, "", None);
        folded.folds = 1;
        assert!(!caret_only_change(&key(1, 0, "", None), &folded));
        assert!(!content_change_is_local(&key(1, 0, "", None), &folded));
        // Anything else that changes row content still forces a full rebuild.
        let mut wide = key(1, 9, "", None);
        wide.wrap_width = Some(px(500.));
        assert!(!caret_only_change(&key(1, 0, "", None), &wide));
    }

    #[test]
    fn display_index_counts_a_closed_fold_once() {
        use super::display_index;
        // Two closed folds: lines 2–5 and 10–12 hidden behind their headers.
        let folds = [(2usize, 5usize), (10, 12)];

        // Before any fold, display index is the line itself.
        assert_eq!(display_index(&folds, 0), 0);
        assert_eq!(display_index(&folds, 2), 2);
        // A hidden line shares its header's index — a caret dragged inside a
        // fold labels the gutter as if it were on the header.
        assert_eq!(display_index(&folds, 3), 2);
        assert_eq!(display_index(&folds, 5), 2);
        // Past the fold, every line shifts up by the 3 lines it hides.
        assert_eq!(display_index(&folds, 6), 3);
        assert_eq!(display_index(&folds, 10), 7);
        // Both folds discount: 3 + 2 hidden lines by the time we're past them.
        assert_eq!(display_index(&folds, 13), 8);

        // The distance the gutter shows is the difference of two of these, and
        // it must equal the number of `j` presses: from line 0 to line 13 the
        // display lines are 0, 1, 2(fold), 6, 7, 8, 9, 10(fold), 13.
        assert_eq!(display_index(&folds, 13) - display_index(&folds, 0), 8);
        // No folds: unchanged from plain line numbering.
        assert_eq!(display_index(&[], 42), 42);
    }

    #[test]
    fn ranges_touching_windows_the_matches_for_one_line() {
        use super::ranges_touching;
        // Four matches of a 3-char query, at chars 0, 10, 20, 30.
        let r = [(0, 3), (10, 13), (20, 23), (30, 33)];

        // A line covering [10, 20) sees only the match inside it.
        assert_eq!(ranges_touching(&r, 10, 20), &[(10, 13)]);
        // A line spanning several sees all of them, contiguously.
        assert_eq!(ranges_touching(&r, 0, 25), &[(0, 3), (10, 13), (20, 23)]);
        // Boundaries are half-open at both ends: a match ending exactly at the
        // line's start, or starting exactly at its end, doesn't touch it.
        assert_eq!(ranges_touching(&r, 3, 10), &[]);
        assert_eq!(ranges_touching(&r, 25, 30), &[]);
        // A partial overlap still counts — the highlight gets clipped later.
        assert_eq!(ranges_touching(&r, 11, 15), &[(10, 13)]);
        // Off the end, and the empty case.
        assert_eq!(ranges_touching(&r, 100, 200), &[]);
        assert_eq!(ranges_touching(&[], 0, 10), &[]);

        // Exhaustive cross-check against the naive scan it replaced: for every
        // line window, the window must contain exactly the overlapping matches.
        for start in 0..40 {
            for end in start..40 {
                let naive: Vec<_> =
                    r.iter().copied().filter(|&(lo, hi)| lo < end && hi > start).collect();
                assert_eq!(ranges_touching(&r, start, end), &naive[..], "[{start}, {end})");
            }
        }
    }

    #[test]
    fn line_forms_keeps_only_what_stays_valid() {
        let form = || LineForm {
            text: String::new(),
            segments: Vec::new(),
            metrics: (1.0, 0.0),
            decor: None,
            row_starts: vec![0],
        };
        let w: Option<Pixels> = Some(px(400.));
        let fill = |c: &mut LineForms, n: usize| {
            for i in 0..n {
                c.put(i, form());
            }
        };
        let held = |c: &mut LineForms, n: usize| {
            (0..n)
                .filter(|&i| {
                    let hit = c.take(i);
                    let present = hit.is_some();
                    if let Some(f) = hit {
                        c.put(i, f);
                    }
                    present
                })
                .collect::<Vec<_>>()
        };

        // A caret-only rebuild (same revision, same width) keeps everything —
        // this is what makes a `j` cheap.
        let mut c = LineForms::default();
        c.begin(7, w, 5, None);
        fill(&mut c, 5);
        c.begin(7, w, 5, None);
        assert_eq!(held(&mut c, 5), vec![0, 1, 2, 3, 4]);

        // A confined edit drops that line and its two neighbours, keeps the rest.
        c.begin(8, w, 5, Some(2));
        assert_eq!(held(&mut c, 5), vec![0, 4]);

        // An edit at line 0 clamps instead of underflowing.
        fill(&mut c, 5);
        c.begin(9, w, 5, Some(0));
        assert_eq!(held(&mut c, 5), vec![2, 3, 4]);

        // An unconfined content change drops everything.
        fill(&mut c, 5);
        c.begin(10, w, 5, None);
        assert!(held(&mut c, 5).is_empty());

        // So does a wrap-width change — boundaries depend on it, which is why
        // this cache does nothing for a resize.
        fill(&mut c, 5);
        c.begin(10, Some(px(500.)), 5, None);
        assert!(held(&mut c, 5).is_empty());

        // And so does a line-count change, even when a line is named dirty:
        // the indices no longer mean the same lines.
        fill(&mut c, 5);
        c.begin(11, Some(px(500.)), 6, Some(2));
        assert!(held(&mut c, 6).is_empty());

        // Out-of-range access is refused, not a panic.
        assert!(c.take(99).is_none());
        c.put(99, form());
    }

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
