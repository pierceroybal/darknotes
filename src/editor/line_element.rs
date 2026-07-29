//! `LineElement`: the visual-row element the editor list is made of —
//! layout/paint for one wrapped row, at a height known from its content
//! alone (`height`) — plus the per-segment text styling (runs, heading
//! metrics, decor bands) it and the row builder share.
//!
//! A child module of `editor` so paint can reach private `Editor` state.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    fill, outline, point, prelude::*, px, relative, size, App, BorderStyle, Bounds, ContentMask,
    Corners, Edges, Font, FontId, FontStyle, FontWeight, GlobalElementId, GlyphId, Hsla,
    InspectorElementId, LayoutId, Pixels, ShapedLine, SharedString, StrikethroughStyle, Style,
    TextRun, TransformationMatrix, Window,
};

use crate::markdown::{Segment, Span, SpanKind};
use crate::theme::Theme;


/// One caret on the cursor line. `block` is vim's normal/command block caret
/// (inverts the char under it); otherwise it's the insert-mode bar.
#[derive(Clone, Copy)]
pub(super) struct LineCaret {
    pub(super) col: usize,
    pub(super) block: bool,
}

/// How the caret paints this frame: solid accent (focused, blink phase on),
/// hidden (focused, blink phase off), or dim `muted` (editor unfocused —
/// solid, no blinking).
#[derive(Clone, Copy, PartialEq)]
pub(super) enum CaretPaint {
    Solid,
    Hidden,
    Dim,
}

/// One row's line-number gutter, stored as the *inputs* to its label rather
/// than a formatted string.
///
/// Relative numbering re-labels every row whenever the cursor line moves, so a
/// baked-in label would make every cached row stale on every caret move — which
/// is why `Plan::Patch` used to be skipped entirely in that mode, costing a
/// whole-document rebuild per `j`. Deriving the label in `prepaint` from a
/// shared cursor-line cell (the same trick `scroll_x` and `caret_paint` use)
/// keeps cached rows valid, and formats only the rows actually on screen.
#[derive(Clone)]
pub(super) struct Gutter {
    /// 0-based logical line this row belongs to.
    pub(super) line: usize,
    /// A wrapped line's continuation row: blanks of the same width, so its text
    /// stays aligned under the first row's.
    pub(super) continuation: bool,
    /// Digits reserved for the number. The label is always `width + 3` chars,
    /// which is what keeps its shaped width independent of the caret.
    pub(super) width: usize,
    /// Hybrid relative numbering: the distance to the cursor line, except on the
    /// cursor line itself, which shows its absolute number.
    pub(super) relative: bool,
    /// Cursor line, read at paint time — see `Editor::cur_line`.
    pub(super) cur_line: Rc<Cell<usize>>,
}

impl Gutter {
    /// This row's label and its color, resolved against the live cursor line.
    /// Callers that only want the width still need the text, since it decides it.
    pub(super) fn resolve(&self, theme: &Theme) -> (SharedString, Hsla) {
        let width = self.width;
        if self.continuation {
            return (format!(" {:>width$}  ", "").into(), theme.muted);
        }
        let cur = self.cur_line.get();
        let n = if self.relative && self.line != cur {
            self.line.abs_diff(cur)
        } else {
            self.line + 1
        };
        let color = if self.line == cur { theme.foreground } else { theme.muted };
        (format!(" {n:>width$}  ").into(), color)
    }
}

/// The selected column span within a line (visual mode). `to_eol` means the
/// selection covers this line's newline, so the highlight fills to the edge.
#[derive(Clone, Copy)]
pub(super) struct Highlight {
    pub(super) start_col: usize,
    pub(super) end_col: usize,
    pub(super) to_eol: bool,
}

