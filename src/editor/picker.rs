//! The fuzzy picker: file switcher, buffer list, command palette, wikilink
//! insert, and vault-wide content search — one modal over the editor; keys route
//! here while open.
//!
//! A child module of `editor` so methods can touch private `Editor` state.

use std::ops::Range;
use std::path::{Path, PathBuf};

use gpui::{
    div, hsla, prelude::*, px, uniform_list, Context, HighlightStyle, KeyDownEvent, SharedString,
    StyledText, Window,
};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config as NucleoConfig, Matcher, Utf32Str};

use crate::theme::Theme;
use crate::vim::Mode;

use super::{rel_display, search_sensitive, Editor, COMMANDS};

/// Match rows the content picker builds per keystroke. `render_picker` resolves
/// every result per frame, so an unbounded list would cost thousands of row
/// builds a frame on a one-character query; `Picker::hits` still reports the
/// true total, so the cap is never silent.
// ponytail: fixed cap; stream or virtualize if 2000 ever feels short.
const GREP_CAP: usize = 2000;

/// Same glyph gpui's own right-hand truncation uses (its const is private).
const ELLIPSIS: &str = "…";

/// A hit further into the line than this gets the line's *start* elided, so the
/// match survives the row's right-hand truncation. Below it, the line reads from
/// its own beginning.
///
/// A char count standing in for a pixel width: the 640px panel at the default
/// 15px Inter fits ~83 chars, less the `  :42  ` gutter — so ~75 chars of line
/// text, and 48 leaves margin for wider-than-average text. Erring low is the
/// cheap direction (a needlessly elided row still reads; an invisible match is
/// the bug). Measuring real text width is the only exact fix, and
/// `content_rows` has no font metrics.
const ELIDE_AFTER: usize = 48;

/// Context kept before an elided hit: about four or five words at typical word
/// length, snapped forward to a word start so a row never opens mid-word.
const LEAD_CHARS: usize = 32;

/// What a picker row resolves to on Enter. A new picker kind either reuses a
/// variant (an outline picker is a `FileLine` per heading) or adds one.
pub(super) enum PickItem {
    File(PathBuf),
    /// A registry command, by `Command::name`.
    Command(&'static str),
    /// An open buffer, by index into `Editor::buffers`. A raw index is safe:
    /// the picker is modal, so the buffer list can't change while it's open.
    Buffer(usize),
    /// Insert `[[name]]` at the caret; payload is the `rel_display` name.
    InsertLink(String),
    /// A place in a file: open it and put the caret there. `col` is a char
    /// offset into the line — 0 for an agenda task, the match column for a
    /// content-search hit.
    FileLine { path: PathBuf, line: usize, col: usize },
}

/// A styled run inside a row's display text, as a byte range. Kept semantic and
/// resolved to theme colors at render, so building rows needs no theme access.
enum Span {
    /// Dimmed: the `:42` line gutter, a header's folder.
    Dim(Range<usize>),
    /// A query hit, painted like an hlsearch match in the buffer.
    Hit(Range<usize>),
}

impl Span {
    fn style(&self, theme: &Theme) -> (Range<usize>, HighlightStyle) {
        match self {
            Span::Dim(r) => {
                (r.clone(), HighlightStyle { color: Some(theme.muted), ..Default::default() })
            }
            Span::Hit(r) => (
                r.clone(),
                HighlightStyle {
                    background_color: Some(theme.search_match),
                    ..Default::default()
                },
            ),
        }
    }
}

/// A row resolved for one frame: display text, styled runs, header flag. Owned,
/// because the list closure outlives the borrow of `Picker`.
type RenderRow = (SharedString, Vec<(Range<usize>, HighlightStyle)>, bool);

/// One picker row: what it draws and what it resolves to.
struct Row {
    text: String,
    /// Empty for every picker but content search.
    spans: Vec<Span>,
    item: PickItem,
    /// A group header (a file name above its matches): drawn in the heading
    /// color and skipped by `j`/`k`, so only match rows can be picked.
    header: bool,
}

impl Row {
    /// A plain selectable row — every picker but content search.
    fn plain(text: String, item: PickItem) -> Self {
        Row { text, spans: Vec::new(), item, header: false }
    }
}

/// The open picker — a modal over the editor. While `Some`, keys route here
/// (like `Pane::Sidebar`); the query narrows `rows` and `Enter` acts on the
/// highlighted pick.
pub(super) struct Picker {
    /// Query-line prefix: `"> "` for files, `": "` for commands.
    title: &'static str,
    /// Every candidate row. Fixed at open for the nucleo-filtered pickers;
    /// rebuilt per keystroke for content search.
    rows: Vec<Row>,
    query: String,
    /// Visible rows as indices into `rows`, best-first; every index, in order,
    /// when the query is empty.
    results: Vec<usize>,
    pub(super) selected: usize,
    /// Content-search snapshot. Non-empty only for the `:grep` picker, whose
    /// every keystroke rebuilds `rows` from this instead of nucleo-filtering a
    /// fixed list. `lines.is_empty()` *is* the "which kind am I" test — a vault
    /// with no note lines degenerates to a fixed list of no rows, which looks
    /// the same from outside: nothing found.
    lines: Vec<crate::grep::Line>,
    /// Total hits for the current content query, before `GREP_CAP`.
    hits: usize,
}

impl Picker {
    /// A picker over a fixed row list — the nucleo-filtered kind.
    fn over(title: &'static str, rows: Vec<Row>) -> Self {
        let results = (0..rows.len()).collect();
        Picker { title, rows, query: String::new(), results, selected: 0, lines: Vec::new(), hits: 0 }
    }

