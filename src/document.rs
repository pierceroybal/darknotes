use ropey::Rope;
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::markdown::{self, ListContinuation};

/// A caret or selection, in absolute char offsets. `anchor == head` is a bare
/// caret. Plural in `Document` from day one so multi-cursor / helix
/// selection-first land without a rewrite. Phase 2 still keeps exactly one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    /// Fixed end of the selection.
    pub anchor: usize,
    /// Moving end — where the caret renders.
    pub head: usize,
}

impl Selection {
    pub fn caret(at: usize) -> Self {
        Self { anchor: at, head: at }
    }

    // Used once visual mode / selection deletion lands.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    pub fn range(&self) -> std::ops::Range<usize> {
        self.anchor.min(self.head)..self.anchor.max(self.head)
    }
}

/// A cursor movement, parameterized so operators (`d{motion}`) and plain
/// movement share one definition. Geometry lives here, not in the vim grammar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Motion {
    CharLeft,
    CharRight,
    LineUp,
    LineDown,
    WordForward,
    WordBackward,
    WordEnd,
    /// `W`/`B`/`E`: the same geometry over WORDs — whitespace-delimited runs,
    /// so punctuation never breaks one (`dW` takes a whole URL).
    BigWordForward,
    BigWordBackward,
    BigWordEnd,
    /// `cw` target: vim's special case — in a word, the change stops at the
    /// end of the current word (trailing whitespace and the newline stay);
    /// on whitespace it sweeps like `w`. Only the change operator emits it.
    ChangeWord,
    /// `cW` target: `ChangeWord` over WORDs.
    ChangeBigWord,
    /// `{`/`}`: to the previous/next empty line past the current paragraph
    /// (file edge when none). Strict vim: whitespace-only lines are not
    /// boundaries. From a blank line the motion first skips the blank run.
    ParaBackward,
    ParaForward,
    LineStart,
    /// `^`: first non-blank char of the line.
    FirstNonBlank,
    LineEnd,
    FileStart,
    FileEnd,
    /// `{count}G` / `{count}gg`: the line's first non-blank, 1-based and
    /// clamped to the buffer. The count rides in the variant, not the motion's
    /// repeat count, so a bare `G` (file end) stays distinguishable from `1G`.
    GotoLine(usize),
    /// `%`: the bracket matching the first one at or after the caret on its
    /// line. Not an operator target — `d%`'s inclusive-both-ends span has no
    /// representation in the `[min, max)` sweep, and `di(`/`da(` cover the
    /// delete-a-block case better.
    MatchBracket,
    /// `f{char}`/`F{char}`: onto the count-th `char` forward/backward on the
    /// caret's line. Inclusive as an operator target — `dfx` takes the `x`.
    FindChar(char),
    FindCharBack(char),
    /// `t{char}`/`T{char}`: to just before/after the count-th `char` on the
    /// caret's line. Not found → the motion fails and the caret stays.
    TillChar(char),
    TillCharBack(char),
}

impl Motion {
    /// Line-wise vertical moves, which track the goal column instead of the
    /// caret's live column.
    fn is_vertical(self) -> bool {
        matches!(self, Motion::LineUp | Motion::LineDown)
    }

    /// Vim's "jump" motions: the ones that record a jumplist entry, so `Ctrl-O`
    /// can come back. Relative moves (chars, words, `f`/`t`, line ends) don't.
    pub fn is_jump(self) -> bool {
        matches!(
            self,
            Motion::FileStart
                | Motion::FileEnd
                | Motion::GotoLine(_)
                | Motion::ParaBackward
                | Motion::ParaForward
                | Motion::MatchBracket
        )
    }
}

/// A text object: the region around the caret an operator acts on (`diw`,
/// `dap`). `around` (`a` vs `i`) also takes the adjacent whitespace — trailing
/// spaces for a word, trailing blank lines for a paragraph.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextObject {
    Word { around: bool },
    Paragraph { around: bool },
    /// `i"`/`a"`, `i'`, `` i` ``: the run between a pair of `ch` on the caret's
    /// line. `around` takes the quotes too.
    Quote { ch: char, around: bool },
    /// `i(`/`a{`/`i[`/`i<` and their aliases: the innermost `open`/`close`
    /// pair containing the caret, which may span lines. `around` takes the
    /// brackets too.
    Block { open: char, close: char, around: bool },
}

/// The unnamed register: text from the last delete/yank, replayed by `p`/`P`.
/// `linewise` (from `dd`/`yy`) pastes on new lines; charwise pastes inline.
#[derive(Clone, Default)]
struct Register {
    text: String,
    linewise: bool,
}

/// A point-in-time buffer state for undo/redo. Full-rope snapshots — ropey
/// clones are CoW-cheap (shared backing). ponytail: swap for an op-log only if
/// huge buffers make the clones bite.
struct Snapshot {
    rope: Rope,
    caret: usize,
    dirty: bool,
}

/// The text buffer plus its cursors. All positions are absolute char offsets
/// into the rope; convert to `(line, col)` only for rendering.
pub struct Document {
    pub rope: Rope,
    /// `selections[0]` is primary. Phase 2 keeps exactly one.
    pub selections: Vec<Selection>,
    /// Backing file, if any. `None` is an unnamed scratch buffer.
    path: Option<PathBuf>,
    /// Set on every edit, cleared on save.
    dirty: bool,
    /// The backing file existed but has since been deleted out from under
    /// this buffer (an external watcher sets this — see `Editor::poll_fs_events`).
    /// Content is kept as last known; `save` recreates the file and clears it.
    missing: bool,
    /// Hash of the file content as this buffer last read or wrote it
    /// (`open`/`save`). The watcher compares disk against this to tell a
    /// genuine external change from an echo of our own write.
    disk_hash: u64,
    /// Content generation, drawn from a process-wide counter so no two
    /// documents (or states of one document) ever share a value. Bumped on
    /// every content change — including undo/redo, which can restore
    /// `dirty: false` while still changing text. Cheap change detection for
    /// render caches: equal revisions ⇒ identical rope.
    revision: u64,
    /// Last delete/yank, for `p`/`P`.
    register: Register,
    /// States before each change (`u` pops); `redo` is the inverse (`Ctrl-R`).
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    /// Vim's "want" column for vertical motion. `j`/`k` aim for it (clamped to
    /// each line), so passing through short lines doesn't truncate the column.
    /// Any horizontal move or edit resets it to the actual column.
    goal_col: usize,
    /// The last content change, when it touched exactly one line and left the
    /// line count alone: `(revision before, revision after, line)`.
    ///
    /// Both revisions are recorded so a consumer can prove a cached derivation
    /// (the render layer's markdown parse) is *this* buffer's state immediately
    /// before the edit — a buffer switch or any intervening change breaks the
    /// pairing and falls back to recomputing. `None` means "assume everything
    /// changed": multi-line edits, anything crossing a newline, undo/redo.
    last_edit: Option<(u64, u64, usize)>,
    /// `m{a}`–`m{z}`: char offsets, per buffer as in vim (`ma` in two files is
    /// two marks). Uppercase marks are global and live on the editor. Offsets
    /// are raw — an edit above a mark leaves it pointing at a shifted spot, and
    /// a jump to it clamps. Vim adjusts; matching that means routing every
    /// edit's `(offset, delta)` through here.
    marks: HashMap<char, usize>,
}

/// Undo states kept per buffer, matching vim's `undolevels` default. Past this
/// the oldest is dropped — see `checkpoint`.
const UNDO_LEVELS: usize = 1000;

/// Source of `Document::revision` values — see that field's doc.
static REVISION: AtomicU64 = AtomicU64::new(0);

fn next_revision() -> u64 {
    REVISION.fetch_add(1, Ordering::Relaxed) + 1
}

impl Document {
    pub fn new(text: &str) -> Self {
        Self {
            rope: Rope::from_str(text),
            selections: vec![Selection::caret(0)],
            path: None,
            dirty: false,
            missing: false,
            disk_hash: Self::hash_text(text),
            revision: next_revision(),
            register: Register::default(),
            undo: Vec::new(),
            redo: Vec::new(),
            goal_col: 0,
            last_edit: None,
            marks: HashMap::new(),
        }
    }