/// One text line, shaped as a single uniform run with the caret drawn entirely
/// as an overlay. Shaping keys on `text` + `segments`, never the caret position,
/// so GPUI's shaped-line cache reuses layouts as the caret moves within a line.
/// When `render_markdown` is on, non-cursor lines carry concealed text/segments
/// and the cursor line carries source, so only the two lines a vertical move
/// swaps between re-shape — everything else stays cached.
/// With soft-wrap, one of these is one *visual row*: `build_rows` slices a
/// wrapped line into per-row text/segments/highlights, so this element never
/// needs to know about wrapping.
#[derive(Clone)]
pub(super) struct LineElement {
    pub(super) text: SharedString,
    /// Styled segments matching `text` (concealed or source), mapped to runs in
    /// `prepaint`. Independent of the caret, so the cache keys on content alone.
    pub(super) segments: Vec<Segment>,
    /// `Some` only on the cursor line.
    pub(super) caret: Option<LineCaret>,
    /// `Some` when part of this line falls inside the visual selection. Columns
    /// index this element's `text` — concealed lines get spans already remapped
    /// through the conceal map.
    pub(super) selection: Option<Highlight>,
    /// Search-match column spans within this line (hlsearch/incsearch), in the
    /// same (possibly concealed) coordinates as `selection`.
    pub(super) search: Vec<Highlight>,
    /// Shared horizontal scroll offset. The cursor line writes it (prepaint),
    /// every line reads it (paint) — see `Editor::scroll_x`.
    pub(super) scroll_x: Rc<Cell<Pixels>>,
    /// How the caret paints (blink phase + focus), shared like `scroll_x` —
    /// see `Editor::caret_paint`.
    pub(super) caret_paint: Rc<Cell<CaretPaint>>,
    /// Line-number gutter, as the inputs to its label rather than the label —
    /// see `Gutter`. `None` when the gutter is off. Painted at a fixed left
    /// position; the text is shifted right past it.
    pub(super) gutter: Option<Gutter>,
    /// Nudge `scroll_x` to keep the caret horizontally on screen — nowrap
    /// only. A wrapped row never overflows, and its caret reaching the right
    /// edge must not shift the pane. Only the caret row acts on it.
    pub(super) follow_h: bool,
    /// Font-size multiplier for this row (heading lines shape larger, the
    /// cursor line included). 1.0 everywhere else; the gutter always stays
    /// at body size.
    pub(super) scale: f32,
    /// Extra breathing room above the text — a heading line's first visual
    /// row carries rendered markdown's top margin. Zero everywhere else.
    /// Included in `height`; text and quads paint below it.
    pub(super) pad_top: Pixels,
    /// Block-level paint decoration (code band / quote bar / rule hairline).
    /// `None` on the cursor line and with markdown rendering off.
    pub(super) decor: Option<RowDecor>,
}

pub(super) struct LinePrepaint {
    shaped: ShapedLine,
    /// Shaped line-number gutter and its width; `None` when the gutter is off.
    gutter: Option<ShapedLine>,
    gutter_w: Pixels,
    /// `(x within the line, width)` of the selection quad; `None` if unselected.
    selection: Option<(Pixels, Pixels)>,
    /// `(x within the line, width)` of each search-match quad.
    search: Vec<(Pixels, Pixels)>,
    /// `(x within the line, width)` of the caret quad; `None` off the cursor line.
    caret: Option<(Pixels, Pixels)>,
    /// Block caret only: the glyph under the caret, repainted dark over the
    /// block. `(font, glyph, x within the line)`. `None` for the bar and at EOL.
    caret_glyph: Option<(FontId, GlyphId, Pixels)>,
    /// `(x0, x1, checked)` of a task box's transparent `[ ]` span; the box
    /// paints centered in it. `None` when the row has none (or shows source —
    /// those rows carry Marker, not Task).
    task: Option<(Pixels, Pixels, bool)>,
}

impl LineElement {
    /// This row's height: the scaled line box plus the heading top margin,
    /// rounded to whole pixels so abutting rows meet on crisp boundaries.
    /// `RowList`'s offset table and `request_layout` must agree, so both
    /// call this.
    pub(super) fn height(&self, line_h: Pixels) -> Pixels {
        (line_h * self.scale).round() + self.pad_top
    }
}