    /// The row `results[i]` draws.
    fn row(&self, i: usize) -> &Row {
        &self.rows[self.results[i]]
    }

    /// First pickable result — past a leading group header.
    fn first_selectable(&self) -> usize {
        (0..self.results.len()).find(|&i| !self.row(i).header).unwrap_or(0)
    }
}

impl Editor {
    /// `:ls` — the open-buffer picker: tab number + display name (+ `[+]`).
    pub(super) fn open_buffer_picker(&mut self) {
        let rows = self
            .buffers
            .iter()
            .enumerate()
            .map(|(i, b)| {
                let dirty = if b.doc.is_dirty() { " [+]" } else { "" };
                Row::plain(
                    format!("{}: {}{dirty}", i + 1, self.buffer_display(b)),
                    PickItem::Buffer(i),
                )
            })
            .collect();
        self.picker = Some(Picker::over("buf> ", rows));
    }

    /// Open the fuzzy file picker over a snapshot of the current vault files.
    /// Display is the vault-relative path with `.md` dropped.
    pub(super) fn open_file_picker(&mut self) {
        let root = self.vault.root.clone();
        let rows = self
            .vault
            .files
            .iter()
            .map(|p| Row::plain(rel_display(&root, p), PickItem::File(p.clone())))
            .collect();
        self.picker = Some(Picker::over("> ", rows));
    }

    /// Open the note picker whose pick inserts a `[[wikilink]]` at the caret.
    pub(super) fn open_insert_link_picker(&mut self) {
        let root = self.vault.root.clone();
        let rows = self
            .vault
            .files
            .iter()
            .map(|p| {
                let name = rel_display(&root, p);
                Row::plain(name.clone(), PickItem::InsertLink(name))
            })
            .collect();
        self.picker = Some(Picker::over("[[ ", rows));
    }