    /// Open `path` as the backing file. A missing file is not an error — it
    /// opens as an empty buffer that `save` will create (vim-style).
    pub fn open(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        let text = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e),
        };
        Ok(Self {
            rope: Rope::from_str(&text),
            selections: vec![Selection::caret(0)],
            path: Some(path),
            dirty: false,
            missing: false,
            disk_hash: Self::hash_text(&text),
            revision: next_revision(),
            register: Register::default(),
            undo: Vec::new(),
            redo: Vec::new(),
            goal_col: 0,
            last_edit: None,
            marks: HashMap::new(),
        })
    }

    /// Write the buffer to its backing file. No-op for an unnamed buffer.
    pub fn save(&mut self) -> io::Result<()> {
        if let Some(path) = &self.path {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let text = self.rope.to_string();
            std::fs::write(path, &text)?;
            self.dirty = false;
            self.missing = false;
            self.disk_hash = Self::hash_text(&text);
        }
        Ok(())
    }

    /// Adopt `path` as the backing file and write to it (vim `:w <name>`).
    pub fn save_as(&mut self, path: impl Into<PathBuf>) -> io::Result<()> {
        self.path = Some(path.into());
        self.save()
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// The backing file is gone (see the field doc on `missing`).
    pub fn is_missing(&self) -> bool {
        self.missing
    }

    pub fn set_missing(&mut self, missing: bool) {
        self.missing = missing;
    }

    /// Fingerprint used for buffer-vs-disk comparison (see the `disk_hash`
    /// field doc). Callers hash disk content with `hash_text` to compare.
    pub fn disk_hash(&self) -> u64 {
        self.disk_hash
    }

    /// Record a disk state observed but not adopted (the W12 dirty-buffer
    /// case), so duplicate events for the same change don't re-warn.
    pub fn set_disk_hash(&mut self, hash: u64) {
        self.disk_hash = hash;
    }

    pub fn hash_text(text: &str) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut h);
        h.finish()
    }

    /// Content generation — changes iff the rope changed. See the field doc.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Mark a content change: dirty for save tracking, a fresh revision for
    /// render caches. Every rope mutation routes through here (or `restore`).
    ///
    /// Clears `last_edit`, so anything not explicitly reporting a single-line
    /// change is treated as a whole-document one. That's the safe direction: a
    /// missed opportunity costs a full rebuild, a wrong claim renders staleness.
    fn touch(&mut self) {
        self.dirty = true;
        self.revision = next_revision();
        self.last_edit = None;
    }

    /// `touch` for a change confined to `line` that left the line count alone —
    /// see the `last_edit` field doc. Callers must be certain of both: the
    /// render layer reuses every *other* line's markdown parse on this promise.
    fn touch_line(&mut self, line: usize) {
        let before = self.revision;
        self.dirty = true;
        self.revision = next_revision();
        self.last_edit = Some((before, self.revision, line));
    }

    /// The line the last change touched, if it touched only one *and* the
    /// caller's cached derivation (`cached_revision`) is this buffer's state
    /// immediately before that change. Both halves matter — see `last_edit`.
    pub fn single_line_edit(&self, cached_revision: u64) -> Option<usize> {
        match self.last_edit {
            Some((before, after, line))
                if before == cached_revision && after == self.revision =>
            {
                Some(line)
            }
            _ => None,
        }
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Re-point the backing file after a rename/move on disk. Content and
    /// dirty state are untouched; the revision bumps so path-dependent render
    /// caches (markdown styling keys off the extension) rebuild.
    pub fn set_path(&mut self, path: PathBuf) {
        self.path = Some(path);
        self.revision = next_revision();
        // Markdown styling keys off the extension, so every line may restyle.
        self.last_edit = None;
    }

    /// Whether markdown-aware editing conveniences (list continuation, list-line
    /// Tab, span styling) apply: true for `.md` files and pathless scratch
    /// buffers (no file yet, destined to become a note), false for any other
    /// real extension.
    pub fn is_markdown(&self) -> bool {
        self.path
            .as_deref()
            .map_or(true, |p| p.extension().is_some_and(|e| e == "md"))
    }

    fn caret(&self) -> usize {
        self.selections[0].head
    }

    /// Absolute char offset of the primary caret.
    pub fn caret_offset(&self) -> usize {
        self.caret()
    }

    /// Place the caret at an absolute char offset, clamped to the buffer.
    pub fn jump_to(&mut self, at: usize) {
        self.set_caret(at.min(self.rope.len_chars()));
    }

    /// `m{a}`: name `at` in this buffer (see the `marks` field doc).
    pub fn set_mark(&mut self, name: char, at: usize) {
        self.marks.insert(name, at);
    }

    /// A lowercase mark's offset, if set in this buffer.
    pub fn mark(&self, name: char) -> Option<usize> {
        self.marks.get(&name).copied()
    }

    fn set_caret(&mut self, at: usize) {
        self.selections[0] = Selection::caret(at);
        self.goal_col = self.line_col_of(at).1;
    }

    pub fn insert(&mut self, text: &str) {
        let at = self.caret();
        // Text carrying a newline splits a line, so the line count changes and
        // only a whole-document reparse is safe.
        let line = (!text.contains('\n')).then(|| self.rope.char_to_line(at));
        self.rope.insert(at, text);
        match line {
            Some(line) => self.touch_line(line),
            None => self.touch(),
        }
        self.set_caret(at + text.chars().count());
    }

    /// Smart newline: continue a markdown list when the caret's line is a list
    /// item, else a plain newline. The list prefix lands at the caret, so the
    /// text after it follows the new marker (splitting an item mid-line works).
    /// `clear_empty` (Enter, not `o`) steps an empty item out of the list
    /// instead of repeating its marker: an indented item dedents by `width`
    /// (one level per press, marker kept), a top-level one erases the marker,
    /// leaving the empty line.
    pub fn insert_newline(&mut self, clear_empty: bool, width: usize) {
        if !self.is_markdown() {
            self.insert("\n");
            return;
        }
        let (line, _) = self.line_col_of(self.caret());
        let text: String = self.rope.line(line).chars().filter(|&c| c != '\n').collect();
        match markdown::list_continuation(&text) {
            ListContinuation::Item { empty, .. } if empty && clear_empty => {
                if text.starts_with(' ') {
                    self.indent(width, true);
                    return;
                }
                let start = self.rope.line_to_char(line);
                self.rope.remove(start..start + self.line_len_chars(line));
                self.touch();
                self.set_caret(start);
            }
            ListContinuation::Item { prefix, .. } => {
                self.insert(&format!("\n{prefix}"));
                self.renumber_block(line + 1, width); // mid-list: items below shift down
            }
            ListContinuation::Plain => self.insert("\n"),
        }
    }

    /// Insert-mode Tab (`dedent` = Shift-Tab). On a list item, Tab shifts the
    /// whole line right by `width` spaces and Shift-Tab outdents it — so a bullet
    /// nests regardless of caret column, the caret riding with the content. Off a
    /// list item, Tab inserts `width` spaces at the caret; Shift-Tab still trims
    /// leading whitespace.
    pub fn indent(&mut self, width: usize, dedent: bool) {
        let caret = self.caret();
        let (line, col) = self.line_col_of(caret);
        let start = self.rope.line_to_char(line);
        let text: String = self.rope.line(line).chars().filter(|&c| c != '\n').collect();

        if dedent {
            let lead = text.chars().take_while(|&c| c == ' ').count().min(width);
            if lead > 0 {
                self.rope.remove(start..start + lead);
                self.touch();
                self.set_caret(start + col.saturating_sub(lead));
            }
        } else if self.is_markdown() && markdown::is_list_item(&text) {
            self.rope.insert(start, &" ".repeat(width));
            self.touch();
            self.set_caret(caret + width);
        } else {
            self.insert(&" ".repeat(width));
        }
        if self.is_markdown() {
            self.renumber_block(line, width);
        }
    }

    /// Renumber ordered items in the contiguous list block around `line`.
    /// The block is bounded by lines that are neither list items nor indented
    /// continuation text (blank lines end it, so loose lists renumber only up
    /// to the gap). Items whose leading spaces fall in the same `width` bucket
    /// are siblings: a run at the block's base depth counts up from its first
    /// item's number, nested runs restart at 1. An unordered sibling or a
    /// depth change breaks the run; `.`/`)` punctuation is kept per item. The
    /// caret keeps its place in its line's content when digit widths change.
    /// Returns the block's last line; no-op (returning `line`) off a list item.
    pub fn renumber_block(&mut self, line: usize, width: usize) -> usize {
        let text = |d: &Self, l: usize| markdown::line_text(&d.rope, l);
        let in_block =
            |t: &str| markdown::is_list_item(t) || (t.starts_with(' ') && !t.trim().is_empty());
        if !markdown::is_list_item(&text(self, line)) {
            return line;
        }
        let mut start = line;
        while start > 0 && in_block(&text(self, start - 1)) {
            start -= 1;
        }
        let last = self.rope.len_lines().saturating_sub(1);
        let mut end = line;
        while end < last && in_block(&text(self, end + 1)) {
            end += 1;
        }

        let width = width.max(1);
        let (cline, mut ccol) = self.line_col_of(self.caret());
        let mut base_depth: Option<usize> = None;
        // counters[depth]: Some(last number written) while an ordered run is live.
        let mut counters: Vec<Option<u64>> = Vec::new();
        for l in start..=end {
            let t = text(self, l);
            if !markdown::is_list_item(&t) {
                continue; // continuation text under an item: numbering unaffected
            }
            let lead = t.chars().take_while(|&c| c == ' ').count();
            let depth = lead / width;
            let base = *base_depth.get_or_insert(depth);
            counters.truncate(depth + 1);
            counters.resize(depth + 1, None);
            let Some((n, digits)) = markdown::ordered_item(&t) else {
                counters[depth] = None; // unordered sibling breaks the run
                continue;
            };
            let next = match counters[depth] {
                Some(prev) => prev.saturating_add(1),
                None if depth == base => n, // base run keeps its first item's number
                None => 1,
            };
            counters[depth] = Some(next);
            if next != n {
                let at = self.rope.line_to_char(l) + lead;
                self.rope.remove(at..at + digits);
                let new = next.to_string();
                self.rope.insert(at, &new);
                self.touch();
                if l == cline && ccol >= lead + digits {
                    ccol = ccol + new.len() - digits; // all-ASCII: len == chars
                }
            }
        }
        self.set_caret(self.offset_in_line(cline, ccol));
        end
    }

    /// Flip the GFM task box on `line` (`[ ]` ↔ `[x]`; `[X]` unchecks).
    /// Markdown-only, like the other list conveniences. The caret stays put —
    /// one char replaces one char, so every offset stays valid. Returns
    /// whether a box was found and flipped.
    pub fn toggle_task(&mut self, line: usize) -> bool {
        if !self.is_markdown() || line >= self.rope.len_lines() {
            return false;
        }
        let text: String = self.rope.line(line).chars().filter(|&c| c != '\n').collect();
        let Some((at, checked)) = markdown::task_box(&text) else {
            return false;
        };
        // `at` is the `[`'s byte offset; the flipped char sits one past it.
        let inner = self.rope.line_to_char(line) + text[..at].chars().count() + 1;
        self.rope.remove(inner..inner + 1);
        self.rope.insert(inner, if checked { " " } else { "x" });
        self.touch_line(line);
        true
    }

    /// Backspace: remove the char before the caret (crosses lines).
    pub fn delete_backward(&mut self) {
        let at = self.caret();
        if at == 0 {
            return;
        }
        // Backspacing a newline joins two lines — a line-count change, so no
        // single-line claim (see `touch_line`).
        let line = (self.rope.char(at - 1) != '\n').then(|| self.rope.char_to_line(at - 1));
        self.rope.remove(at - 1..at);
        match line {
            Some(line) => self.touch_line(line),
            None => self.touch(),
        }
        self.set_caret(at - 1);
    }

    /// Forward-delete the char at the caret (crosses lines).
    pub fn delete_forward(&mut self) {
        let at = self.caret();
        if at < self.rope.len_chars() {
            let line = (self.rope.char(at) != '\n').then(|| self.rope.char_to_line(at));
            self.rope.remove(at..at + 1);
            match line {
                Some(line) => self.touch_line(line),
                None => self.touch(),
            }
        }
    }

    pub fn move_motion(&mut self, m: Motion, count: usize) {
        let target = self.motion_target(m, self.caret(), count);
        // Vertical motion preserves the goal column; set_caret would reset it.
        if m.is_vertical() {
            self.selections[0] = Selection::caret(target);
        } else {
            self.set_caret(target);
        }
    }

    /// Visual mode: move the selection's head, leaving the anchor fixed so the
    /// span grows/shrinks.
    pub fn extend_motion(&mut self, m: Motion, count: usize) {
        let head = self.selections[0].head;
        let target = self.motion_target(m, head, count);
        self.selections[0].head = target;
        // Mirror move_motion: vertical keeps the goal column, else it's redefined.
        if !m.is_vertical() {
            self.goal_col = self.line_col_of(target).1;
        }
    }

    /// Collapse the primary selection to a bare caret at its head (leaving visual).
    pub fn collapse_selection(&mut self) {
        self.set_caret(self.selections[0].head);
    }

    /// Char range `[start, end)` the primary selection covers. Charwise is
    /// inclusive of the char under the head (vim semantics); linewise rounds out
    /// to whole lines.
    pub fn selection_span(&self, linewise: bool) -> (usize, usize) {
        let r = self.selections[0].range(); // r.end == max(anchor, head)
        if linewise {
            let l0 = self.rope.char_to_line(r.start);
            let l1 = self.rope.char_to_line(r.end);
            let start = self.rope.line_to_char(l0);
            let end = if l1 + 1 >= self.rope.len_lines() {
                self.rope.len_chars()
            } else {
                self.rope.line_to_char(l1 + 1)
            };
            (start, end)
        } else {
            (r.start, (r.end + 1).min(self.rope.len_chars()))
        }
    }

    /// Visual `d`/`x`, or `c` when `change`: delete the selection into the
    /// register, then drop the caret on a real char of the resulting line. A
    /// linewise change spares the span's last newline, leaving one empty line
    /// for the insert that follows (vim `Vc`, same rule as `cip`), and leaves
    /// the caret there rather than snapping it onto a char.
    pub fn delete_selection(&mut self, linewise: bool, change: bool) {
        let (start, mut end) = self.selection_span(linewise);
        if start < end {
            self.set_register(self.rope.slice(start..end).to_string(), linewise);
            if change && linewise && self.rope.char(end - 1) == '\n' {
                end -= 1;
            }
            self.rope.remove(start..end);
            self.touch();
        }
        let at = start.min(self.rope.len_chars());
        if change {
            self.set_caret(at);
            return;
        }
        let (line, _) = self.line_col_of(at);
        let line_start = self.rope.line_to_char(line);
        let last_col = self.line_len_chars(line).saturating_sub(1);
        self.set_caret(line_start + (at - line_start).min(last_col));
    }

    /// Visual `y`: copy the selection into the register; caret drops to its start.
    pub fn yank_selection(&mut self, linewise: bool) {
        let (start, end) = self.selection_span(linewise);
        if start < end {
            self.set_register(self.rope.slice(start..end).to_string(), linewise);
        }
        self.set_caret(start.min(self.rope.len_chars()));
    }

    /// Visual `>`/`<`: shift every selected line, collapsing the selection.
    pub fn indent_selection(&mut self, width: usize, dedent: bool) {
        let r = self.selections[0].range();
        let l0 = self.rope.char_to_line(r.start);
        let l1 = self.rope.char_to_line(r.end);
        self.indent_lines(l0, l1, width, dedent);
    }

    /// Shift lines `l0..=l1` (`l1` clamps to the buffer) right by `width`
    /// spaces, or left by up to `width` leading spaces (`dedent`). Empty
    /// lines stay put (vim behavior). Drops the caret on the first line's
    /// first non-blank. Backs visual `>`/`<` and normal `>>`/`<<`.
    pub fn indent_lines(&mut self, l0: usize, l1: usize, width: usize, dedent: bool) {
        let l1 = l1.min(self.rope.len_lines().saturating_sub(1));
        // Bottom-up so earlier lines' char offsets stay valid mid-edit.
        for line in (l0..=l1).rev() {
            let start = self.rope.line_to_char(line);
            if self.line_len_chars(line) == 0 {
                continue;
            }
            if dedent {
                let lead =
                    self.rope.line(line).chars().take_while(|&c| c == ' ').count().min(width);
                if lead > 0 {
                    self.rope.remove(start..start + lead);
                    self.touch();
                }
            } else {
                self.rope.insert(start, &" ".repeat(width));
                self.touch();
            }
        }
        if self.is_markdown() {
            let mut l = l0;
            while l <= l1 {
                l = self.renumber_block(l, width) + 1;
            }
        }
        let text: String = self.rope.line(l0).chars().filter(|&c| c != '\n').collect();
        let nb = text.chars().take_while(|c| c.is_whitespace()).count();
        self.set_caret(self.offset_in_line(l0, nb.min(text.chars().count().saturating_sub(1))));
    }

    /// `d{motion}`: delete the char range the motion sweeps over.
    pub fn delete_motion(&mut self, m: Motion, count: usize) {
        let from = self.caret();
        let to = self.op_motion_target(m, from, count);
        let (a, b) = (from.min(to), from.max(to));
        if a < b {
            self.set_register(self.rope.slice(a..b).to_string(), false);
            self.rope.remove(a..b);
            self.touch();
            self.set_caret(a);
        }
    }

    /// `dd`: delete `count` whole lines starting at the caret's line.
    pub fn delete_lines(&mut self, count: usize) {
        let (line, _) = self.line_col_of(self.caret());
        let start = self.rope.line_to_char(line);
        let end_line = line + count.max(1);
        // Deleting through the buffer's last line must also take the newline
        // *before* the range: there is none after it, so leaving the previous
        // one behind keeps a phantom empty last line (and on an already-empty
        // last line the delete would remove nothing at all).
        let (end, del_start) = if end_line >= self.rope.len_lines() {
            (self.rope.len_chars(), start.saturating_sub(1))
        } else {
            (self.rope.line_to_char(end_line), start)
        };
        if del_start < end {
            self.set_register(self.rope.slice(start..end).to_string(), true);
            self.rope.remove(del_start..end);
            self.touch();
        }
        let at = start.min(self.rope.len_chars());
        self.set_caret(self.rope.line_to_char(self.rope.char_to_line(at)));
    }

    /// `dj`/`dk`: delete the caret's line plus `count` lines in a direction
    /// (`up` = `dk`), linewise. A no-op when there's no line in that direction —
    /// vim treats `dj` on the last line and `dk` on the first as a failed motion
    /// rather than deleting the lone line.
    pub fn delete_lines_dir(&mut self, count: usize, up: bool) {
        let count = count.max(1);
        let (line, _) = self.line_col_of(self.caret());
        let last = self.rope.len_lines().saturating_sub(1);
        if (up && line == 0) || (!up && line == last) {
            return;
        }
        let (first, last_del) = if up {
            (line.saturating_sub(count), line)
        } else {
            (line, (line + count).min(last))
        };
        let start = self.rope.line_to_char(first);
        // Same last-line rule as `delete_lines`: take the preceding newline.
        let (end, del_start) = if last_del >= last {
            (self.rope.len_chars(), start.saturating_sub(1))
        } else {
            (self.rope.line_to_char(last_del + 1), start)
        };
        if del_start < end {
            self.set_register(self.rope.slice(start..end).to_string(), true);
            self.rope.remove(del_start..end);
            self.touch();
        }
        let at = start.min(self.rope.len_chars());
        self.set_caret(self.rope.line_to_char(self.rope.char_to_line(at)));
    }

    /// Char span `[start, end)` of a text object at the caret, plus whether it
    /// is linewise (paragraphs are, words aren't). `None` when there's nothing
    /// under the caret (empty line for a word, empty buffer for a paragraph).
    fn object_span(&self, obj: TextObject) -> Option<(usize, usize, bool)> {
        match obj {
            TextObject::Word { around } => self.word_span(around).map(|(a, b)| (a, b, false)),
            TextObject::Paragraph { around } => {
                self.paragraph_span(around).map(|(a, b)| (a, b, true))
            }
            TextObject::Quote { ch, around } => {
                self.quote_span(ch, around).map(|(a, b)| (a, b, false))
            }
            TextObject::Block { open, close, around } => {
                self.block_span(open, close, around).map(|(a, b)| (a, b, false))
            }
        }
    }

    /// `i"`/`a"`: the run between a pair of `q` on the caret's line. Quotes are
    /// paired left to right and the first pair reaching the caret wins, so a
    /// caret before the opening quote still selects that pair (vim). `around`
    /// takes the quotes themselves.
    ///
    /// ponytail: no escape handling — `\"` inside a string closes the pair.
    /// Track the backslash if real notes hit it.
    fn quote_span(&self, q: char, around: bool) -> Option<(usize, usize)> {
        let (line, col) = self.line_col_of(self.caret());
        let base = self.rope.line_to_char(line);
        let n = self.line_len_chars(line);
        let text = self.rope.line(line);
        let mut i = 0;
        while i < n {
            if text.char(i) != q {
                i += 1;
                continue;
            }
            // Unterminated opener: no pair on this line at all.
            let close = (i + 1..n).find(|&j| text.char(j) == q)?;
            if col <= close {
                return if around {
                    Some((base + i, base + close + 1))
                } else {
                    (i + 1 < close).then_some((base + i + 1, base + close))
                };
            }
            i = close + 1;
        }
        None
    }

    /// `i(`/`a(`: the innermost `open`/`close` pair containing the caret,
    /// spanning lines if need be. A caret sitting on either bracket uses that
    /// pair (vim). `around` takes the brackets themselves; `i` on an empty pair
    /// selects nothing, so the operator no-ops.
    fn block_span(&self, open: char, close: char, around: bool) -> Option<(usize, usize)> {
        let len = self.rope.len_chars();
        let p = self.caret();
        let start = if p < len && self.rope.char(p) == open {
            p
        } else {
            // Scan back for an open with no matching close between it and the
            // caret. A caret on the closing bracket resolves here too: the
            // pairs between balance out.
            let mut depth = 0usize;
            let mut i = p;
            loop {
                if i == 0 {
                    return None;
                }
                i -= 1;
                let c = self.rope.char(i);
                if c == close {
                    depth += 1;
                } else if c == open {
                    if depth == 0 {
                        break i;
                    }
                    depth -= 1;
                }
            }
        };
        let mut depth = 0usize;
        let mut j = start + 1;
        let end = loop {
            if j >= len {
                return None;
            }
            let c = self.rope.char(j);
            if c == open {
                depth += 1;
            } else if c == close {
                if depth == 0 {
                    break j;
                }
                depth -= 1;
            }
            j += 1;
        };
        if around {
            Some((start, end + 1))
        } else {
            (start + 1 < end).then_some((start + 1, end))
        }
    }

    /// Visual-mode text object (`viw`, `va(`): set the selection to the
    /// object's span. The head sits on the span's last char, since a charwise
    /// selection includes it.
    pub fn select_object(&mut self, obj: TextObject) {
        let Some((start, end, _)) = self.object_span(obj) else {
            return;
        };
        let head = end.saturating_sub(1).max(start);
        self.selections[0] = Selection { anchor: start, head };
        self.goal_col = self.line_col_of(head).1;
    }

    /// `iw`/`aw`: the same-class run under the caret (a whitespace run counts
    /// as its own "word", per vim). Never crosses the line. `around` adds the
    /// trailing spaces — or the leading ones when none trail; from whitespace
    /// it adds the following word instead.
    fn word_span(&self, around: bool) -> Option<(usize, usize)> {
        let len = self.rope.len_chars();
        let p = self.caret();
        if p >= len || self.rope.char(p) == '\n' {
            return None;
        }
        let is_ws = |c: char| c != '\n' && c.is_whitespace();
        let c0 = self.rope.char(p);
        let same_run = |c: char| {
            if c == '\n' {
                false
            } else if is_ws(c0) {
                is_ws(c)
            } else {
                !c.is_whitespace() && char_class(c) == char_class(c0)
            }
        };
        let mut start = p;
        while start > 0 && same_run(self.rope.char(start - 1)) {
            start -= 1;
        }
        let mut end = p + 1;
        while end < len && same_run(self.rope.char(end)) {
            end += 1;
        }
        if around {
            if is_ws(c0) {
                // From whitespace, `aw` takes the spaces plus the word after.
                if end < len && !self.rope.char(end).is_whitespace() {
                    let cls = char_class(self.rope.char(end));
                    while end < len {
                        let c = self.rope.char(end);
                        if c.is_whitespace() || char_class(c) != cls {
                            break;
                        }
                        end += 1;
                    }
                }
            } else {
                let e = end + self.rope.chars_at(end).take_while(|&c| is_ws(c)).count();
                if e > end {
                    end = e;
                } else {
                    while start > 0 && is_ws(self.rope.char(start - 1)) {
                        start -= 1;
                    }
                }
            }
        }
        Some((start, end))
    }

    /// `ip`/`ap`: the block of contiguous non-blank lines around the caret's
    /// line (or of blank lines, when the caret sits on one). `around` adds the
    /// trailing blank lines — or the leading ones when none trail; from a
    /// blank block it adds the following paragraph instead.
    fn paragraph_span(&self, around: bool) -> Option<(usize, usize)> {
        let blank = |l: usize| self.line_is_blank(l);
        let last_line = self.rope.len_lines().saturating_sub(1);
        let (line, _) = self.line_col_of(self.caret());
        let on_blank = blank(line);
        let mut first = line;
        while first > 0 && blank(first - 1) == on_blank {
            first -= 1;
        }
        let mut last = line;
        while last < last_line && blank(last + 1) == on_blank {
            last += 1;
        }
        if around {
            let mut l = last;
            while l < last_line && blank(l + 1) != on_blank {
                l += 1;
            }
            if l > last {
                last = l;
            } else if !on_blank {
                while first > 0 && blank(first - 1) {
                    first -= 1;
                }
            }
        }
        let start = self.rope.line_to_char(first);
        let end = if last >= last_line {
            self.rope.len_chars()
        } else {
            self.rope.line_to_char(last + 1)
        };
        (start < end).then_some((start, end))
    }

    /// `d{object}`, or `c{object}` when `change`: delete the object's span into
    /// the register. A change on a linewise object spares the span's last
    /// newline, leaving one empty line for the insert that follows (vim `cip`).
    pub fn delete_object(&mut self, obj: TextObject, change: bool) {
        let Some((start, mut end, linewise)) = self.object_span(obj) else {
            return;
        };
        self.set_register(self.rope.slice(start..end).to_string(), linewise);
        let mut del_start = start;
        if linewise && change {
            if self.rope.char(end - 1) == '\n' {
                end -= 1;
            }
        } else if linewise && end >= self.rope.len_chars() && start > 0 {
            // Deleting through EOF takes the newline *before* the span too,
            // else a phantom empty last line remains (same rule as `dd`).
            del_start = start - 1;
        }
        if del_start < end {
            self.rope.remove(del_start..end);
            self.touch();
        }
        let at = start.min(self.rope.len_chars());
        // A linewise delete lands the caret at line start (like `dd`); charwise
        // stays at the span start, the normal-mode clamp snapping it if needed.
        self.set_caret(if linewise && !change {
            self.rope.line_to_char(self.rope.char_to_line(at))
        } else {
            at
        });
    }

    /// `y{object}`: copy the object's span into the register. Charwise drops
    /// the caret to the span start (like `y{motion}`); linewise stays (like `yy`).
    pub fn yank_object(&mut self, obj: TextObject) {
        let Some((start, end, linewise)) = self.object_span(obj) else {
            return;
        };
        self.set_register(self.rope.slice(start..end).to_string(), linewise);
        if !linewise {
            self.set_caret(start);
        }
    }

    /// `x`: delete `count` chars at the caret, not past end-of-line.
    pub fn delete_char_under(&mut self, count: usize) {
        let from = self.caret();
        let (line, _) = self.line_col_of(from);
        let start = self.rope.line_to_char(line);
        let line_end = start + self.line_len_chars(line);
        let to = (from + count.max(1)).min(line_end);
        if from < to {
            self.set_register(self.rope.slice(from..to).to_string(), false);
            self.rope.remove(from..to);
            self.touch_line(line);
        }
        // The caret stays at the deletion point — which may now be past the
        // line's last char. `s` needs it there (insert continues at that spot);
        // for `x` the editor snaps it back with the normal-mode clamp.
        self.set_caret(from);
    }

    /// `r{char}`: overwrite `count` chars at the caret with `char`. Vim fails
    /// the whole command when the line is too short rather than replacing what
    /// fits. The caret stays on the last replaced char.
    pub fn replace_char(&mut self, ch: char, count: usize) {
        let count = count.max(1);
        let from = self.caret();
        let (line, _) = self.line_col_of(from);
        let eol = self.rope.line_to_char(line) + self.line_len_chars(line);
        if from + count > eol {
            return;
        }
        self.rope.remove(from..from + count);
        self.rope.insert(from, &std::iter::repeat(ch).take(count).collect::<String>());
        self.touch_line(line);
        self.set_caret(from + count - 1);
    }

    /// `~`: flip the case of `count` chars at the caret and step past them,
    /// stopping at end of line. Case mapping keeps one char per char (`ß`
    /// stays `ß`) so offsets — carets, selections — survive the edit.
    pub fn toggle_case(&mut self, count: usize) {
        let from = self.caret();
        let (line, _) = self.line_col_of(from);
        let eol = self.rope.line_to_char(line) + self.line_len_chars(line);
        let to = (from + count.max(1)).min(eol);
        if from >= to {
            return;
        }
        let flipped: String = self
            .rope
            .slice(from..to)
            .chars()
            .map(|c| {
                let mut flip =
                    if c.is_uppercase() { c.to_lowercase().collect::<Vec<_>>() } else {
                        c.to_uppercase().collect::<Vec<_>>()
                    };
                if flip.len() == 1 { flip.pop().unwrap() } else { c }
            })
            .collect();
        self.rope.remove(from..to);
        self.rope.insert(from, &flipped);
        self.touch_line(line);
        // Past the last flipped char; the normal-mode clamp pulls it back at EOL.
        self.set_caret(to);
    }

    /// `J`/`gJ`: join the caret's line with the ones below it — `count` names
    /// lines, so both `J` and `2J` make one join. `space` (plain `J`) collapses
    /// each joint to a single space, dropping the next line's indent; `gJ`
    /// splices verbatim. The caret lands on the last joint.
    ///
    /// ponytail: vim also suppresses the space before a `)`; add that rule if
    /// joining wrapped code in notes makes it show.
    pub fn join_lines(&mut self, count: usize, space: bool) {
        let (line, _) = self.line_col_of(self.caret());
        let mut caret = self.caret();
        for _ in 0..count.max(2) - 1 {
            if line + 1 >= self.rope.len_lines() {
                break;
            }
            let start = self.rope.line_to_char(line);
            let eol = start + self.line_len_chars(line);
            let mut end = self.rope.line_to_char(line + 1);
            if space {
                let n = self.line_len_chars(line + 1);
                end += self.rope.line(line + 1).chars().take(n).take_while(|c| *c == ' ').count();
            }
            self.rope.remove(eol..end);
            // No separator for an empty line on either side, nor a second
            // space when the joint already has one.
            let joined_blank = eol >= self.rope.len_chars() || self.rope.char(eol) == '\n';
            if space && eol > start && !joined_blank && self.rope.char(eol - 1) != ' ' {
                self.rope.insert(eol, " ");
            }
            self.touch();
            caret = eol;
        }
        self.set_caret(caret.min(self.rope.len_chars()));
    }

    /// Snap a caret sitting one past the line's last char back onto it. Normal
    /// mode disallows that column (insert mode needs it for appending), so the
    /// editor calls this after every keystroke that lands in normal mode.
    ///
    /// The snap is a view-legality correction, not a horizontal move, so it
    /// preserves the goal column — vim keeps `curswant` across clamps, which
    /// is what lets `j`/`k` pass a short line and return to their column.
    pub fn clamp_caret_to_line(&mut self) {
        let (line, col) = self.line_col_of(self.caret());
        let len = self.line_len_chars(line);
        if col >= len && col > 0 {
            let goal = self.goal_col;
            self.set_caret(self.rope.line_to_char(line) + len - 1);
            self.goal_col = goal;
        }
    }

    /// Unnamed-register contents, for mirroring to the system clipboard.
    pub fn register_text(&self) -> &str {
        &self.register.text
    }

    /// Stash text in the unnamed register. Linewise text is normalized to end in
    /// a newline so paste can treat it as whole lines regardless of EOF quirks.
    /// Also the entry point for loading external (system-clipboard) text.
    pub fn set_register(&mut self, text: String, linewise: bool) {
        let text = if linewise && !text.ends_with('\n') {
            format!("{text}\n")
        } else {
            text
        };
        self.register = Register { text, linewise };
    }

    /// `y{motion}`: copy the char range into the register (charwise). Caret moves
    /// to the range start, as in vim.
    pub fn yank_motion(&mut self, m: Motion, count: usize) {
        let from = self.caret();
        let to = self.op_motion_target(m, from, count);
        let (a, b) = (from.min(to), from.max(to));
        if a < b {
            self.set_register(self.rope.slice(a..b).to_string(), false);
            self.set_caret(a);
        }
    }

    /// `yy`: copy `count` whole lines into the register (linewise). Caret stays.
    pub fn yank_lines(&mut self, count: usize) {
        let (line, _) = self.line_col_of(self.caret());
        let start = self.rope.line_to_char(line);
        let end_line = (line + count.max(1)).min(self.rope.len_lines());
        let end = if end_line >= self.rope.len_lines() {
            self.rope.len_chars()
        } else {
            self.rope.line_to_char(end_line)
        };
        self.set_register(self.rope.slice(start..end).to_string(), true);
    }

    /// `p`/`P`: insert the register. Linewise pastes below (`after`) or above the
    /// current line; charwise pastes after the caret char (`after`) or at it.
    pub fn paste(&mut self, after: bool) {
        if self.register.text.is_empty() {
            return;
        }
        let text = self.register.text.clone();
        let caret = self.caret();
        let (line, _) = self.line_col_of(caret);
        let line_start = self.rope.line_to_char(line);

        let (at, payload, new_caret) = if self.register.linewise {
            let eol = line_start + self.line_len_chars(line);
            if !after {
                (line_start, text, line_start) // paste above this line
            } else if eol < self.rope.len_chars() {
                (eol + 1, text, eol + 1) // after the line's newline
            } else {
                // Last line with no trailing newline: prepend one to split.
                (eol, format!("\n{text}"), eol + 1)
            }
        } else {
            let line_end = line_start + self.line_len_chars(line);
            let at = if after { (caret + 1).min(line_end) } else { caret };
            let last = at + text.chars().count().saturating_sub(1); // vim lands on last pasted char
            (at, text, last)
        };

        self.rope.insert(at, &payload);
        self.touch();
        self.set_caret(new_caret.min(self.rope.len_chars()));
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot { rope: self.rope.clone(), caret: self.caret(), dirty: self.dirty }
    }

    fn restore(&mut self, s: Snapshot) {
        self.rope = s.rope;
        // The snapshot's dirty flag comes back verbatim (undo to the saved
        // state is clean), but the content still changed — new revision.
        self.dirty = s.dirty;
        self.revision = next_revision();
        // An undo can move any number of lines; no single-line claim.
        self.last_edit = None;
        self.set_caret(s.caret.min(self.rope.len_chars()));
    }

    /// Record the pre-change state. The editor calls this once per undoable unit
    /// (a normal-mode edit, or entering insert — the whole insert session coalesces).
    ///
    /// The stack is capped at `UNDO_LEVELS`, dropping the oldest state past it.
    /// Snapshots share rope structure, so each costs only its diverged nodes —
    /// but without a cap a long session retains every intermediate state for the
    /// process lifetime, and idle RSS climbs with edit count rather than
    /// document size.
    pub fn checkpoint(&mut self) {
        let snap = self.snapshot();
        self.undo.push(snap);
        if self.undo.len() > UNDO_LEVELS {
            self.undo.drain(..self.undo.len() - UNDO_LEVELS);
        }
        self.redo.clear();
    }

    pub fn undo(&mut self) {
        if let Some(prev) = self.undo.pop() {
            let cur = self.snapshot();
            self.redo.push(cur);
            self.restore(prev);
        }
    }

    pub fn redo(&mut self) {
        if let Some(next) = self.redo.pop() {
            let cur = self.snapshot();
            self.undo.push(cur);
            self.restore(next);
        }
    }

    /// 0-based `(line, column)` of the primary caret; column counted in chars.
    pub fn caret_line_col(&self) -> (usize, usize) {
        self.line_col_of(self.caret())
    }

    /// Motion target as swept by an operator. Vim's operator-pending `w` is
    /// special: the final sweep stops at the end of its line instead of the
    /// next line's first word, so `dw`/`yw` on the last word never take the
    /// newline. Plain caret movement uses `motion_target` directly.
    fn op_motion_target(&self, m: Motion, from: usize, count: usize) -> usize {
        match m {
            Motion::WordForward => self.word_sweep_end(from, count.max(1), false),
            Motion::BigWordForward => self.word_sweep_end(from, count.max(1), true),
            // Inclusive forward motions: the sweep covers the char the caret
            // would land on, so `dfx` takes the `x` and `dtx` stops just
            // before it. A failed find returns `from` and stays a no-op.
            Motion::FindChar(_) | Motion::TillChar(_) => {
                let to = self.motion_target(m, from, count);
                if to > from { to + 1 } else { to }
            }
            _ => self.motion_target(m, from, count),
        }
    }

    /// End of an operator's `w` sweep: `next_word_start` repeated, but the
    /// last repeat is clamped to the end of the line it starts on. Starting
    /// on an empty line the sweep takes exactly that line's newline (vim
    /// `dw` there collapses the line).
    fn word_sweep_end(&self, from: usize, count: usize, big: bool) -> usize {
        let mut q = from;
        for _ in 1..count {
            q = self.next_word_start(q, big);
        }
        let (line, _) = self.line_col_of(q);
        let eol = self.rope.line_to_char(line) + self.line_len_chars(line);
        let stop = if eol == q { q + 1 } else { eol };
        self.next_word_start(q, big).min(stop)
    }

    /// `cw`/`cW` target — see `Motion::ChangeWord`. Inside a word the change
    /// stops at the run's end; from whitespace it sweeps like an operator's `w`.
    fn change_word_end(&self, from: usize, count: usize, big: bool) -> usize {
        let len = self.rope.len_chars();
        if from >= len || self.rope.char(from).is_whitespace() {
            return self.word_sweep_end(from, count, big);
        }
        // Count > 1 spans whole words; the last stops at its run end.
        let mut p = from;
        for _ in 1..count {
            p = self.next_word_start(p, big);
        }
        if p >= len {
            return len;
        }
        let cls = class_of(big);
        let c0 = cls(self.rope.char(p));
        while p < len {
            let c = self.rope.char(p);
            if c.is_whitespace() || cls(c) != c0 {
                break;
            }
            p += 1;
        }
        p
    }

    /// Offset of the count-th `ch` on the caret's line, searching from just
    /// after `from` (forward) or just before it (backward). `None` when the
    /// line holds fewer than `count` of them — vim fails the motion outright
    /// rather than moving partway.
    fn find_char(&self, from: usize, ch: char, count: usize, forward: bool) -> Option<usize> {
        let (line, col) = self.line_col_of(from);
        let base = self.rope.line_to_char(line);
        let n = self.line_len_chars(line);
        let text = self.rope.line(line);
        let scan: Box<dyn Iterator<Item = usize>> =
            if forward { Box::new(col + 1..n) } else { Box::new((0..col).rev()) };
        scan.filter(|&i| text.char(i) == ch).nth(count.max(1) - 1).map(|i| base + i)
    }

    /// `%`: the offset of the bracket matching the first one at or after the
    /// caret on its line. Nesting-aware over `()`, `[]`, `{}`; no string or
    /// comment awareness, same as vim's plain `%`.
    fn match_bracket(&self, from: usize) -> Option<usize> {
        const PAIRS: [(char, char); 3] = [('(', ')'), ('[', ']'), ('{', '}')];
        let len = self.rope.len_chars();
        let (line, _) = self.line_col_of(from);
        let eol = self.rope.line_to_char(line) + self.line_len_chars(line);
        let mut at = from;
        let (open, close, forward) = loop {
            if at >= eol {
                return None;
            }
            let c = self.rope.char(at);
            if let Some(&(o, cl)) = PAIRS.iter().find(|&&(o, _)| o == c) {
                break (o, cl, true);
            }
            if let Some(&(o, cl)) = PAIRS.iter().find(|&&(_, cl)| cl == c) {
                break (o, cl, false);
            }
            at += 1;
        };
        // Starting on the bracket itself takes depth to 1, so it can't
        // underflow before the match closes it out.
        let mut depth = 0usize;
        if forward {
            for i in at..len {
                match self.rope.char(i) {
                    c if c == open => depth += 1,
                    c if c == close => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(i);
                        }
                    }
                    _ => {}
                }
            }
        } else {
            for i in (0..=at).rev() {
                match self.rope.char(i) {
                    c if c == close => depth += 1,
                    c if c == open => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(i);
                        }
                    }
                    _ => {}
                }
            }
        }
        None
    }

    /// Offset of the first non-blank char on `line` — its end when the line is
    /// blank or all whitespace.
    fn first_non_blank(&self, line: usize) -> usize {
        let n = self.line_len_chars(line);
        let lead =
            self.rope.line(line).chars().take(n).take_while(|c| c.is_whitespace()).count();
        self.rope.line_to_char(line) + lead
    }

    /// Target offset of a motion from `from`, repeated `count` times. Char and
    /// line motions stay within their line's bounds (vim `h`/`l` don't wrap);
    /// word motions cross lines.
    pub fn motion_target(&self, m: Motion, from: usize, count: usize) -> usize {
        let count = count.max(1);
        match m {
            Motion::CharLeft => {
                let (line, col) = self.line_col_of(from);
                self.rope.line_to_char(line) + col.saturating_sub(count)
            }
            Motion::CharRight => {
                let (line, col) = self.line_col_of(from);
                let max = self.line_len_chars(line);
                self.rope.line_to_char(line) + (col + count).min(max)
            }
            Motion::LineUp => {
                let (line, _) = self.line_col_of(from);
                self.offset_in_line(line.saturating_sub(count), self.goal_col)
            }
            Motion::LineDown => {
                let (line, _) = self.line_col_of(from);
                let last = self.rope.len_lines().saturating_sub(1);
                self.offset_in_line((line + count).min(last), self.goal_col)
            }
            Motion::LineStart => {
                let (line, _) = self.line_col_of(from);
                self.rope.line_to_char(line)
            }
            Motion::FirstNonBlank => {
                let (line, _) = self.line_col_of(from);
                self.first_non_blank(line)
            }
            Motion::GotoLine(n) => {
                let last = self.rope.len_lines().saturating_sub(1);
                self.first_non_blank(n.saturating_sub(1).min(last))
            }
            Motion::MatchBracket => self.match_bracket(from).unwrap_or(from),
            Motion::LineEnd => {
                let (line, _) = self.line_col_of(from);
                self.rope.line_to_char(line) + self.line_len_chars(line)
            }
            Motion::FileStart => 0,
            Motion::FileEnd => {
                let last = self.rope.len_lines().saturating_sub(1);
                self.rope.line_to_char(last)
            }
            Motion::ParaForward => {
                let (mut line, _) = self.line_col_of(from);
                let last = self.rope.len_lines().saturating_sub(1);
                for _ in 0..count {
                    while line < last && self.line_is_blank(line) {
                        line += 1;
                    }
                    while line < last && !self.line_is_blank(line) {
                        line += 1;
                    }
                }
                self.rope.line_to_char(line)
            }
            Motion::ParaBackward => {
                let (mut line, _) = self.line_col_of(from);
                for _ in 0..count {
                    while line > 0 && self.line_is_blank(line) {
                        line -= 1;
                    }
                    while line > 0 && !self.line_is_blank(line) {
                        line -= 1;
                    }
                }
                self.rope.line_to_char(line)
            }
            Motion::WordForward | Motion::BigWordForward => {
                let big = m == Motion::BigWordForward;
                let mut p = from;
                for _ in 0..count {
                    p = self.next_word_start(p, big);
                }
                p
            }
            Motion::WordBackward | Motion::BigWordBackward => {
                let big = m == Motion::BigWordBackward;
                let mut p = from;
                for _ in 0..count {
                    p = self.prev_word_start(p, big);
                }
                p
            }
            Motion::WordEnd | Motion::BigWordEnd => {
                let big = m == Motion::BigWordEnd;
                let mut p = from;
                for _ in 0..count {
                    p = self.next_word_end(p, big);
                }
                p
            }
            Motion::ChangeWord => self.change_word_end(from, count, false),
            Motion::ChangeBigWord => self.change_word_end(from, count, true),
            // `t`/`T` land beside the found char; `f`/`F` land on it. A failed
            // find keeps the caret where it is (vim).
            Motion::FindChar(ch) => self.find_char(from, ch, count, true).unwrap_or(from),
            Motion::FindCharBack(ch) => self.find_char(from, ch, count, false).unwrap_or(from),
            Motion::TillChar(ch) => {
                self.find_char(from, ch, count, true).map_or(from, |p| p - 1)
            }
            Motion::TillCharBack(ch) => {
                self.find_char(from, ch, count, false).map_or(from, |p| p + 1)
            }
        }
    }

    fn line_col_of(&self, at: usize) -> (usize, usize) {
        let line = self.rope.char_to_line(at);
        (line, at - self.rope.line_to_char(line))
    }

    /// Char offset of `col` within `line`, clamped to the line's content
    /// (excludes the trailing '\n').
    fn offset_in_line(&self, line: usize, col: usize) -> usize {
        self.rope.line_to_char(line) + col.min(self.line_len_chars(line))
    }

    /// Chars in a line excluding its trailing newline.
    ///
    /// Indexes the last char rather than iterating to it: `Chars::last()` walks
    /// the whole line, which is ~1ns/char and shows up on long ones (a 5k-char
    /// line costs 49× an indexed read). This is called on every keystroke that
    /// lands in normal mode, via `clamp_caret_to_line`.
    ///
    /// Only `\n` is stripped, so a CRLF line keeps its `\r` in the count.
    fn line_len_chars(&self, line: usize) -> usize {
        let slice = self.rope.line(line);
        let n = slice.len_chars();
        if n > 0 && slice.char(n - 1) == '\n' {
            n - 1
        } else {
            n
        }
    }

    /// Whether `line` holds no content — its newline alone, or nothing (the
    /// buffer's last line). The paragraph motions and `ip`/`ap` test this per
    /// line across a range, so it answers in O(log n) instead of measuring the
    /// line's length.
    fn line_is_blank(&self, line: usize) -> bool {
        let slice = self.rope.line(line);
        match slice.len_chars() {
            0 => true,
            1 => slice.char(0) == '\n',
            _ => false,
        }
    }

    /// Start of the next word at/after `from`. Approximates vim `w`: skip the
    /// current same-class run, then skip whitespace. `big` is vim `W` — every
    /// non-blank counts as one class, so punctuation never breaks a run.
    fn next_word_start(&self, from: usize, big: bool) -> usize {
        let len = self.rope.len_chars();
        let mut p = from;
        if p >= len {
            return len;
        }
        let cls = class_of(big);
        let c0 = self.rope.char(p);
        if !c0.is_whitespace() {
            let c0 = cls(c0);
            while p < len {
                let c = self.rope.char(p);
                if c.is_whitespace() || cls(c) != c0 {
                    break;
                }
                p += 1;
            }
        }
        while p < len && self.rope.char(p).is_whitespace() {
            p += 1;
        }
        p
    }

    /// End of the next word after `from` (vim `e`/`E`): always step forward at
    /// least one char, skip whitespace, then land on the last char of that
    /// class run. Stays put when no word follows.
    fn next_word_end(&self, from: usize, big: bool) -> usize {
        let len = self.rope.len_chars();
        let mut p = from + 1;
        while p < len && self.rope.char(p).is_whitespace() {
            p += 1;
        }
        if p >= len {
            return from;
        }
        let cls = class_of(big);
        let c0 = cls(self.rope.char(p));
        while p + 1 < len {
            let next = self.rope.char(p + 1);
            if next.is_whitespace() || cls(next) != c0 {
                break;
            }
            p += 1;
        }
        p
    }

    /// Start of the word before `from`. Approximates vim `b`/`B`.
    fn prev_word_start(&self, from: usize, big: bool) -> usize {
        let mut p = from;
        while p > 0 && self.rope.char(p - 1).is_whitespace() {
            p -= 1;
        }
        if p > 0 {
            let cls = class_of(big);
            let c0 = cls(self.rope.char(p - 1));
            while p > 0 {
                let prev = self.rope.char(p - 1);
                if prev.is_whitespace() || cls(prev) != c0 {
                    break;
                }
                p -= 1;
            }
        }
        p
    }
}