impl IntoElement for LineElement {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for LineElement {
    type RequestLayoutState = ();
    type PrepaintState = LinePrepaint;

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = self.height(window.line_height()).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _layout: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> LinePrepaint {
        let style = window.text_style();
        let font = style.font();
        let fg = style.color;
        let base_size = style.font_size.to_pixels(window.rem_size());
        // Headings shape larger; the gutter below keeps body size so line
        // numbers stay column-aligned across rows.
        let font_size = base_size * self.scale;

        // Styled runs from this line's markdown spans. Shaping keys on text +
        // markup, never the caret, so the layout cache keeps hitting as the
        // cursor moves; the caret below is an overlay that re-shapes nothing.
        let theme = *cx.global::<Theme>();
        let runs = segments_to_runs(&self.text, &self.segments, &font, fg, &theme);
        // Shape the line-number gutter (if any). It sits at a fixed left position
        // and never scrolls, so the text below is shifted right by its width.
        let gutter = self.gutter.as_ref().map(|g| {
            let (text, color) = g.resolve(&theme);
            let runs = [run(&font, text.len(), color)];
            window.text_system().shape_line(text, base_size, &runs, None)
        });
        let gutter_w = gutter.as_ref().map_or(Pixels::ZERO, |g| g.width);
        let shaped = window
            .text_system()
            .shape_line(self.text.clone(), font_size, &runs, None);

        let (caret, caret_glyph) = match self.caret {
            None => (None, None),
            Some(c) => {
                let (caret_byte, under_end) = caret_bytes(&self.text, c.col);
                let x = shaped.x_for_index(caret_byte);
                // Follow the caret horizontally: keep it `margin` inside both
                // edges of the pane. Only the cursor row writes scroll_x; every
                // row reads it in paint. A short line (caret near x=0) snaps the
                // offset back to 0 on its own.
                // ponytail: margin ≈ 2 chars; no mouse-wheel/`zh`/`zl` scroll yet.
                if self.follow_h {
                    let margin = font_size * 2.;
                    let viewport = bounds.size.width - gutter_w;
                    let mut s = self.scroll_x.get();
                    if x < s + margin {
                        s = x - margin;
                        if s < Pixels::ZERO {
                            s = Pixels::ZERO;
                        }
                    } else if x > s + viewport - margin {
                        s = x - viewport + margin;
                    }
                    self.scroll_x.set(s);
                }
                match (c.block, under_end) {
                    // Block caret over a char: full-cell quad, and grab that
                    // glyph from the cached layout to repaint it dark on top.
                    (true, Some(end)) => {
                        let glyph = shaped.runs.iter().find_map(|r| {
                            r.glyphs
                                .iter()
                                .find(|g| g.index == caret_byte)
                                .map(|g| (r.font_id, g.id, g.position.x))
                        });
                        (Some((x, shaped.x_for_index(end) - x)), glyph)
                    }
                    // ponytail: EOL block width is a font-size estimate; it only
                    // shows past the last glyph, where exactness doesn't matter.
                    (true, None) => (Some((x, font_size * 0.5)), None),
                    // Insert-mode bar.
                    (false, _) => (Some((x, px(2.))), None),
                }
            }
        };

        // Resolve the highlight columns to a pixel span. `to_eol` overshoots to
        // the pane width; the content mask in paint clips it to the line box.
        let selection = self.selection.map(|h| {
            let x0 = shaped.x_for_index(caret_bytes(&self.text, h.start_col).0);
            let width = if h.to_eol {
                bounds.size.width
            } else {
                shaped.x_for_index(caret_bytes(&self.text, h.end_col).0) - x0
            };
            (x0, width)
        });

        // Search matches never cover the newline (`to_eol` is always false), so
        // both edges resolve through the shaped line like the selection above.
        let search = self
            .search
            .iter()
            .map(|h| {
                let x0 = shaped.x_for_index(caret_bytes(&self.text, h.start_col).0);
                let x1 = shaped.x_for_index(caret_bytes(&self.text, h.end_col).0);
                (x0, x1 - x0)
            })
            .collect();

        // A task row's box target: the pixel span of its `[ ]` bytes.
        let mut task = None;
        let mut byte = 0;
        for seg in &self.segments {
            if let Some(SpanKind::Task(checked)) = seg.kind {
                task =
                    Some((shaped.x_for_index(byte), shaped.x_for_index(byte + seg.len), checked));
                break;
            }
            byte += seg.len;
        }

        LinePrepaint {
            shaped,
            gutter,
            gutter_w,
            selection,
            search,
            caret,
            caret_glyph,
            task,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _layout: &mut (),
        prepaint: &mut LinePrepaint,
        window: &mut Window,
        cx: &mut App,
    ) {
        // The glyph box: the row minus any heading top margin. Text, quads,
        // and the gutter all center in it, sitting below the pad.
        let line_height = bounds.size.height - self.pad_top;
        let oy = bounds.origin.y + self.pad_top;
        let theme = *cx.global::<Theme>();
        // The gutter sits flush left and never scrolls; paint it first, outside
        // the text's clip so scrolled text can't bleed over it.
        if let Some(gutter) = &prepaint.gutter {
            let _ = gutter.paint(point(bounds.origin.x, oy), line_height, window, cx);
        }
        // Text starts past the gutter and shifts left by the horizontal scroll
        // offset; clip to the area right of the gutter so left-overflow stops at
        // the gutter and right-overflow stops at the pane edge.
        let text_origin_x = bounds.origin.x + prepaint.gutter_w;
        // Code rows inset all their content (text, caret, highlights) by
        // CODE_MARGIN + CODE_PAD so it clears the band border; wrap width
        // shrank to match in `append_line_rows`.
        let pad = match self.decor {
            Some(RowDecor::CodeBand { .. }) => CODE_MARGIN + CODE_PAD,
            _ => Pixels::ZERO,
        };
        let ox = text_origin_x + pad - self.scroll_x.get();
        let text_bounds = Bounds::new(
            point(text_origin_x, bounds.origin.y),
            size(bounds.size.width - prepaint.gutter_w, bounds.size.height),
        );
        // Decorations paint first (beneath everything) and outside the text
        // mask — they pin to the pane edge, never scroll, and stay inside the
        // row horizontally, so they don't need the clip. Outside it, a band
        // row can bleed 1px into the row below: bordered/rounded quads render
        // with antialiased edges, and two abutting edges on a fractional
        // pixel boundary each blend with the backdrop, leaving a hairline
        // seam between rows — overlapping the opaque quads hides it.
        match self.decor {
            Some(RowDecor::CodeBand { top, bottom }) => {
                let edge = |on: bool| if on { px(1.) } else { Pixels::ZERO };
                let radius = |on: bool| if on { px(4.) } else { Pixels::ZERO };
                let bleed = if bottom { Pixels::ZERO } else { px(1.) };
                let band_bounds = Bounds::new(
                    point(text_bounds.origin.x + CODE_MARGIN, text_bounds.origin.y),
                    size(text_bounds.size.width - CODE_MARGIN * 2., text_bounds.size.height + bleed),
                );
                window.paint_quad(
                    fill(band_bounds, theme.code_bg)
                        .corner_radii(Corners {
                            top_left: radius(top),
                            top_right: radius(top),
                            bottom_right: radius(bottom),
                            bottom_left: radius(bottom),
                        })
                        .border_widths(Edges {
                            top: edge(top),
                            right: px(1.),
                            bottom: edge(bottom),
                            left: px(1.),
                        })
                        .border_color(theme.border),
                )
            }
            Some(RowDecor::QuoteBar) => window.paint_quad(fill(
                Bounds::new(text_bounds.origin, size(px(3.), line_height)),
                theme.muted,
            )),
            Some(RowDecor::Rule) => window.paint_quad(fill(
                Bounds::new(
                    point(text_origin_x, bounds.origin.y + (line_height - px(1.)) / 2.),
                    size(text_bounds.size.width, px(1.)),
                ),
                theme.border,
            )),
            None => {}
        }
        window.with_content_mask(Some(ContentMask { bounds: text_bounds }), |window| {
            // Paint order, bottom-up: search-match quads, selection
            // highlight, caret quad, the line, the inverted caret glyph, and
            // the task box over its transparent source bytes.
            for &(x, width) in &prepaint.search {
                let origin = point(ox + x, oy);
                window.paint_quad(fill(
                    Bounds::new(origin, size(width, line_height)),
                    theme.search_match,
                ));
            }
            if let Some((x, width)) = prepaint.selection {
                let origin = point(ox + x, oy);
                window.paint_quad(fill(
                    Bounds::new(origin, size(width, line_height)),
                    theme.selection,
                ));
            }
            let caret_paint = self.caret_paint.get();
            if let Some((x, width)) = prepaint.caret {
                if caret_paint != CaretPaint::Hidden {
                    let color = if caret_paint == CaretPaint::Dim {
                        theme.muted
                    } else {
                        theme.accent
                    };
                    let origin = point(ox + x, oy);
                    window.paint_quad(fill(
                        Bounds::new(origin, size(width, line_height)),
                        color,
                    ));
                }
            }
            let shaped = &prepaint.shaped;
            let _ = shaped.paint(point(ox, oy), line_height, window, cx);
            // A hidden caret's block quad isn't there, so keep the glyph in
            // its normal color instead of repainting it dark.
            if let Some((font_id, glyph_id, gx)) =
                prepaint.caret_glyph.filter(|_| caret_paint != CaretPaint::Hidden)
            {
                // Match the baseline `ShapedLine::paint` uses: line is vertically
                // centered, glyph sits on the baseline (`paint_glyph` y is baseline).
                let padding_top = (line_height - shaped.ascent - shaped.descent) / 2.;
                let baseline = point(ox + gx, oy + padding_top + shaped.ascent);
                let _ =
                    window.paint_glyph(baseline, font_id, glyph_id, shaped.font_size, theme.background);
            }
            // Task box centered over its `[ ]` span: outlined when unchecked,
            // accent-filled with a check when done. Whole-pixel origin — a
            // 1px border at a fractional x antialiases unevenly (one edge
            // crisp, the other ghosted).
            if let Some((x0, x1, checked)) = prepaint.task {
                let s = px(12.).min(line_height);
                let b = Bounds::new(
                    point(
                        (ox + (x0 + x1 - s) / 2.).round(),
                        (oy + (line_height - s) / 2.).round(),
                    ),
                    size(s, s),
                );
                if checked {
                    window.paint_quad(fill(b, theme.accent).corner_radii(px(3.)));
                    // Lucide check through the sidebar-icon asset pipeline; its
                    // 24-viewBox padding insets the stroke, so it paints
                    // across the full box.
                    let _ = window.paint_svg(
                        b,
                        "icons/check.svg".into(),
                        TransformationMatrix::unit(),
                        theme.background,
                        cx,
                    );
                } else {
                    window.paint_quad(
                        outline(b, theme.muted, BorderStyle::default()).corner_radii(px(3.)),
                    );
                }
            }
        });
    }
}

pub(super) fn run(font: &Font, len: usize, color: Hsla) -> TextRun {
    TextRun {
        len,
        font: font.clone(),
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    }
}

/// Map a line's flattened segments to styled runs covering the whole line —
/// `shape_line` drops glyphs unless the run lengths sum to the byte length. An
/// empty line keeps one zero-length default run, matching the prior behavior.
pub(super) fn segments_to_runs(
    text: &str,
    segments: &[Segment],
    font: &Font,
    fg: Hsla,
    theme: &Theme,
) -> Vec<TextRun> {
    if segments.is_empty() {
        return vec![run(font, text.len(), fg)];
    }
    segments
        .iter()
        .map(|seg| {
            let (color, weight, style, background_color) = segment_style(seg.kind, fg, theme);
            let mut font = font.clone();
            font.weight = weight;
            font.style = style;
            // `color: None` takes the run's own color, so a struck link keeps
            // its rule in link color.
            let strikethrough = seg
                .struck
                .then(|| StrikethroughStyle { thickness: px(1.), color: None });
            TextRun { len: seg.len, font, color, background_color, underline: None, strikethrough }
        })
        .collect()
}

/// `(font-size multiplier, top-margin factor in lines)` for a heading line's
/// segments: H1 1.5×, H2 1.3×, H3 1.15×, everything else body size. H1/H2
/// carry 0.6 of a line of breathing room above, H3 0.3 — applied to the
/// line's first visual row only. A heading whose every byte is covered by a
/// higher-priority span (e.g. `# **all bold**`) flattens with no Heading
/// segment left and stays at body size — rare enough to ignore.
pub(super) fn heading_metrics(segments: &[Segment]) -> (f32, f32) {
    let level = segments.iter().find_map(|s| match s.kind {
        Some(SpanKind::Heading(n)) => Some(n),
        _ => None,
    });
    match level {
        Some(1) => (1.5, 0.6),
        Some(2) => (1.3, 0.6),
        Some(3) => (1.15, 0.3),
        _ => (1.0, 0.0),
    }
}

/// Block-level paint decoration for a row. Applied only to concealed rows —
/// the cursor line and raw view show plain source, like conceal.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum RowDecor {
    /// `code_bg` band behind a fence/code line, inset `CODE_MARGIN` from the
    /// pane edges and drawn with a 1px side border. `top`/`bottom` mark the
    /// block's first/last line, which
    /// close the border and round its corners; `append_line_rows` further
    /// restricts them to the first/last visual row of a wrapped line.
    CodeBand { top: bool, bottom: bool },
    /// 3px bar at the left edge of a blockquote line (its `>` conceals).
    QuoteBar,
    /// Hairline across the row replacing a `---`/`***`/`___` line.
    Rule,
}

/// Horizontal margin of a code band's quad from the pane edges.
pub(super) const CODE_MARGIN: Pixels = px(8.);

/// Text inset inside a code band, so content clears the border. Band text
/// sits `CODE_MARGIN + CODE_PAD` from the pane edge; wrap width shrinks by
/// twice that sum for band lines, keeping wrapped rows inside the border.
pub(super) const CODE_PAD: Pixels = px(8.);

/// Decoration for line `line`, from its spans. The scanner pushes a line's
/// role span first, so the leading span's kind decides — which also covers
/// empty in-fence lines (their zero-length `CodeText` span still leads) where
/// the flattened segments would be empty. A code band's `top`/`bottom` come
/// from whether the neighboring lines are in-band.
// ponytail: back-to-back fenced blocks (no blank line between) merge into one
// band; track fence open/close state here if that ever reads wrong.
pub(super) fn row_decor(spans: &[Vec<Span>], line: usize) -> Option<RowDecor> {
    let lead = |i: usize| spans.get(i).and_then(|s| s.first()).map(|s| s.kind);
    match lead(line) {
        Some(SpanKind::CodeText | SpanKind::CodeFence) => {
            let band =
                |i: usize| matches!(lead(i), Some(SpanKind::CodeText | SpanKind::CodeFence));
            Some(RowDecor::CodeBand {
                top: line == 0 || !band(line - 1),
                bottom: !band(line + 1),
            })
        }
        Some(SpanKind::BlockQuote) => Some(RowDecor::QuoteBar),
        Some(SpanKind::Rule) => Some(RowDecor::Rule),
        _ => None,
    }
}

/// The two fence lines (`[open, close]`) of the fenced block containing
/// `line`, `[None, None]` when it isn't in one; `close` is `None` while the
/// block is unclosed at EOF. Scans leading span kinds from the top, tracking
/// open/close state like the scanner — adjacent blocks make a purely local
/// opener-vs-closer test ambiguous. Revealing these along with the cursor
/// line keeps a block's fences visible while editing inside it.
// ponytail: O(line) rescan per caret move (index + `first` per line); track
// state incrementally if a profile ever blames it.
pub(super) fn fence_block(spans: &[Vec<Span>], line: usize) -> [Option<usize>; 2] {
    let lead = |i: usize| spans.get(i).and_then(|s| s.first()).map(|s| s.kind);
    let mut open = None;
    for i in 0..=line {
        if lead(i) == Some(SpanKind::CodeFence) {
            open = match open {
                // `line` itself is this block's closing fence.
                Some(top) if i == line => return [Some(top), Some(i)],
                Some(_) => None, // a block closed above `line`
                None => Some(i),
            };
        }
    }
    let Some(top) = open else { return [None, None] };
    let close = (line + 1..spans.len()).find(|&i| lead(i) == Some(SpanKind::CodeFence));
    [Some(top), close]
}

/// Visual style for a flattened segment: (color, weight, slant, background).
/// `None` is default body text.
fn segment_style(
    kind: Option<SpanKind>,
    fg: Hsla,
    theme: &Theme,
) -> (Hsla, FontWeight, FontStyle, Option<Hsla>) {
    let normal = (fg, FontWeight::NORMAL, FontStyle::Normal, None);
    let Some(kind) = kind else { return normal };
    match kind {
        SpanKind::Heading(_) => (theme.heading, FontWeight::BOLD, FontStyle::Normal, None),
        SpanKind::Strong => (strong_color(fg), FontWeight::BOLD, FontStyle::Normal, None),
        SpanKind::Code | SpanKind::CodeText | SpanKind::CodeFence => {
            (theme.code, FontWeight::NORMAL, FontStyle::Normal, Some(theme.code_bg))
        }
        SpanKind::Link => (theme.link, FontWeight::NORMAL, FontStyle::Normal, None),
        SpanKind::BlockQuote => (theme.muted, FontWeight::NORMAL, FontStyle::Italic, None),
        SpanKind::Frontmatter | SpanKind::Marker | SpanKind::Rule => {
            (theme.muted, FontWeight::NORMAL, FontStyle::Normal, None)
        }
        // Concealed rows paint a real box over the `[ ]` bytes: the glyphs
        // shape (reserving the box's width in the layout) but paint
        // transparent. Source view remaps Task to Marker before this runs.
        SpanKind::Task(_) => (Hsla { a: 0., ..fg }, FontWeight::NORMAL, FontStyle::Normal, None),
        // Strike never reaches a segment's kind — it lives in `Segment::struck`.
        SpanKind::ListItem | SpanKind::Strike => normal,
    }
}

// ponytail: gpui's `layout_line` (text_system.rs) infers "same font" from
// "same decoration" (color/underline/strikethrough) and merges adjacent runs
// on that basis without checking weight — so a Strong run flanked by same-`fg`
// text (exactly what concealment produces once the differently-colored `**`
// marker is dropped) gets folded into the surrounding regular-weight run and
// silently loses its bold. Nudging alpha by an imperceptible amount keeps the
// two runs "different" so gpui resolves the bold font instead of assuming it's
// unchanged. Remove this workaround (and the color nudge it does) whichever
// comes first: (1) `LineElement` switches its shaping call from `shape_line`
// to `shape_text` — likely when line wrapping is implemented, since
// `shape_text`'s `process_line` already resolves fonts per-run correctly and
// this bug can't occur there; or (2) a gpui upgrade fixes `layout_line` to
// compare fonts directly instead of inferring sameness from decoration
// (reported upstream to zed-industries/zed).
fn strong_color(fg: Hsla) -> Hsla {
    Hsla { a: (fg.a - 0.001).max(0.0), ..fg }
}

/// `(byte offset of char column `col`, byte offset just past the char under it)`.
/// The second is `None` at or past end-of-line, where no char sits under the caret.
pub(super) fn caret_bytes(text: &str, col: usize) -> (usize, Option<usize>) {
    let caret_byte = text
        .char_indices()
        .nth(col)
        .map(|(b, _)| b)
        .unwrap_or(text.len());
    let under_end = text[caret_byte..]
        .chars()
        .next()
        .map(|c| caret_byte + c.len_utf8());
    (caret_byte, under_end)
}

#[cfg(test)]
mod tests {
    use super::{
        caret_bytes, fence_block, heading_metrics, row_decor, segment_style, Gutter, RowDecor,
    };
    use crate::markdown::{self, SpanKind};
    use gpui::Hsla;
    use ropey::Rope;
    use std::cell::Cell;
    use std::rc::Rc;