    /// `:agenda` — today's open tasks, flat and fuzzy-filterable, `Enter` jumps
    /// to the source line (where the ordinary task-toggle keys then work). The
    /// bucket label leads each row, so typing `overdue` narrows to that group.
    /// A flat list is the MVP: no date grouping, no editing from the list.
    pub(super) fn open_agenda_picker(&mut self) {
        let today = jiff::Zoned::now().date();
        let root = self.vault.root.clone();
        let rows: Vec<Row> = crate::tasks::agenda(&self.vault, today)
            .into_iter()
            .map(|t| {
                // `path:line` ahead of the text, grep/quickfix order.
                let display = format!(
                    "{:<7} {}:{}  {}",
                    t.bucket(today).label(),
                    rel_display(&root, &t.path),
                    t.line + 1,
                    t.text
                );
                Row::plain(display, PickItem::FileLine { path: t.path, line: t.line, col: 0 })
            })
            .collect();
        if rows.is_empty() {
            self.message = Some("agenda: nothing due".into());
            return;
        }
        self.picker = Some(Picker::over("agenda> ", rows));
    }

    /// `:grep {pat}` — literal content search across the vault. One disk pass
    /// snapshots every note line; keystrokes filter it in memory. `Enter` jumps
    /// to the match and adopts the query as the search register, so `n`/`N` walk
    /// the rest of that file's hits.
    pub(super) fn open_grep_picker(&mut self, query: Option<&str>) {
        let mut p = Picker::over("grep> ", Vec::new());
        p.lines = crate::grep::snapshot(&self.vault);
        p.query = query.unwrap_or_default().to_string();
        self.picker = Some(p);
        self.refilter_picker(); // builds the rows for a prefilled query
    }

    /// Open the command palette: every registry command, its primary ex alias
    /// appended to the display so typing `:w`-style names finds it too.
    pub(super) fn open_command_palette(&mut self) {
        let builtin = COMMANDS.iter().map(|c| {
            let display = match c.ex.first() {
                Some(ex) => format!("{}  :{}", c.name, ex),
                None => c.name.to_string(),
            };
            Row::plain(display, PickItem::Command(c.name))
        });
        let notes = self
            .note_cmds
            .keys()
            .map(|name| Row::plain(format!("{name}  :{name}"), PickItem::Command(*name)));
        self.picker = Some(Picker::over(": ", builtin.chain(notes).collect()));
    }

    /// Re-run the open picker's query. The fixed-list kinds narrow their rows
    /// with nucleo; the content picker rebuilds its rows from the line snapshot,
    /// so its `rows` are always exactly the (capped, grouped) hit list.
    fn refilter_picker(&mut self) {
        // Read off `self` before borrowing `self.picker`: a disjoint-field
        // borrow doesn't survive being captured by the row builder.
        let root = self.vault.root.clone();
        let cfg = self.search_cfg;
        if let Some(p) = self.picker.as_mut() {
            if p.lines.is_empty() {
                p.results = filter_rows(&p.query, &p.rows);
            } else {
                let sensitive = search_sensitive(&p.query, &cfg);
                let hits = crate::grep::find(&p.lines, &p.query, sensitive);
                p.hits = hits.len();
                let rows = content_rows(&p.lines, &hits, &root, &p.query, sensitive);
                p.results = (0..rows.len()).collect();
                p.rows = rows;
            }
            // A new query invalidates the old highlight; a content list opens on
            // a header, so start at the first row that can be picked.
            p.selected = p.first_selectable();
        }
    }