#[derive(PartialEq)]
enum CharClass {
    Word,
    Other,
}

/// Word chars (identifiers) vs. everything else. Whitespace is handled by the
/// callers, so it never reaches here.
fn char_class(c: char) -> CharClass {
    if c.is_alphanumeric() || c == '_' {
        CharClass::Word
    } else {
        CharClass::Other
    }
}

/// The classifier the word motions compare with: per-char for `w`, or a
/// constant for `W`, where every non-blank belongs to one run.
fn class_of(big: bool) -> fn(char) -> CharClass {
    if big {
        |_| CharClass::Word
    } else {
        char_class
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_and_delete() {
        let mut d = Document::new("");
        d.insert("hi");
        assert_eq!(d.rope.to_string(), "hi");
        d.delete_backward();
        assert_eq!(d.rope.to_string(), "h");
    }

    #[test]
    fn vertical_move_clamps_column() {
        // Caret at end of "hello" (col 5), move down onto "hi" (len 2) → clamps.
        let mut d = Document::new("hello\nhi\nworld");
        d.move_motion(Motion::CharRight, 5);
        d.move_motion(Motion::LineDown, 1);
        assert_eq!(d.caret_line_col(), (1, 2));
    }

    #[test]
    fn vertical_move_keeps_goal_column_past_short_line() {
        // Col 4, down through empty line, down to a long line → lands back at 4.
        let mut d = Document::new("abcdef\n\nuvwxyz");
        d.move_motion(Motion::CharRight, 4);
        d.move_motion(Motion::LineDown, 1);
        assert_eq!(d.caret_line_col(), (1, 0)); // clamped to empty line
        d.move_motion(Motion::LineDown, 1);
        assert_eq!(d.caret_line_col(), (2, 4)); // goal column restored
        // A horizontal move redefines the goal column.
        d.move_motion(Motion::CharLeft, 2);
        d.move_motion(Motion::LineUp, 2);
        assert_eq!(d.caret_line_col(), (0, 2));
    }

    #[test]
    fn eol_clamp_keeps_goal_column_sticky() {
        // The editor clamps after every normal-mode keystroke. Passing a
        // 1-char line snaps the caret to its only char (col 0) — but the
        // goal column must survive the snap, or every later j/k sticks to
        // the first column (vim keeps curswant across clamps).
        let mut d = Document::new("aaaa aaaa aaaa\nb\ncccc cccc cccc");
        d.move_motion(Motion::CharRight, 9);
        d.move_motion(Motion::LineDown, 1);
        d.clamp_caret_to_line();
        assert_eq!(d.caret_line_col(), (1, 0)); // snapped onto "b"
        d.move_motion(Motion::LineDown, 1);
        d.clamp_caret_to_line();
        assert_eq!(d.caret_line_col(), (2, 9)); // goal survived the clamp
    }

    #[test]
    fn word_forward() {
        let d = Document::new("foo bar baz");
        assert_eq!(d.motion_target(Motion::WordForward, 0, 1), 4);
        assert_eq!(d.motion_target(Motion::WordForward, 0, 2), 8);
    }

    #[test]
    fn paragraph_motions() {
        // Lines: 0 "one", 1 "two", 2 "", 3 "three", 4 "", 5 "", 6 "four".
        let d = Document::new("one\ntwo\n\nthree\n\n\nfour");
        let at = |l: usize| d.rope.line_to_char(l);
        assert_eq!(d.motion_target(Motion::ParaForward, 0, 1), at(2));
        assert_eq!(d.motion_target(Motion::ParaForward, 0, 2), at(4));
        // From a blank line: past the blank run and the next paragraph.
        assert_eq!(d.motion_target(Motion::ParaForward, at(2), 1), at(4));
        // No boundary left: clamp to the file edge.
        assert_eq!(d.motion_target(Motion::ParaForward, at(6), 1), at(6));
        assert_eq!(d.motion_target(Motion::ParaBackward, at(1), 1), 0);
        assert_eq!(d.motion_target(Motion::ParaBackward, at(6), 1), at(5));
        assert_eq!(d.motion_target(Motion::ParaBackward, at(5), 1), at(2));
    }

    #[test]
    fn word_end() {
        // "foo bar baz": e from start → 'o' (2); from there → 'r' (6); count 2 → 6.
        let d = Document::new("foo bar baz");
        assert_eq!(d.motion_target(Motion::WordEnd, 0, 1), 2);
        assert_eq!(d.motion_target(Motion::WordEnd, 2, 1), 6);
        assert_eq!(d.motion_target(Motion::WordEnd, 0, 2), 6);
        // No word ahead → stays put.
        assert_eq!(d.motion_target(Motion::WordEnd, 10, 1), 10);
    }

    #[test]
    fn delete_word() {
        let mut d = Document::new("foo bar");
        d.delete_motion(Motion::WordForward, 1); // dw at start → "foo " gone
        assert_eq!(d.rope.to_string(), "bar");
    }

    #[test]
    fn dw_stops_at_end_of_line() {
        // dw on the last word of a line: word goes, newline stays.
        let mut d = Document::new("foo bar\nbaz");
        d.move_motion(Motion::CharRight, 4);
        d.delete_motion(Motion::WordForward, 1);
        assert_eq!(d.rope.to_string(), "foo \nbaz");

        // Trailing whitespace goes too, still not the newline.
        let mut d = Document::new("foo  \nbar");
        d.delete_motion(Motion::WordForward, 1);
        assert_eq!(d.rope.to_string(), "\nbar");

        // On an empty line, dw takes the newline: the line collapses.
        let mut d = Document::new("\nbar");
        d.delete_motion(Motion::WordForward, 1);
        assert_eq!(d.rope.to_string(), "bar");

        // With a count, only the final word is clamped to its line.
        let mut d = Document::new("foo\nbar baz");
        d.delete_motion(Motion::WordForward, 2);
        assert_eq!(d.rope.to_string(), "baz");

        // Mid-line dw is untouched: sweeps through to the next word's start.
        let mut d = Document::new("foo bar");
        d.delete_motion(Motion::WordForward, 1);
        assert_eq!(d.rope.to_string(), "bar");
    }

    #[test]
    fn change_word_stops_at_word_end() {
        // cw in a word: trailing space stays.
        let mut d = Document::new("foo bar");
        d.delete_motion(Motion::ChangeWord, 1);
        assert_eq!(d.rope.to_string(), " bar");

        // cw on the last word of a line: the newline stays.
        let mut d = Document::new("foo\nbar");
        d.delete_motion(Motion::ChangeWord, 1);
        assert_eq!(d.rope.to_string(), "\nbar");

        // On the last char of a word, only that char changes (unlike `ce`).
        let mut d = Document::new("foo bar");
        d.move_motion(Motion::CharRight, 2);
        d.delete_motion(Motion::ChangeWord, 1);
        assert_eq!(d.rope.to_string(), "fo bar");

        // 2cw spans a whole word plus the next word's run.
        let mut d = Document::new("foo bar baz");
        d.delete_motion(Motion::ChangeWord, 2);
        assert_eq!(d.rope.to_string(), " baz");

        // On whitespace there's no word to preserve: sweeps like `w`.
        let mut d = Document::new("a  bc");
        d.move_motion(Motion::CharRight, 1);
        d.delete_motion(Motion::ChangeWord, 1);
        assert_eq!(d.rope.to_string(), "abc");
    }

    #[test]
    fn delete_two_lines() {
        let mut d = Document::new("a\nb\nc");
        d.delete_lines(2);
        assert_eq!(d.rope.to_string(), "c");
    }

    #[test]
    fn delete_lines_on_last_line_takes_preceding_newline() {
        // Empty last line (buffer ends in '\n'): dd deletes it, not a no-op.
        let mut d = Document::new("abc\n");
        d.move_motion(Motion::LineDown, 1);
        d.delete_lines(1);
        assert_eq!(d.rope.to_string(), "abc");
        assert_eq!(d.caret_line_col(), (0, 0));

        // Non-empty last line: no phantom empty line left behind.
        let mut d = Document::new("abc\ndef");
        d.move_motion(Motion::LineDown, 1);
        d.delete_lines(1);
        assert_eq!(d.rope.to_string(), "abc");

        // A mid-buffer dd before an empty last line keeps that line.
        let mut d = Document::new("abc\ndef\n");
        d.move_motion(Motion::LineDown, 1);
        d.delete_lines(1);
        assert_eq!(d.rope.to_string(), "abc\n");
    }

    #[test]
    fn find_and_till_motions() {
        // "say (hi) x" — indices: s0 a1 y2 _3 (4 h5 i6 )7 _8 x9
        let mut d = Document::new("say (hi) x");
        // `f` lands on the char, `t` just before it.
        assert_eq!(d.motion_target(Motion::FindChar('('), 0, 1), 4);
        assert_eq!(d.motion_target(Motion::TillChar('('), 0, 1), 3);
        // A count picks the n-th occurrence on the line.
        assert_eq!(d.motion_target(Motion::FindChar(')'), 0, 1), 7);
        // Backward from the tail: `F` onto the char, `T` just after it.
        assert_eq!(d.motion_target(Motion::FindCharBack('('), 9, 1), 4);
        assert_eq!(d.motion_target(Motion::TillCharBack('('), 9, 1), 5);
        // Not found → the motion fails and the caret stays.
        assert_eq!(d.motion_target(Motion::FindChar('z'), 0, 1), 0);
        assert_eq!(d.motion_target(Motion::TillCharBack('z'), 9, 1), 9);
        // A find never leaves the caret's line.
        let d2 = Document::new("ab\ncx");
        assert_eq!(d2.motion_target(Motion::FindChar('x'), 0, 1), 0);
        // Operator sweeps: `dt` stops before the target, `df` takes it.
        d.delete_motion(Motion::TillChar('x'), 1);
        assert_eq!(d.rope.to_string(), "x");
        let mut d = Document::new("say (hi) x");
        d.delete_motion(Motion::FindChar(')'), 1);
        assert_eq!(d.rope.to_string(), " x");
    }

    #[test]
    fn find_operator_sweep_stops_on_the_target() {
        // "say \"hi (there) x\" ok" — the `x` is col 16, the closing quote 17.
        // `dfx` is inclusive of the `x` and nothing past it.
        let mut d = Document::new("say \"hi (there) x\" ok");
        d.delete_motion(Motion::FindChar('x'), 1);
        assert_eq!(d.rope.to_string(), "\" ok");
    }

    #[test]
    fn till_char_adjacent_is_a_noop() {
        // `dtx` with the target already next to the caret deletes nothing —
        // the sweep and the caret coincide.
        let mut d = Document::new("ax");
        d.delete_motion(Motion::TillChar('x'), 1);
        assert_eq!(d.rope.to_string(), "ax");
    }

    #[test]
    fn big_word_motions_span_punctuation() {
        let mut d = Document::new("see http://a.b/c end");
        d.move_motion(Motion::BigWordForward, 1);
        assert_eq!(d.caret_line_col(), (0, 4)); // start of the URL
        // `W` clears the whole URL where `w` would stop at each punctuation run.
        d.move_motion(Motion::BigWordForward, 1);
        assert_eq!(d.caret_line_col(), (0, 17)); // "end"
        d.move_motion(Motion::BigWordBackward, 1);
        assert_eq!(d.caret_line_col(), (0, 4));
        d.move_motion(Motion::BigWordEnd, 1);
        assert_eq!(d.caret_line_col(), (0, 15)); // last char of the URL
        // Contrast: plain `w` breaks the URL at its punctuation.
        d.move_motion(Motion::LineStart, 1);
        d.move_motion(Motion::WordForward, 2);
        assert_eq!(d.caret_line_col(), (0, 8)); // the `:` run
    }

    #[test]
    fn first_non_blank_and_goto_line() {
        let d = Document::new("a\n    bb\nccc\n");
        assert_eq!(d.motion_target(Motion::FirstNonBlank, 5, 1), 6);
        // 1-based, landing on the first non-blank; past the end clamps.
        assert_eq!(d.motion_target(Motion::GotoLine(2), 0, 1), 6);
        assert_eq!(d.motion_target(Motion::GotoLine(1), 6, 1), 0);
        assert_eq!(d.motion_target(Motion::GotoLine(99), 0, 1), 13);
    }

    #[test]
    fn match_bracket_motion() {
        // Nesting-aware, and it jumps both ways.
        let d = Document::new("f(a(b)c)d");
        assert_eq!(d.motion_target(Motion::MatchBracket, 0, 1), 7); // scans to `(` at 1
        assert_eq!(d.motion_target(Motion::MatchBracket, 3, 1), 5);
        assert_eq!(d.motion_target(Motion::MatchBracket, 7, 1), 1); // from the closer
        // Spans lines, and fails (staying put) with no bracket on the line.
        let d = Document::new("x {\n  y\n}\n");
        assert_eq!(d.motion_target(Motion::MatchBracket, 0, 1), 8);
        assert_eq!(d.motion_target(Motion::MatchBracket, 4, 1), 4);
    }

    #[test]
    fn quote_object_spans() {
        // `ci"` from inside the string.
        let mut d = Document::new("say \"hi there\" ok");
        d.move_motion(Motion::CharRight, 7);
        d.delete_object(TextObject::Quote { ch: '"', around: false }, true);
        assert_eq!(d.rope.to_string(), "say \"\" ok");

        // `da\"` takes the quotes; a caret before the opener still finds the pair.
        let mut d = Document::new("say \"hi\" ok");
        d.delete_object(TextObject::Quote { ch: '"', around: true }, false);
        assert_eq!(d.rope.to_string(), "say  ok");

        // An empty pair has no inner span, so `i` no-ops.
        let mut d = Document::new("a \"\" b");
        d.move_motion(Motion::CharRight, 3);
        d.delete_object(TextObject::Quote { ch: '"', around: false }, false);
        assert_eq!(d.rope.to_string(), "a \"\" b");

        // Unterminated quote: no pair, no edit. And quotes never leave the line.
        let mut d = Document::new("a \"b\nc\" d");
        d.move_motion(Motion::CharRight, 3);
        d.delete_object(TextObject::Quote { ch: '"', around: false }, false);
        assert_eq!(d.rope.to_string(), "a \"b\nc\" d");
    }

    #[test]
    fn block_object_spans() {
        // `di(` from inside, nesting-aware.
        let mut d = Document::new("f(a(b)c)d");
        d.move_motion(Motion::CharRight, 4); // on 'b'
        d.delete_object(TextObject::Block { open: '(', close: ')', around: false }, false);
        assert_eq!(d.rope.to_string(), "f(a()c)d");

        // A caret on either bracket uses that pair; `a` takes the brackets.
        let mut d = Document::new("f(ab)c");
        d.move_motion(Motion::CharRight, 1); // on '('
        d.delete_object(TextObject::Block { open: '(', close: ')', around: true }, false);
        assert_eq!(d.rope.to_string(), "fc");
        let mut d = Document::new("f(ab)c");
        d.move_motion(Motion::CharRight, 4); // on ')'
        d.delete_object(TextObject::Block { open: '(', close: ')', around: false }, false);
        assert_eq!(d.rope.to_string(), "f()c");

        // Spans lines.
        let mut d = Document::new("x {\n  y\n} z");
        d.move_motion(Motion::LineDown, 1);
        d.delete_object(TextObject::Block { open: '{', close: '}', around: false }, false);
        assert_eq!(d.rope.to_string(), "x {} z");

        // Outside any pair: no edit.
        let mut d = Document::new("no brackets");
        d.delete_object(TextObject::Block { open: '(', close: ')', around: false }, false);
        assert_eq!(d.rope.to_string(), "no brackets");
    }

    #[test]
    fn select_object_sets_selection() {
        // `viw` selects the word, head on its last char (charwise is inclusive).
        let mut d = Document::new("foo bar baz");
        d.move_motion(Motion::CharRight, 5); // on 'a' of "bar"
        d.select_object(TextObject::Word { around: false });
        assert_eq!(d.selections[0].anchor, 4);
        assert_eq!(d.selections[0].head, 6);
        // The span the selection yields matches what the operator would take.
        assert_eq!(d.selection_span(false), (4, 7));
    }

    #[test]
    fn join_lines_collapses_indent() {
        // `J`: one space at the joint, the next line's indent dropped.
        let mut d = Document::new("foo\n    bar\nbaz\n");
        d.join_lines(1, true);
        assert_eq!(d.rope.to_string(), "foo bar\nbaz\n");
        assert_eq!(d.caret_offset(), 3); // on the inserted space

        // A count names lines, so `3J` makes two joins.
        let mut d = Document::new("a\nb\nc\nd\n");
        d.join_lines(3, true);
        assert_eq!(d.rope.to_string(), "a b c\nd\n");

        // `gJ` splices verbatim, indent included.
        let mut d = Document::new("foo\n    bar\n");
        d.join_lines(1, false);
        assert_eq!(d.rope.to_string(), "foo    bar\n");

        // No second space when the joint already has one; none for a blank side.
        let mut d = Document::new("foo \nbar\n");
        d.join_lines(1, true);
        assert_eq!(d.rope.to_string(), "foo bar\n");
        let mut d = Document::new("\nbar\n");
        d.join_lines(1, true);
        assert_eq!(d.rope.to_string(), "bar\n");

        // Nothing below: a no-op rather than eating the trailing newline.
        let mut d = Document::new("only");
        d.join_lines(1, true);
        assert_eq!(d.rope.to_string(), "only");
    }

    #[test]
    fn replace_char_needs_room() {
        let mut d = Document::new("abcd\nx");
        d.replace_char('-', 2);
        assert_eq!(d.rope.to_string(), "--cd\nx");
        assert_eq!(d.caret_offset(), 1); // on the last replaced char

        // Vim fails the whole command rather than replacing what fits, and it
        // never runs past the line into the next one.
        let mut d = Document::new("ab\nxy");
        d.replace_char('-', 5);
        assert_eq!(d.rope.to_string(), "ab\nxy");
    }

    #[test]
    fn toggle_case_flips_and_advances() {
        let mut d = Document::new("aB c");
        d.toggle_case(2);
        assert_eq!(d.rope.to_string(), "Ab c");
        assert_eq!(d.caret_offset(), 2);

        // Stops at end of line instead of flipping into the next.
        let mut d = Document::new("ab\ncd");
        d.toggle_case(9);
        assert_eq!(d.rope.to_string(), "AB\ncd");
    }

    #[test]
    fn linewise_change_keeps_a_line_to_type_into() {
        // `Vjc` clears both lines but leaves one empty for the insert.
        let mut d = Document::new("aaa\nbbb\nccc\n");
        d.extend_motion(Motion::LineDown, 1);
        d.delete_selection(true, true);
        assert_eq!(d.rope.to_string(), "\nccc\n");
        assert_eq!(d.caret_offset(), 0);
        // The register still holds the whole span, newline included.
        assert_eq!(d.register_text(), "aaa\nbbb\n");
    }

    #[test]
    fn word_object_spans() {
        // diw mid-word: just the word.
        let mut d = Document::new("foo bar baz");
        d.move_motion(Motion::CharRight, 5); // on 'a' of "bar"
        d.delete_object(TextObject::Word { around: false }, false);
        assert_eq!(d.rope.to_string(), "foo  baz");
        assert_eq!(d.caret_line_col(), (0, 4));

        // daw: word + trailing space.
        let mut d = Document::new("foo bar baz");
        d.move_motion(Motion::CharRight, 5);
        d.delete_object(TextObject::Word { around: true }, false);
        assert_eq!(d.rope.to_string(), "foo baz");

        // daw on the last word: no trailing space → takes the leading one.
        let mut d = Document::new("foo bar");
        d.move_motion(Motion::CharRight, 5);
        d.delete_object(TextObject::Word { around: true }, false);
        assert_eq!(d.rope.to_string(), "foo");

        // iw on whitespace: the whitespace run is the object.
        let mut d = Document::new("foo   bar");
        d.move_motion(Motion::CharRight, 4);
        d.delete_object(TextObject::Word { around: false }, false);
        assert_eq!(d.rope.to_string(), "foobar");

        // Punctuation is its own word class (like vim).
        let mut d = Document::new("foo(bar)");
        d.move_motion(Motion::CharRight, 4); // on 'b'
        d.delete_object(TextObject::Word { around: false }, false);
        assert_eq!(d.rope.to_string(), "foo()");

        // iw never crosses the line.
        let mut d = Document::new("\nfoo");
        d.delete_object(TextObject::Word { around: false }, false); // caret on empty line
        assert_eq!(d.rope.to_string(), "\nfoo");
    }

    #[test]
    fn paragraph_object_spans() {
        // dip on a middle paragraph: its lines only, linewise.
        let mut d = Document::new("aaa\n\nbbb\nccc\n\nddd\n");
        d.move_motion(Motion::LineDown, 2); // on "bbb"
        d.delete_object(TextObject::Paragraph { around: false }, false);
        assert_eq!(d.rope.to_string(), "aaa\n\n\nddd\n");
        assert_eq!(d.caret_line_col(), (2, 0));

        // dap also takes the trailing blank line.
        let mut d = Document::new("aaa\n\nbbb\nccc\n\nddd\n");
        d.move_motion(Motion::LineDown, 2);
        d.delete_object(TextObject::Paragraph { around: true }, false);
        assert_eq!(d.rope.to_string(), "aaa\n\nddd\n");

        // dap on the last paragraph: no trailing blanks → takes the leading ones.
        let mut d = Document::new("aaa\n\nbbb");
        d.move_motion(Motion::LineDown, 2);
        d.delete_object(TextObject::Paragraph { around: true }, false);
        assert_eq!(d.rope.to_string(), "aaa");

        // dip through EOF takes the preceding newline (no phantom last line).
        let mut d = Document::new("aaa\n\nbbb\nccc");
        d.move_motion(Motion::LineDown, 3);
        d.delete_object(TextObject::Paragraph { around: false }, false);
        assert_eq!(d.rope.to_string(), "aaa\n");

        // cip clears the lines but keeps one empty line for the insert.
        let mut d = Document::new("aaa\n\nbbb\nccc\n\nddd\n");
        d.move_motion(Motion::LineDown, 2);
        d.delete_object(TextObject::Paragraph { around: false }, true);
        assert_eq!(d.rope.to_string(), "aaa\n\n\n\nddd\n");
        assert_eq!(d.caret_line_col(), (2, 0));

        // ip on a blank line: the blank block.
        let mut d = Document::new("aaa\n\n\nbbb\n");
        d.move_motion(Motion::LineDown, 1);
        d.delete_object(TextObject::Paragraph { around: false }, false);
        assert_eq!(d.rope.to_string(), "aaa\nbbb\n");

        // The register is linewise: p pastes on new lines.
        let mut d = Document::new("aaa\nbbb\n\nccc\n");
        d.yank_object(TextObject::Paragraph { around: false });
        assert_eq!(d.caret_line_col(), (0, 0)); // linewise yank keeps the caret
        d.move_motion(Motion::LineDown, 3); // on "ccc"
        d.paste(true);
        assert_eq!(d.rope.to_string(), "aaa\nbbb\n\nccc\naaa\nbbb\n");
    }

    #[test]
    fn yank_word_object() {
        let mut d = Document::new("foo bar");
        d.move_motion(Motion::CharRight, 5); // on 'a'
        d.yank_object(TextObject::Word { around: false });
        assert_eq!(d.caret_line_col(), (0, 4)); // caret to span start
        assert_eq!(d.register_text(), "bar");
    }

    #[test]
    fn dj_dk_delete_adjacent_lines() {
        // dj: current line + the one below.
        let mut d = Document::new("a\nb\nc\nd");
        d.move_motion(Motion::LineDown, 1); // line "b"
        d.delete_lines_dir(1, false);
        assert_eq!(d.rope.to_string(), "a\nd");

        // dk: current line + the one above.
        let mut d = Document::new("a\nb\nc\nd");
        d.move_motion(Motion::LineDown, 2); // line "c"
        d.delete_lines_dir(1, true);
        assert_eq!(d.rope.to_string(), "a\nd");

        // Count: 2dj deletes the line plus two below.
        let mut d = Document::new("a\nb\nc\nd");
        d.delete_lines_dir(2, false);
        assert_eq!(d.rope.to_string(), "d");

        // dj on the last line / dk on the first are no-ops (no line to join).
        let mut d = Document::new("a\nb");
        d.move_motion(Motion::LineDown, 1); // last line
        d.delete_lines_dir(1, false);
        assert_eq!(d.rope.to_string(), "a\nb");
        d.move_motion(Motion::FileStart, 1); // first line
        d.delete_lines_dir(1, true);
        assert_eq!(d.rope.to_string(), "a\nb");
    }

    #[test]
    fn line_end_motion() {
        let d = Document::new("hello\nhi");
        assert_eq!(d.motion_target(Motion::LineEnd, 0, 1), 5);
    }

    #[test]
    fn x_deletes_char_under() {
        let mut d = Document::new("abc");
        d.delete_char_under(1);
        assert_eq!(d.rope.to_string(), "bc");
    }

    #[test]
    fn delete_char_under_last_char_leaves_caret_at_line_end() {
        // `s` on the last char: insert must continue where the char was
        // (after "ab"), so the delete does not snap the caret back.
        let mut d = Document::new("abc");
        d.move_motion(Motion::LineEnd, 1);
        d.clamp_caret_to_line(); // caret on 'c', as normal mode has it
        d.delete_char_under(1);
        assert_eq!(d.rope.to_string(), "ab");
        assert_eq!(d.caret_line_col(), (0, 2));
    }

    #[test]
    fn clamp_caret_to_line_snaps_past_end() {
        // `$` targets one past the last char; normal mode snaps onto it.
        let mut d = Document::new("hello\n");
        d.move_motion(Motion::LineEnd, 1);
        assert_eq!(d.caret_line_col(), (0, 5));
        d.clamp_caret_to_line();
        assert_eq!(d.caret_line_col(), (0, 4));
        // No-op mid-line and on an empty line.
        d.clamp_caret_to_line();
        assert_eq!(d.caret_line_col(), (0, 4));
        d.move_motion(Motion::LineDown, 1); // the empty last line
        d.clamp_caret_to_line();
        assert_eq!(d.caret_line_col(), (1, 0));
    }

    #[test]
    fn yank_line_and_paste_below() {
        let mut d = Document::new("foo\nbar\n");
        d.yank_lines(1); // caret on line 0 → yanks "foo\n"
        d.paste(true); // p → duplicate below
        assert_eq!(d.rope.to_string(), "foo\nfoo\nbar\n");
        assert_eq!(d.caret_line_col(), (1, 0)); // caret on pasted line
    }

    #[test]
    fn paste_charwise_after_caret() {
        let mut d = Document::new("abc");
        d.yank_motion(Motion::CharRight, 2); // yank "ab", caret back to 0
        d.paste(true); // p inserts after 'a'
        assert_eq!(d.rope.to_string(), "aabbc");
    }

    #[test]
    fn linewise_paste_at_eof_without_newline() {
        let mut d = Document::new("only");
        d.yank_lines(1); // normalized to "only\n"
        d.paste(true);
        assert_eq!(d.rope.to_string(), "only\nonly\n");
    }

    #[test]
    fn delete_then_paste_roundtrips() {
        let mut d = Document::new("foo bar");
        d.delete_motion(Motion::WordForward, 1); // "foo " into register
        assert_eq!(d.rope.to_string(), "bar");
        d.paste(false); // P puts it back before caret
        assert_eq!(d.rope.to_string(), "foo bar");
    }

    #[test]
    fn extend_motion_keeps_anchor() {
        let mut d = Document::new("abcdef");
        d.extend_motion(Motion::CharRight, 3);
        assert_eq!(d.selections[0].anchor, 0);
        assert_eq!(d.selections[0].head, 3);
    }

    #[test]
    fn visual_linewise_delete_removes_whole_lines() {
        // Anchor on line 0, head dragged to line 1 → `Vjd` deletes both.
        let mut d = Document::new("aaa\nbbb\nccc\n");
        d.extend_motion(Motion::LineDown, 1);
        d.delete_selection(true, false);
        assert_eq!(d.rope.to_string(), "ccc\n");
        assert_eq!(d.caret_line_col(), (0, 0));
    }

    #[test]
    fn visual_charwise_delete_is_inclusive() {
        // anchor 0, head 2 selects "abc" (char under head included).
        let mut d = Document::new("abcdef");
        d.extend_motion(Motion::CharRight, 2);
        d.delete_selection(false, false);
        assert_eq!(d.rope.to_string(), "def");
    }

    #[test]
    fn visual_yank_then_paste() {
        let mut d = Document::new("abcdef");
        d.extend_motion(Motion::CharRight, 2); // select "abc"
        d.yank_selection(false);
        assert_eq!(d.caret_line_col(), (0, 0)); // caret drops to selection start
        d.paste(true); // p after caret
        assert_eq!(d.rope.to_string(), "aabcbcdef");
    }

    #[test]
    fn toggle_task_flips_box_and_keeps_caret() {
        let mut d = Document::new("- [ ] a\nplain\n  1. [X] b");
        d.jump_to(6); // the 'a'
        assert!(d.toggle_task(0));
        assert_eq!(d.rope.line(0).to_string(), "- [x] a\n");
        assert_eq!(d.caret_offset(), 6);
        assert!(d.toggle_task(0));
        assert_eq!(d.rope.line(0).to_string(), "- [ ] a\n");
        assert!(!d.toggle_task(1)); // no box on a plain line
        assert!(d.toggle_task(2)); // indented ordered item, capital X unchecks
        assert_eq!(d.rope.line(2).to_string(), "  1. [ ] b");
    }

    #[test]
    fn blank_line_test_excludes_a_one_char_last_line() {
        // The buffer's last line carries no newline, so a one-char line there
        // is one char long *as stored* — the blankness test must look at the
        // char, not just the length, or `{`/`}`/`ip` treat it as a paragraph
        // break. "a\n\nb": only line 1 is blank.
        let d = Document::new("a\n\nb");
        assert!(!d.line_is_blank(0));
        assert!(d.line_is_blank(1));
        assert!(!d.line_is_blank(2));
        // A trailing newline makes a genuinely empty final line.
        let d = Document::new("a\n");
        assert!(d.line_is_blank(1));

        // The motion it backs: `}` from line 0 stops at the blank line, not
        // past it onto the one-char line.
        let d = Document::new("a\n\nb");
        assert_eq!(d.motion_target(Motion::ParaForward, 0, 1), d.rope.line_to_char(1));
    }

    #[test]
    fn single_line_edit_reports_only_confined_changes() {
        let mut d = Document::new("alpha\nbeta\ngamma");
        let rev = |d: &Document| d.revision();

        // Typing on one line: reported, against the revision that preceded it.
        let before = rev(&d);
        d.move_motion(Motion::LineDown, 1);
        d.insert("x");
        assert_eq!(d.single_line_edit(before), Some(1));
        // Only the immediately-preceding revision qualifies — this is what stops
        // a stale cache (or another buffer's) from being spliced into.
        assert_eq!(d.single_line_edit(before - 1), None);
        assert_eq!(d.single_line_edit(rev(&d)), None);

        // Anything that changes the line count must not be reported.
        let before = rev(&d);
        d.insert("\n");
        assert_eq!(d.single_line_edit(before), None);

        // Backspacing a newline joins two lines: also a line-count change.
        let mut d = Document::new("a\nb");
        d.move_motion(Motion::LineDown, 1);
        let before = rev(&d);
        d.delete_backward();
        assert_eq!(d.single_line_edit(before), None);

        // Backspacing an ordinary char stays on its line.
        let mut d = Document::new("ab\ncd");
        d.move_motion(Motion::LineDown, 1);
        d.move_motion(Motion::CharRight, 2);
        let before = rev(&d);
        d.delete_backward();
        assert_eq!(d.single_line_edit(before), Some(1));

        // Undo can move any number of lines, so it makes no claim.
        let mut d = Document::new("a\nb");
        d.checkpoint();
        d.insert("x");
        let before = rev(&d);
        d.undo();
        assert_eq!(d.single_line_edit(before), None);

        // Line-wise deletes and pastes likewise.
        let mut d = Document::new("a\nb\nc");
        let before = rev(&d);
        d.delete_lines(1);
        assert_eq!(d.single_line_edit(before), None);

        // In-place single-char rewrites report their line.
        let mut d = Document::new("abc\ndef");
        d.move_motion(Motion::LineDown, 1);
        let before = rev(&d);
        d.replace_char('Z', 1);
        assert_eq!(d.single_line_edit(before), Some(1));
        let before = rev(&d);
        d.toggle_case(1);
        assert_eq!(d.single_line_edit(before), Some(1));

        // A checkbox toggle reports the line it flipped, not the caret's.
        let mut d = Document::new("- [ ] a\n- [ ] b");
        let before = rev(&d);
        assert!(d.toggle_task(1));
        assert_eq!(d.single_line_edit(before), Some(1));
        assert_eq!(d.caret_line_col().0, 0); // caret never moved
    }

    #[test]
    fn undo_stack_is_capped() {
        let mut d = Document::new("x");
        for i in 0..UNDO_LEVELS + 50 {
            d.checkpoint();
            d.insert(&i.to_string());
        }
        assert_eq!(d.undo.len(), UNDO_LEVELS);
        // The cap drops the oldest states, so undo still walks back the most
        // recent ones rather than failing outright.
        let before = d.rope.to_string();
        d.undo();
        assert_ne!(d.rope.to_string(), before);
    }

    #[test]
    fn undo_redo_restores_text() {
        let mut d = Document::new("hello");
        d.checkpoint();
        d.delete_char_under(1);
        assert_eq!(d.rope.to_string(), "ello");
        d.undo();
        assert_eq!(d.rope.to_string(), "hello");
        d.redo();
        assert_eq!(d.rope.to_string(), "ello");
    }

    #[test]
    fn smart_newline_continues_and_clears_lists() {
        // Continue a bullet: caret at EOL of "- foo", Enter → new "- " below.
        let mut d = Document::new("- foo");
        d.move_motion(Motion::LineEnd, 1);
        d.insert_newline(true, 2);
        assert_eq!(d.rope.to_string(), "- foo\n- ");
        assert_eq!(d.caret_line_col(), (1, 2));

        // Enter on an empty item clears the marker (exits the list).
        d.insert_newline(true, 2);
        assert_eq!(d.rope.to_string(), "- foo\n");

        // `o`-style newline (clear_empty=false) keeps the empty marker instead.
        let mut d = Document::new("- ");
        d.move_motion(Motion::LineEnd, 1);
        d.insert_newline(false, 2);
        assert_eq!(d.rope.to_string(), "- \n- ");

        // A non-list line just splits.
        let mut d = Document::new("plain");
        d.move_motion(Motion::LineEnd, 1);
        d.insert_newline(true, 2);
        assert_eq!(d.rope.to_string(), "plain\n");
    }

    #[test]
    fn smart_newline_dedents_nested_empty_items() {
        // Enter on an empty nested item walks out one level per press, marker
        // kept; only the top-level press clears the marker.
        let mut d = Document::new("- a\n    - ");
        d.move_motion(Motion::FileEnd, 1);
        d.move_motion(Motion::LineEnd, 1);
        d.insert_newline(true, 2);
        assert_eq!(d.rope.to_string(), "- a\n  - ");
        assert_eq!(d.caret_line_col(), (1, 4));
        d.insert_newline(true, 2);
        assert_eq!(d.rope.to_string(), "- a\n- ");
        d.insert_newline(true, 2);
        assert_eq!(d.rope.to_string(), "- a\n");

        // Odd indent shallower than a step clears to top level, not below it.
        let mut d = Document::new(" - ");
        d.move_motion(Motion::LineEnd, 1);
        d.insert_newline(true, 2);
        assert_eq!(d.rope.to_string(), "- ");

        // A nested empty item under `o` (clear_empty=false) still repeats.
        let mut d = Document::new("  - ");
        d.move_motion(Motion::LineEnd, 1);
        d.insert_newline(false, 2);
        assert_eq!(d.rope.to_string(), "  - \n  - ");
    }

    #[test]
    fn smart_tab_shifts_list_lines() {
        // Tab anywhere on a bullet shifts the whole line; the caret rides along.
        let mut d = Document::new("- foo");
        d.move_motion(Motion::LineEnd, 1); // caret at col 5
        d.indent(2, false);
        assert_eq!(d.rope.to_string(), "  - foo");
        assert_eq!(d.caret_line_col(), (0, 7));

        // Shift-Tab outdents.
        d.indent(2, true);
        assert_eq!(d.rope.to_string(), "- foo");
        assert_eq!(d.caret_line_col(), (0, 5));

        // Off a list item, Tab inserts at the caret instead of shifting the line.
        let mut d = Document::new("foo");
        d.move_motion(Motion::CharRight, 1);
        d.indent(2, false);
        assert_eq!(d.rope.to_string(), "f  oo");
    }

    #[test]
    fn is_markdown_by_path() {
        let mut d = Document::new("x");
        assert!(d.is_markdown()); // pathless scratch buffer
        d.path = Some(PathBuf::from("/v/note.md"));
        assert!(d.is_markdown());
        d.path = Some(PathBuf::from("/v/schema.sql"));
        assert!(!d.is_markdown());
        d.path = Some(PathBuf::from("/v/Makefile"));
        assert!(!d.is_markdown());
    }

    #[test]
    fn non_markdown_skips_list_conveniences() {
        // Enter after a list-looking line in a .sql file: plain split, no "- ".
        let mut d = Document::new("- foo");
        d.path = Some(PathBuf::from("/v/schema.sql"));
        d.move_motion(Motion::LineEnd, 1);
        d.insert_newline(true, 2);
        assert_eq!(d.rope.to_string(), "- foo\n");

        // Tab on a list-looking line inserts at the caret, no line shift.
        let mut d = Document::new("- foo");
        d.path = Some(PathBuf::from("/v/schema.sql"));
        d.move_motion(Motion::CharRight, 1);
        d.indent(2, false);
        assert_eq!(d.rope.to_string(), "-   foo");
    }

    #[test]
    fn indent_selection_shifts_lines_uniformly() {
        // Mixed depths both shift by one width; the empty line is untouched.
        let mut d = Document::new("- a\n  - b\n\n- c");
        d.extend_motion(Motion::LineDown, 3);
        d.indent_selection(2, false);
        assert_eq!(d.rope.to_string(), "  - a\n    - b\n\n  - c");
        // Caret collapses to the first line's first non-blank.
        assert_eq!(d.caret_line_col(), (0, 2));

        // Dedent trims up to width per line; the shallowest line hits col 0.
        let mut d = Document::new("- a\n    - b");
        d.extend_motion(Motion::LineDown, 1);
        d.indent_selection(2, true);
        assert_eq!(d.rope.to_string(), "- a\n  - b");
        assert_eq!(d.caret_line_col(), (0, 0));
    }

    #[test]
    fn indent_lines_shifts_line_range() {
        // `>>`-style: an explicit line range, caret to l0's first non-blank.
        let mut d = Document::new("- a\n- b\n- c");
        d.indent_lines(0, 1, 2, false);
        assert_eq!(d.rope.to_string(), "  - a\n  - b\n- c");
        assert_eq!(d.caret_line_col(), (0, 2));

        // A range past EOF clamps instead of panicking (`5>>` near the end).
        d.indent_lines(2, 9, 2, true);
        assert_eq!(d.rope.to_string(), "  - a\n  - b\n- c");
    }

    #[test]
    fn indent_renumbers_ordered_lists() {
        // Tab on a middle item: it nests (restarting at 1) and the items
        // below close the gap. Shift-Tab restores the original numbering.
        let mut d = Document::new("1. a\n2. b\n3. c");
        d.move_motion(Motion::LineDown, 1);
        d.move_motion(Motion::LineEnd, 1);
        d.indent(2, false);
        assert_eq!(d.rope.to_string(), "1. a\n  1. b\n2. c");
        d.indent(2, true);
        assert_eq!(d.rope.to_string(), "1. a\n2. b\n3. c");
        assert_eq!(d.caret_line_col(), (1, 4));

        // The base run keeps its first item's number (9 stays); a digit-width
        // change (10 → 1) keeps the caret on its spot in the content.
        let mut d = Document::new("9. a\n10. b");
        d.move_motion(Motion::LineDown, 1);
        d.move_motion(Motion::LineEnd, 1);
        d.indent(2, false);
        assert_eq!(d.rope.to_string(), "9. a\n  1. b");
        assert_eq!(d.caret_line_col(), (1, 6));

        // `)` punctuation is kept, and a blank line ends the block: the
        // second list never renumbers.
        let mut d = Document::new("1) a\n2) b\n3) c\n\n7) x");
        d.move_motion(Motion::LineDown, 1);
        d.indent(2, false);
        assert_eq!(d.rope.to_string(), "1) a\n  1) b\n2) c\n\n7) x");

        // An unordered sibling breaks the run: `5. y` seeds a fresh run and
        // keeps its number instead of continuing from `2. b`.
        let mut d = Document::new("1. a\n2. b\n- x\n5. y\n6. z");
        d.move_motion(Motion::LineDown, 4);
        d.indent(2, false);
        assert_eq!(d.rope.to_string(), "1. a\n2. b\n- x\n5. y\n  1. z");

        // Visual dedent pulls nested items back into the outer run.
        let mut d = Document::new("1. a\n  1. b\n  2. c");
        d.move_motion(Motion::LineDown, 1);
        d.extend_motion(Motion::LineDown, 1);
        d.indent_selection(2, true);
        assert_eq!(d.rope.to_string(), "1. a\n2. b\n3. c");

        // Enter mid-list: the new item takes the next number and the items
        // below shift down.
        let mut d = Document::new("1. a\n2. b");
        d.move_motion(Motion::LineEnd, 1);
        d.insert_newline(false, 2);
        assert_eq!(d.rope.to_string(), "1. a\n2. \n3. b");
        assert_eq!(d.caret_line_col(), (1, 3));

        // `dd` mid-list (the editor renumbers at the caret after every delete
        // action): the items below close the gap.
        let mut d = Document::new("1. a\n2. b\n3. c");
        d.move_motion(Motion::LineDown, 1);
        d.delete_lines(1);
        let (line, _) = d.caret_line_col();
        d.renumber_block(line, 2);
        assert_eq!(d.rope.to_string(), "1. a\n2. c");
    }

    #[test]
    fn save_load_roundtrip() {
        let mut path = std::env::temp_dir();
        path.push("darknotes_roundtrip_test.md");
        let _ = std::fs::remove_file(&path);

        let mut d = Document::open(&path).unwrap(); // missing file → empty
        assert!(!d.is_dirty());
        d.insert("hello");
        assert!(d.is_dirty());
        d.save().unwrap();
        assert!(!d.is_dirty());

        let reopened = Document::open(&path).unwrap();
        assert_eq!(reopened.rope.to_string(), "hello");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn disk_hash_tells_own_save_from_external_write() {
        let mut path = std::env::temp_dir();
        path.push("darknotes_disk_hash_test.md");

        let mut d = Document::open(&path).unwrap();
        d.insert("hello");
        d.save().unwrap();
        // Watcher event echoing our own save: disk matches disk_hash.
        let disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(Document::hash_text(&disk), d.disk_hash());

        // Genuine external write: disk no longer matches.
        std::fs::write(&path, "changed elsewhere").unwrap();
        let disk = std::fs::read_to_string(&path).unwrap();
        assert_ne!(Document::hash_text(&disk), d.disk_hash());

        let _ = std::fs::remove_file(&path);
    }
}