    #[test]
    fn gutter_labels_track_the_cursor_line_at_a_fixed_width() {
        let theme = crate::theme::Theme::by_name("dark").unwrap();
        let cur = Rc::new(Cell::new(0usize));
        let g = |line: usize, relative: bool, continuation: bool| Gutter {
            line,
            continuation,
            width: 3,
            relative,
            cur_line: cur.clone(),
        };
        let label = |gut: &Gutter| gut.resolve(&theme).0.to_string();

        // Absolute: 1-based, right-aligned in `width`, ignores the cursor.
        assert_eq!(label(&g(0, false, false)), "   1  ");
        assert_eq!(label(&g(41, false, false)), "  42  ");

        // Relative is hybrid: distance to the cursor, except on the cursor
        // line, which shows its own absolute number.
        cur.set(10);
        assert_eq!(label(&g(10, true, false)), "  11  "); // cursor line
        assert_eq!(label(&g(7, true, false)), "   3  ");
        assert_eq!(label(&g(13, true, false)), "   3  ");

        // The cursor line is the only thing that moved; every label follows it
        // with no rebuild. This is what makes `Plan::Patch` legal in relative
        // mode — the row is unchanged, only the shared cell is.
        cur.set(7);
        assert_eq!(label(&g(10, true, false)), "   3  ");
        assert_eq!(label(&g(7, true, false)), "   8  ");

        // Continuation rows are same-width blanks, so wrapped text stays aligned.
        assert_eq!(label(&g(10, true, true)), "      ");

        // The load-bearing invariant: the label is always `width + 3` chars
        // whatever the caret does. `gutter_w` is measured from it and feeds the
        // text offset and click-to-caret math, so a variable-width label would
        // make those depend on the cursor's line number.
        for cursor in [0usize, 5, 999] {
            cur.set(cursor);
            for line in [0usize, 1, 42, 998] {
                for relative in [true, false] {
                    assert_eq!(label(&g(line, relative, false)).len(), 6, "{line}/{cursor}");
                }
            }
        }

        // The cursor line reads as foreground, everything else muted.
        cur.set(4);
        assert_eq!(g(4, true, false).resolve(&theme).1, theme.foreground);
        assert_eq!(g(5, true, false).resolve(&theme).1, theme.muted);
    }