    /// Keystrokes while the picker is open. Mirrors `vim::command_key`: printable
    /// chars extend the query, Backspace trims it, Esc cancels, Enter acts on the
    /// pick; `Up`/`Down` (and `Ctrl-J`/`Ctrl-K`) move the highlight.
    pub(super) fn picker_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let m = ev.keystroke.modifiers;
        match ev.keystroke.key.as_str() {
            "escape" => self.picker = None,
            "enter" => {
                // take() drops the picker borrow before the pick mutates self.
                if let Some(mut p) = self.picker.take() {
                    // A content pick adopts its query as the search register.
                    let seed = (!p.lines.is_empty()).then(|| p.query.clone());
                    if let Some(&idx) = p.results.get(p.selected) {
                        match p.rows.swap_remove(idx).item {
                            PickItem::File(path) => self.open_path(path, window), // refocuses the editor
                            PickItem::Command(name) => self.run_picked_command(name, window, cx),
                            PickItem::Buffer(i) => self.activate(i, window),
                            PickItem::FileLine { path, line, col } => {
                                self.open_path(path, window); // refocuses the editor
                                self.jump_to_line_col(line, col);
                            }
                            PickItem::InsertLink(name) => {
                                // Bypasses feed_vim's undo checkpointing, so
                                // checkpoint here or `u` swallows earlier edits.
                                self.doc_mut().checkpoint();
                                self.doc_mut().insert(&format!("[[{name}]]"));
                            }
                        }
                        // After the jump, so `n` steps on from where it landed.
                        if let Some(q) = seed {
                            self.seed_search(q);
                        }
                    }
                }
            }
            "backspace" => {
                if let Some(p) = self.picker.as_mut() {
                    p.query.pop();
                }
                self.refilter_picker();
            }
            "down" => self.move_picker(1),
            "up" => self.move_picker(-1),
            "j" if m.control => self.move_picker(1),
            "k" if m.control => self.move_picker(-1),
            _ if !m.control && !m.alt && !m.platform => {
                if let Some(s) = ev.keystroke.key_char.clone() {
                    if let Some(p) = self.picker.as_mut() {
                        p.query.push_str(&s);
                    }
                    self.refilter_picker();
                }
            }
            _ => {}
        }
    }

    /// Caret to `line`:`col` in the open document, both clamped — a picked row
    /// is a snapshot and the file may have changed under it. Mirrors the
    /// click-to-place path: normal mode disallows the column past a line's last
    /// char, insert mode needs it.
    pub(super) fn jump_to_line_col(&mut self, line: usize, col: usize) {
        let rope = &self.doc().rope;
        let line = line.min(rope.len_lines().saturating_sub(1));
        let start = rope.line_to_char(line);
        let at = (start + col).min(start + rope.line(line).len_chars());
        self.doc_mut().jump_to(at);
        if self.vim.mode != Mode::Insert {
            self.doc_mut().clamp_caret_to_line();
        }
        // Arriving from a grep hit, an outline pick or a `[[note#Heading]]`
        // link opens the fold it lands in, like a search does.
        self.reveal_caret_line();
    }

    /// Move the highlight `delta` rows, skipping group headers. Running off
    /// either end leaves the highlight where it was.
    fn move_picker(&mut self, delta: isize) {
        if let Some(p) = self.picker.as_mut() {
            let n = p.results.len() as isize;
            let mut i = p.selected as isize + delta;
            while i >= 0 && i < n {
                if !p.row(i as usize).header {
                    p.selected = i as usize;
                    return;
                }
                i += delta;
            }
        }
    }