    #[test]
    fn strong_color_differs_from_plain_text() {
        // Strong must not share an exact color with plain body text: gpui's
        // layout_line treats equal-decoration adjacent runs as equal-font and
        // merges them, dropping bold weight when concealment leaves Strong
        // flanked by plain `fg` text (see `strong_color`'s doc comment).
        let fg: Hsla = gpui::rgb(0xcccccc).into();
        let theme = crate::theme::Theme::by_name("dark").unwrap();
        let (strong_color, _, _, _) = segment_style(Some(SpanKind::Strong), fg, &theme);
        let (plain_color, _, _, _) = segment_style(None, fg, &theme);
        assert_ne!(strong_color, plain_color);
    }

    #[test]
    fn heading_metrics_step_down_by_level() {
        let seg = |kind| markdown::Segment { len: 4, kind, struck: false };
        assert_eq!(heading_metrics(&[seg(Some(SpanKind::Heading(1)))]), (1.5, 0.6));
        // Level wins even after inline spans (e.g. Strong) split the line.
        assert_eq!(
            heading_metrics(&[seg(Some(SpanKind::Heading(2))), seg(Some(SpanKind::Strong))]),
            (1.3, 0.6)
        );
        assert_eq!(heading_metrics(&[seg(Some(SpanKind::Heading(3)))]), (1.15, 0.3));
        assert_eq!(heading_metrics(&[seg(Some(SpanKind::Heading(4)))]), (1.0, 0.0));
        assert_eq!(heading_metrics(&[seg(None)]), (1.0, 0.0));
        assert_eq!(heading_metrics(&[]), (1.0, 0.0));
    }