    /// The picker overlay: a scrim + centered panel (query line + results
    /// list). `None` when the picker is closed.
    pub(super) fn render_picker(&self, theme: &Theme) -> Option<impl IntoElement> {
        let p = self.picker.as_ref()?;
        let theme = *theme;
        let mut prompt = format!("{}{}", p.title, p.query);
        // Content search reports its true hit count, and says so when the row
        // list is capped rather than truncating silently.
        if !p.lines.is_empty() && !p.query.is_empty() {
            let shown = p.rows.iter().filter(|r| !r.header).count();
            let capped =
                if p.hits > shown { format!(" (first {shown} shown)") } else { String::new() };
            prompt.push_str(&format!("   {} hits{capped}", p.hits));
        }
        let selected = p.selected;
        // Resolved rows, moved into the list closure: text, styled runs, header.
        let rows: Vec<RenderRow> = (0..p.results.len())
            .map(|i| {
                let r = p.row(i);
                (r.text.clone().into(), r.spans.iter().map(|s| s.style(&theme)).collect(), r.header)
            })
            .collect();
        let count = rows.len();
        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .flex_col()
                .items_center()
                .pt(px(80.))
                .bg(hsla(0., 0., 0., 0.4)) // scrim
                .child(
                    div()
                        .w(px(640.))
                        .h(px(420.)) // fixed so the list has a box to scroll in
                        .flex()
                        .flex_col()
                        .font_family(self.ui_font_family.clone())
                        .bg(theme.background)
                        .border_1()
                        .border_color(theme.border)
                        .rounded_lg()
                        .shadow_lg()
                        .child(
                            div()
                                .px_2()
                                .py_1()
                                .text_color(theme.foreground)
                                .child(prompt),
                        )
                        .child(
                            uniform_list("picker", count, move |range, _win, _cx| {
                                range
                                    .map(|i| {
                                        let (text, runs, header) = &rows[i];
                                        // Color is what separates a group header
                                        // from its rows: the embedded ui font
                                        // (Inter) ships regular + italic only, so
                                        // asking for bold weight changes nothing.
                                        let (bg, fg) = if i == selected {
                                            (
                                                theme.sidebar_cursor_background,
                                                theme.sidebar_active_foreground,
                                            )
                                        } else if *header {
                                            (theme.background, theme.heading)
                                        } else {
                                            (theme.background, theme.foreground)
                                        };
                                        // `truncate` = nowrap + clip + ellipsis.
                                        // Without nowrap a long row wraps to a
                                        // second line, overflows the fixed slot
                                        // `uniform_list` gave it, and the list's
                                        // uniform-height math paints the tail as
                                        // a phantom row. Ellipsis resolves here
                                        // because a list row is block layout with
                                        // a definite width; gpui adjusts the
                                        // highlight runs when it truncates.
                                        let row =
                                            div().px_2().truncate().bg(bg).text_color(fg);
                                        if runs.is_empty() {
                                            row.child(text.clone())
                                        } else {
                                            row.child(
                                                StyledText::new(text.clone())
                                                    .with_highlights(runs.clone()),
                                            )
                                        }
                                    })
                                    .collect::<Vec<_>>()
                            })
                            .track_scroll(self.picker_scroll.clone())
                            .flex_1(),
                        ),
                ),
        )
    }
}

/// Grouped rows for a content search: a header per file, then that file's
/// matching lines. `hits` must arrive in vault order (as `grep::find` returns
/// them), which is what makes grouping a single pass with no sorting.
fn content_rows(
    lines: &[crate::grep::Line],
    hits: &[usize],
    root: &Path,
    query: &str,
    sensitive: bool,
) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut current: Option<&Path> = None;
    for &i in hits.iter().take(GREP_CAP) {
        let l = &lines[i];
        if current != Some(l.path.as_path()) {
            current = Some(l.path.as_path());
            rows.push(header_row(root, &l.path));
        }
        // `  :42  text`, the line's start elided when the first hit sits too far
        // right. The dim gutter carries the line number; no column, the caret
        // goes there anyway.
        let found = crate::grep::occurrences(&l.text, query, sensitive);
        let cut = found.first().map_or(0, |(r, at)| elide_from(&l.text, *at, r.start));
        let gutter = format!("  :{}", l.line + 1);
        let ellipsis = if cut > 0 { ELLIPSIS } else { "" };
        let text = format!("{gutter}  {ellipsis}{}", &l.text[cut..]);
        // Where `l.text[cut..]` begins in `text`, so a hit's offset in the line
        // shifts into row coordinates. The prefix is ASCII but for the ellipsis,
        // and both are byte-counted, so this is exact either way.
        let base = gutter.len() + 2 + ellipsis.len();
        let mut spans = vec![Span::Dim(2..gutter.len())];
        // The caret column is the first hit's, in *source* coordinates — eliding
        // is a display concern and must not touch it. No hit at all means the
        // filter's plain lowercasing and `occurrences`' count-preserving fold
        // disagreed on some exotic mapping; degrade to the line's start.
        let mut col = 0;
        for (n, (range, at)) in found.into_iter().enumerate() {
            if n == 0 {
                col = l.indent + at;
            }
            // `elide_from` never cuts past the first hit, and hits are ordered.
            spans.push(Span::Hit(base + range.start - cut..base + range.end - cut));
        }
        rows.push(Row {
            text,
            spans,
            item: PickItem::FileLine { path: l.path.clone(), line: l.line, col },
            header: false,
        });
    }
    rows
}

/// Byte offset to start a row's text at so a hit `hit_chars` into the line stays
/// visible: `LEAD_CHARS` of context, snapped forward to the first word start.
/// `0` — show the line from its beginning — when the hit is near enough the
/// front. With no space in the lead-in (one long token, a URL), the cut lands
/// mid-word rather than giving up the elision.
fn elide_from(text: &str, hit_chars: usize, hit_byte: usize) -> usize {
    if hit_chars <= ELIDE_AFTER {
        return 0;
    }
    let back = text
        .char_indices()
        .nth(hit_chars - LEAD_CHARS)
        .map_or(0, |(b, _)| b);
    // Search only up to the hit, so the snap can never skip past it.
    text[back..hit_byte].find(' ').map_or(back, |i| back + i + 1)
}

/// `ideas  projects` — the note's name, its folder dimmed after it.
fn header_row(root: &Path, path: &Path) -> Row {
    let rel = rel_display(root, path);
    let (dir, name) = rel.rsplit_once('/').unwrap_or(("", rel.as_str()));
    let item = PickItem::File(path.to_path_buf());
    if dir.is_empty() {
        return Row { text: name.to_string(), spans: Vec::new(), item, header: true };
    }
    let text = format!("{name}  {dir}");
    let spans = vec![Span::Dim(name.len() + 2..text.len())];
    Row { text, spans, item, header: true }
}