    #[test]
    fn fence_block_finds_enclosing_fences() {
        // 0 a, 1 open, 2 code, 3 close, 4 b, 5 open, 6 code (unclosed)
        let spans = markdown::parse(&Rope::from_str("a\n```\ncode\n```\nb\n```\nx\n"));
        assert_eq!(fence_block(&spans, 0), [None, None]);
        for line in 1..=3 {
            assert_eq!(fence_block(&spans, line), [Some(1), Some(3)], "line {line}");
        }
        assert_eq!(fence_block(&spans, 4), [None, None]);
        assert_eq!(fence_block(&spans, 6), [Some(5), None]); // unclosed at EOF
        // Adjacent blocks: an opener right after a closer keeps its role.
        let spans = markdown::parse(&Rope::from_str("```\n```\n```\nx\n```\n"));
        assert_eq!(fence_block(&spans, 1), [Some(0), Some(1)]);
        assert_eq!(fence_block(&spans, 3), [Some(2), Some(4)]);
    }

    #[test]
    fn row_decor_from_leading_span() {
        let spans = markdown::parse(&Rope::from_str("# h\n> q\n```\ncode\n\n```\nx\n---\n"));
        let band = |top, bottom| Some(RowDecor::CodeBand { top, bottom });
        let expect = [
            None,                     // heading
            Some(RowDecor::QuoteBar), // > q
            band(true, false),        // opening fence
            band(false, false),       // code
            band(false, false),       // empty in-fence line (zero-len span)
            band(false, true),        // closing fence
            None,                     // plain text
            Some(RowDecor::Rule),     // ---
        ];
        for (i, want) in expect.iter().enumerate() {
            assert_eq!(row_decor(&spans, i), *want, "line {i}");
        }
    }

    #[test]
    fn caret_bytes_handles_unicode_and_eol() {
        // "aé": col 0 → byte 0, 'a' ends at 1; col 1 → byte 1, 'é' (2 bytes)
        // ends at 3; col 2 → EOL, byte 3, nothing under.
        assert_eq!(caret_bytes("aé", 0), (0, Some(1)));
        assert_eq!(caret_bytes("aé", 1), (1, Some(3)));
        assert_eq!(caret_bytes("aé", 2), (3, None));
        assert_eq!(caret_bytes("", 0), (0, None));
    }
}