/// Filter `rows` by `query`, returning matching indices best-first (indices,
/// not displays, so duplicate display strings can't mispick). Empty query
/// passes every index through in original order.
fn filter_rows(query: &str, rows: &[Row]) -> Vec<usize> {
    if query.is_empty() {
        return (0..rows.len()).collect();
    }
    // ponytail: fresh Matcher per keystroke (a few scratch allocs); cache it on
    // Picker if a 10k-note vault ever stutters.
    let mut matcher = Matcher::new(NucleoConfig::DEFAULT.match_paths());
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut buf = Vec::new();
    let mut scored: Vec<(u32, usize)> = rows
        .iter()
        .enumerate()
        .filter_map(|(i, r)| {
            pattern.score(Utf32Str::new(&r.text, &mut buf), &mut matcher).map(|s| (s, i))
        })
        .collect();
    // Best score first; ties keep original row order.
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, i)| i).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grep;

    #[test]
    fn fuzzy_filter_ranks_and_passes_through() {
        let rows: Vec<Row> = ["projects/ideas", "archive/old", "daily/today"]
            .into_iter()
            .map(|n| Row::plain(n.to_string(), PickItem::InsertLink(n.to_string())))
            .collect();
        // a subsequence match ranks first
        assert_eq!(filter_rows("idea", &rows).first(), Some(&0));
        // empty query returns every index, original order
        assert_eq!(filter_rows("", &rows), vec![0, 1, 2]);
    }

    #[test]
    fn content_rows_group_by_file_and_mark_hits() {
        let root = std::env::temp_dir().join("darknotes_picker_rows_test");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("projects")).unwrap();
        std::fs::write(root.join("projects/ideas.md"), "a Widget here\n  and widget again\n")
            .unwrap();
        std::fs::write(root.join("top.md"), "no hits\nwidget\n").unwrap();

        let vault = crate::vault::Vault::scan(&root);
        let lines = grep::snapshot(&vault);
        let hits = grep::find(&lines, "widget", false);
        let rows = content_rows(&lines, &hits, &root, "widget", false);

        // One header per file (folder dimmed after the name), then its matches —
        // folders sort first, so `projects/ideas` leads.
        let text: Vec<&str> = rows.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(
            text,
            vec![
                "ideas  projects",
                "  :1  a Widget here",
                "  :2  and widget again",
                "top",
                "  :2  widget",
            ]
        );
        assert_eq!(rows.iter().map(|r| r.header).collect::<Vec<_>>(), [true, false, false, true, false]);

        // Headers aren't pickable, so a fresh content list starts on row 1.
        let p = Picker { lines, ..Picker::over("grep> ", rows) };
        assert_eq!(p.first_selectable(), 1);

        // The gutter is dimmed and each hit is marked at its byte range in the
        // row's text: `Widget` is 6 bytes at index 8 of "  :1  a Widget here".
        let spans = |i: usize| -> Vec<(bool, Range<usize>)> {
            p.rows[i]
                .spans
                .iter()
                .map(|s| match s {
                    Span::Dim(r) => (false, r.clone()),
                    Span::Hit(r) => (true, r.clone()),
                })
                .collect()
        };
        assert_eq!(spans(1), vec![(false, 2..4), (true, 8..14)]);
        assert_eq!(spans(0), vec![(false, 7..15)]); // the header's dimmed folder
        assert!(spans(3).is_empty()); // a root-level note has no folder to dim

        // The caret column is in *source* coordinates: the second row's line is
        // indented two spaces, so its hit at char 4 of the trimmed text is
        // column 6 of the line.
        match &p.rows[2].item {
            PickItem::FileLine { line, col, .. } => assert_eq!((*line, *col), (1, 6)),
            _ => panic!("a match row picks a FileLine"),
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_far_right_hit_elides_the_line_start() {
        // A hit past `ELIDE_AFTER` chars: the row opens with an ellipsis and a
        // few words of lead-in, and the highlight follows it.
        let lead = "word ".repeat(14); // 70 chars, hit starts at 70
        let line = format!("{lead}widget tail");
        let lines = vec![grep::line_for_test(PathBuf::from("/v/n.md"), 7, &line)];
        let rows = content_rows(&lines, &[0], Path::new("/v"), "widget", false);

        let row = &rows[1]; // rows[0] is the file header
        let text = &row.text;
        assert!(text.starts_with("  :8  …word "), "got {text:?}");
        // Lead-in is whole words and about `LEAD_CHARS` of them.
        let shown = text.split('…').nth(1).unwrap();
        assert_eq!(shown, "word ".repeat(6) + "widget tail");

        // The Hit span must land exactly on `widget` in the row's own text.
        let hit = row
            .spans
            .iter()
            .find_map(|s| match s {
                Span::Hit(r) => Some(r.clone()),
                Span::Dim(_) => None,
            })
            .expect("a hit span");
        assert_eq!(&text[hit], "widget");

        // Eliding is display-only: the caret column is still the source column.
        match &row.item {
            PickItem::FileLine { col, .. } => assert_eq!(*col, 70),
            _ => panic!("a match row picks a FileLine"),
        }

        // A hit near the front leaves the line's start alone.
        let lines = vec![grep::line_for_test(PathBuf::from("/v/n.md"), 0, "a widget here")];
        let rows = content_rows(&lines, &[0], Path::new("/v"), "widget", false);
        assert_eq!(rows[1].text, "  :1  a widget here");

        // Multi-byte lead-in: every cut is a char boundary, or slicing panics.
        let line = format!("{}widget", "café ".repeat(14));
        let lines = vec![grep::line_for_test(PathBuf::from("/v/n.md"), 0, &line)];
        let rows = content_rows(&lines, &[0], Path::new("/v"), "widget", false);
        let row = &rows[1];
        assert!(row.text.contains("…café "), "got {:?}", row.text);
        match row.spans.iter().find_map(|s| match s {
            Span::Hit(r) => Some(r.clone()),
            Span::Dim(_) => None,
        }) {
            Some(hit) => assert_eq!(&row.text[hit], "widget"),
            None => panic!("a hit span"),
        }
    }
}
