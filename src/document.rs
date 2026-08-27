use ropey::Rope;
use std::collections::HashMap;
use std::io::{self, Write};
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
    /// genuine external change from an echo of our own write, and `save`
    /// refuses to overwrite a file that no longer matches it. Only a real read
    /// or write may move it — recording an unadopted disk state here would
    /// erase the very divergence both checks exist to find.
    disk_hash: u64,
    /// Hash of an external change already reported for this buffer, so repeat
    /// watcher events for one write don't re-warn. Distinct from `disk_hash`
    /// because this content was seen and *not* adopted.
    warned_hash: Option<u64>,
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
    /// The last content change, when it was confined to one line — see
    /// `LineEdit`. `None` means "assume everything changed": multi-line edits,
    /// anything crossing a newline, undo/redo.
    last_edit: Option<LineEdit>,
    /// `m{a}`–`m{z}`: char offsets, per buffer as in vim (`ma` in two files is
    /// two marks). Uppercase marks are global and live on the editor. Offsets
    /// are raw — an edit above a mark leaves it pointing at a shifted spot, and
    /// a jump to it clamps. Vim adjusts; matching that means routing every
    /// edit's `(offset, delta)` through here.
    marks: HashMap<char, usize>,
    /// Headings the user has folded closed, in document order — see
    /// `FoldAnchor`.
    folded: Vec<FoldAnchor>,
    /// `folded` resolved against the markdown parse: inclusive
    /// `(header, last_hidden)` line ranges, sorted by start and
    /// non-overlapping. Rebuilt by `sync_folds` — which needs a parse, so it
    /// runs once per render rather than per edit. An edit that shifts lines
    /// therefore leaves these stale until the frame lands, the same granularity
    /// the markdown spans themselves are refreshed at.
    folds: Vec<(usize, usize)>,
    /// Bumped by every fold toggle. Pairs with `revision` as `sync_folds`'
    /// staleness check, and is what the render layer's row key watches so a
    /// fold change that moved neither caret nor content still rebuilds.
    folds_gen: u64,
    /// `(revision, folds_gen)` the current `folds` was derived from.
    folds_key: (u64, u64),
}

/// A closed fold, anchored by title as well as line: an edit elsewhere shifts
/// the line, and a title re-finds the heading where a stale index would hide
/// the wrong section. Fold state is view state — deliberately absent from
/// `Snapshot`, so undo can't restore folds from another shape of the document.
#[derive(Clone, Debug, PartialEq, Eq)]
struct FoldAnchor {
    line: usize,
    title: String,
}

/// A content change confined to a single line, leaving the line count alone —
/// what lets a consumer update a derivation incrementally instead of recomputing
/// it over the whole document.
///
/// Both revisions are recorded so the consumer can prove its cached state is
/// *this* buffer's state immediately before the change: a buffer switch, or two
/// edits coalescing into one render, breaks the pairing and forces a recompute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineEdit {
    /// Revision before the change. A cache built at this revision is reusable.
    pub before: u64,
    /// Revision after — must still be the document's current one.
    pub after: u64,
    /// The line whose text changed.
    pub line: usize,
    /// Net change in the document's char count. Every offset past the edited
    /// line shifts by exactly this, which is what makes a stored offset table
    /// (the search-match list) fixable without rescanning.
    pub delta: isize,
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
            warned_hash: None,
            revision: next_revision(),
            register: Register::default(),
            undo: Vec::new(),
            redo: Vec::new(),
            goal_col: 0,
            last_edit: None,
            marks: HashMap::new(),
            folded: Vec::new(),
            folds: Vec::new(),
            folds_gen: 0,
            folds_key: (0, 0),
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
            warned_hash: None,
            revision: next_revision(),
            register: Register::default(),
            undo: Vec::new(),
            redo: Vec::new(),
            goal_col: 0,
            last_edit: None,
            marks: HashMap::new(),
            folded: Vec::new(),
            folds: Vec::new(),
            folds_gen: 0,
            folds_key: (0, 0),
        })
    }

    /// Write the buffer to its backing file. No-op for an unnamed buffer.
    /// Refuses when the file no longer holds what this buffer last read or
    /// wrote — someone else's edit is not ours to discard — unless `force`
    /// (`:w!`). `:e!` takes the other side of that choice.
    pub fn save(&mut self, force: bool) -> io::Result<()> {
        if !force {
            self.check_unchanged()?;
        }
        self.write_now()
    }

    /// Adopt `path` as the backing file and write to it (vim `:w <name>`).
    /// Refuses when `path` already holds a file, unless `force` (`:w! <name>`):
    /// its contents were never loaded here, so nothing has been compared and
    /// overwriting would be blind.
    pub fn save_as(&mut self, path: impl Into<PathBuf>, force: bool) -> io::Result<()> {
        let path = path.into();
        // Naming the file it already has is a plain `:w`, divergence check and
        // all — `disk_hash` describes exactly this file.
        if self.path.as_deref() == Some(path.as_path()) {
            return self.save(force);
        }
        if !force && path.exists() {
            return Err(refused(format!("E13: {} exists (add ! to override)", path.display())));
        }
        // Past here `disk_hash` describes the *old* file, so it says nothing
        // about this target and `write_now` runs unguarded.
        self.path = Some(path);
        self.write_now()
    }

    /// `Err` when the backing file has diverged from what this buffer last read
    /// or wrote.
    fn check_unchanged(&self) -> io::Result<()> {
        let Some(path) = &self.path else { return Ok(()) };
        // A file that is gone is not a conflict — `save` recreates it, as vim
        // does. Neither is one that won't read back as text: there is then
        // nothing to compare and nothing to describe to the user, and the
        // write reports its own failure if the path is genuinely unwritable.
        let Ok(disk) = std::fs::read_to_string(path) else { return Ok(()) };
        if Self::hash_text(&disk) == self.disk_hash {
            return Ok(());
        }
        Err(refused("E13: file changed on disk (add ! to override)".into()))
    }

    /// The write itself, with no guards: create the parent, replace the file,
    /// and adopt the written text as this buffer's disk state.
    fn write_now(&mut self) -> io::Result<()> {
        if let Some(path) = &self.path {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let text = self.rope.to_string();
            atomic_write(path, &text)?;
            self.dirty = false;
            self.missing = false;
            self.disk_hash = Self::hash_text(&text);
            self.warned_hash = None; // whatever diverged, this write settled it
        }
        Ok(())
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

    /// Whether `hash` is an external change already reported for this buffer.
    pub fn already_warned(&self, hash: u64) -> bool {
        self.warned_hash == Some(hash)
    }

    /// Record a disk state observed but not adopted (the W12 dirty-buffer
    /// case), so duplicate events for the same change don't re-warn.
    pub fn set_warned_hash(&mut self, hash: u64) {
        self.warned_hash = Some(hash);
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

    /// `touch` for a change confined to `line` that left the line count alone,
    /// shifting the document's char count by `delta` — see `LineEdit`. Callers
    /// must be certain of both: the render layer reuses every *other* line's
    /// markdown parse, and the search layer shifts its match offsets, on this
    /// promise.
    fn touch_line(&mut self, line: usize, delta: isize) {
        let before = self.revision;
        self.dirty = true;
        self.revision = next_revision();
        self.last_edit = Some(LineEdit { before, after: self.revision, line, delta });
    }

    /// The last change, if it touched only one line *and* the caller's cached
    /// derivation (`cached_revision`) is this buffer's state immediately before
    /// it. Both halves matter — see `LineEdit`.
    pub fn single_line_edit(&self, cached_revision: u64) -> Option<LineEdit> {
        self.last_edit
            .filter(|e| e.before == cached_revision && e.after == self.revision)
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

    /// Whether markdown-aware editing conveniences (prefix continuation,
    /// list-line Tab, span styling) apply: true for `.md` files and pathless scratch
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

    // --- Folding ---------------------------------------------------------
    //
    // Closed folds live here rather than in the editor because vertical
    // motions and linewise operators treat one as a single line, and both
    // resolve against the document. They are view state: no `revision` bump,
    // no place in a `Snapshot`, and the ranges are re-derived from the
    // markdown parse instead of stored, so an edit that shifts lines can't
    // strand one over the wrong text.

    /// Generation of the closed-fold set — see the `folds_gen` field.
    pub fn folds_gen(&self) -> u64 {
        self.folds_gen
    }

    /// Every closed fold as an inclusive `(header, last_hidden)` line range,
    /// sorted by start and non-overlapping.
    pub fn folds(&self) -> &[(usize, usize)] {
        &self.folds
    }

    /// Resolve `folded` against `spans` into line ranges, repairing anchors
    /// whose heading has moved. Called once per row build: a document with no
    /// folds returns on the first check, and a resolved set is reused until the
    /// content or the fold set changes.
    pub fn sync_folds(&mut self, spans: &markdown::Parsed) {
        if self.folded.is_empty() {
            if !self.folds.is_empty() {
                self.folds.clear();
                self.folds_gen += 1;
            }
            return;
        }
        if self.folds_key == (self.revision, self.folds_gen) {
            return;
        }

        // Repair. An anchor still sitting on its heading stays; one whose line
        // moved re-finds it by title; one whose heading is gone is dropped.
        let rope = self.rope.clone(); // ropey clone is cheap (shared, CoW)
        self.folded.retain_mut(|a| {
            if heading_title(spans, &rope, a.line).as_deref() == Some(a.title.as_str()) {
                return true;
            }
            match find_heading(spans, &rope, &a.title) {
                Some(line) => {
                    a.line = line;
                    true
                }
                None => false,
            }
        });
        self.folded.sort_unstable_by_key(|a| a.line);
        // Two anchors can repair onto one line — duplicate titles resolve to
        // the first, the rule `[[note#Heading]]` already follows.
        self.folded.dedup_by_key(|a| a.line);

        // Extents last, skipping any heading an already-closed fold swallows:
        // overlapping ranges would break the sorted-and-disjoint promise
        // `fold_at`'s search depends on.
        let previous = std::mem::take(&mut self.folds);
        for i in 0..self.folded.len() {
            let start = self.folded[i].line;
            if self.folds.last().is_some_and(|&(_, end)| start <= end) {
                continue;
            }
            if let Some(range) = self.fold_extent(spans, start) {
                self.folds.push(range);
            }
        }
        // An edit can move, grow or dissolve a fold with no toggle involved —
        // deleting the `#` from a header, or adding a heading that ends a
        // section early. The render layer watches the generation to know its
        // row geometry is stale, so a repair has to bump it like a toggle does.
        if self.folds != previous {
            self.folds_gen += 1;
        }
        self.folds_key = (self.revision, self.folds_gen);
    }

    /// The lines a fold on heading `start` hides: `(start, last_hidden)`,
    /// inclusive. The section runs to the line before the next heading of the
    /// same or shallower level — subheadings are swallowed — minus trailing
    /// blank lines, so the blank separating two sections stays on screen.
    /// `None` when nothing is left to hide, which is what makes a heading with
    /// an empty section unfoldable.
    fn fold_extent(&self, spans: &markdown::Parsed, start: usize) -> Option<(usize, usize)> {
        let level = heading_level_at(spans, start)?;
        let mut end = start;
        for line in start + 1..self.rope.len_lines() {
            if heading_level_at(spans, line).is_some_and(|l| l <= level) {
                break;
            }
            end = line;
        }
        while end > start && self.line_is_blank(end) {
            end -= 1;
        }
        (end > start).then_some((start, end))
    }

    /// The closed fold containing `line`, if any.
    pub fn fold_at(&self, line: usize) -> Option<(usize, usize)> {
        // Sorted and disjoint, so the last fold starting at or before `line` is
        // the only candidate.
        let i = self.folds.partition_point(|&(s, _)| s <= line).checked_sub(1)?;
        let (start, end) = self.folds[i];
        (line <= end).then_some((start, end))
    }

    /// `line`'s fold header, or `line` itself when no closed fold covers it.
    pub fn fold_start(&self, line: usize) -> usize {
        self.fold_at(line).map_or(line, |(start, _)| start)
    }

    /// The last line hidden by `line`'s closed fold, or `line` itself.
    pub fn fold_end(&self, line: usize) -> usize {
        self.fold_at(line).map_or(line, |(_, end)| end)
    }

    /// Expand a linewise range to cover any closed folds it touches — what
    /// makes `dd` on a closed fold take the whole section.
    fn fold_expand(&self, first: usize, last: usize) -> (usize, usize) {
        (self.fold_start(first), self.fold_end(last))
    }

    /// `count` *display* lines below `line`, clamped to the last: a closed fold
    /// is one step. Shared by vertical motion and the counted linewise
    /// operators, so `3j` and `3dd` cover the same ground.
    fn line_below(&self, line: usize, count: usize) -> usize {
        let last = self.rope.len_lines().saturating_sub(1);
        let mut line = self.fold_start(line);
        for _ in 0..count {
            let below = self.fold_end(line) + 1;
            if below > last {
                break;
            }
            line = below;
        }
        line
    }

    /// `count` display lines above `line`, clamped to the first.
    fn line_above(&self, line: usize, count: usize) -> usize {
        let mut line = self.fold_start(line);
        for _ in 0..count {
            let Some(above) = line.checked_sub(1) else { break };
            line = self.fold_start(above);
        }
        line
    }

    /// The heading whose section contains `line`: `line` itself when it is one,
    /// else the nearest heading above. `None` above the first heading.
    fn enclosing_heading(&self, spans: &markdown::Parsed, line: usize) -> Option<usize> {
        (0..=line.min(self.rope.len_lines().saturating_sub(1)))
            .rev()
            .find(|&l| heading_level_at(spans, l).is_some())
    }

    /// Mark heading `head` closed, if it is a heading with something to hide
    /// and isn't closed already. Reports whether the set changed.
    fn close_heading(&mut self, spans: &markdown::Parsed, head: usize) -> bool {
        if self.folded.iter().any(|a| a.line == head) || self.fold_extent(spans, head).is_none() {
            return false;
        }
        let Some(title) = heading_title(spans, &self.rope, head) else { return false };
        self.folded.push(FoldAnchor { line: head, title });
        true
    }

    /// Adopt a changed fold set: bump the generation (which is what makes the
    /// render layer rebuild) and re-resolve the ranges immediately, so
    /// `fold_at` and the motions are correct within this same keystroke rather
    /// than one render behind.
    fn folds_changed(&mut self, spans: &markdown::Parsed) {
        self.folds_gen += 1;
        self.sync_folds(spans);
    }

    /// `zc`: close the section under `line`'s heading — or the heading above
    /// it, so it works from anywhere inside a section, as vim's does.
    pub fn close_fold(&mut self, spans: &markdown::Parsed, line: usize) {
        let Some(head) = self.enclosing_heading(spans, line) else { return };
        if self.close_heading(spans, head) {
            self.folds_changed(spans);
        }
    }

    /// `zo`: open the fold `line` sits in. Silent when none is closed.
    pub fn open_fold(&mut self, spans: &markdown::Parsed, line: usize) {
        let Some((start, _)) = self.fold_at(line) else { return };
        self.folded.retain(|a| a.line != start);
        self.folds_changed(spans);
    }

    /// `za`.
    pub fn toggle_fold(&mut self, spans: &markdown::Parsed, line: usize) {
        match self.fold_at(line) {
            Some(_) => self.open_fold(spans, line),
            None => self.close_fold(spans, line),
        }
    }

    /// `zR`.
    pub fn open_all_folds(&mut self, spans: &markdown::Parsed) {
        if self.folded.is_empty() {
            return;
        }
        self.folded.clear();
        self.folds_changed(spans);
    }

    /// `zM`: close every foldable heading. Nested sections close too; the
    /// outermost fold is what ends up hiding them, and reopening it exposes
    /// the inner ones still closed — vim's behavior.
    pub fn close_all_folds(&mut self, spans: &markdown::Parsed) {
        let mut changed = false;
        for line in 0..self.rope.len_lines() {
            if heading_level_at(spans, line).is_some() {
                changed |= self.close_heading(spans, line);
            }
        }
        if changed {
            self.folds_changed(spans);
        }
    }

    /// Titles of the closed folds, for the session snapshot.
    pub fn fold_titles(&self) -> Vec<String> {
        self.folded.iter().map(|a| a.title.clone()).collect()
    }

    /// Reinstate folds saved by a previous session. Their lines are unknowable
    /// until the markdown parse exists, so each anchor is seeded past the end of
    /// any document and resolved by title on the first `sync_folds` —
    /// `usize::MAX` can never match a real heading, so repair always runs.
    pub fn set_fold_titles(&mut self, titles: Vec<String>) {
        if titles.is_empty() {
            return;
        }
        self.folded =
            titles.into_iter().map(|title| FoldAnchor { line: usize::MAX, title }).collect();
        self.folds_gen += 1;
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
        let added = text.chars().count();
        self.rope.insert(at, text);
        match line {
            Some(line) => self.touch_line(line, added as isize),
            None => self.touch(),
        }
        self.set_caret(at + added);
    }

    /// Smart newline: carry the caret line's markdown prefix — a list marker or
    /// a blockquote `>` — onto the new line, else a plain newline. The prefix
    /// lands at the caret, so text after it follows the new marker (splitting an
    /// item mid-line works). `clear_empty` (Enter, not `o`) steps an empty item
    /// out instead of repeating its prefix: an indented item dedents by `width`
    /// (one level per press, marker kept), an unindented one erases the prefix,
    /// leaving the empty line.
    pub fn insert_newline(&mut self, clear_empty: bool, width: usize) {
        if !self.is_markdown() {
            self.insert("\n");
            return;
        }
        let (line, _) = self.line_col_of(self.caret());
        let text: String = self.rope.line(line).chars().filter(|&c| c != '\n').collect();
        match markdown::line_continuation(&text) {
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

    /// Enter at the end of an unclosed opening fence: drop `closer` two lines
    /// down and leave the caret on the empty line between the two, so the fence
    /// is typed once. Whether to is the caller's call — the parse answers it
    /// (`markdown::fence_to_close`), and this layer has no spans.
    pub fn insert_fence_close(&mut self, closer: &str) {
        let at = self.caret();
        self.rope.insert(at, &format!("\n\n{closer}"));
        self.touch(); // crosses newlines: whole-document reparse
        self.set_caret(at + 1);
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
        self.touch_line(line, 0); // one char replaces one char
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
            Some(line) => self.touch_line(line, -1),
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
                Some(line) => self.touch_line(line, -1),
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
            let (l0, l1) = self
                .fold_expand(self.rope.char_to_line(r.start), self.rope.char_to_line(r.end));
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

    /// Char span for a linewise delete of lines `first..=last`: the range that
    /// goes to the register, and the offset the removal actually starts at.
    ///
    /// Through the buffer's last line the two differ. There is no newline after
    /// a final line, so removing the range alone leaves the *preceding* one
    /// behind as a phantom empty last line — and on an already-empty last line
    /// it would remove nothing at all. Taking that newline instead is what
    /// leaves the buffer genuinely one line shorter.
    ///
    /// Keyed on line indices, not on whether the range reaches `len_chars`: a
    /// buffer ending in a newline has a final empty line, so a range stopping
    /// at `len_chars` need not be the last line's, and treating it as such
    /// would swallow the trailing newline.
    fn linewise_del_span(&self, first: usize, last: usize) -> (std::ops::Range<usize>, usize) {
        // A closed fold is one line to a linewise operator, so a range touching
        // one covers all of it.
        let (first, last) = self.fold_expand(first, last);
        let start = self.rope.line_to_char(first);
        if last + 1 >= self.rope.len_lines() {
            (start..self.rope.len_chars(), start.saturating_sub(1))
        } else {
            (start..self.rope.line_to_char(last + 1), start)
        }
    }

    /// Visual `d`/`x`, or `c` when `change`: delete the selection into the
    /// register, then drop the caret on a real char of the resulting line. A
    /// linewise change spares the span's last newline, leaving one empty line
    /// for the insert that follows (vim `Vc`, same rule as `cip`), and leaves
    /// the caret there rather than snapping it onto a char.
    pub fn delete_selection(&mut self, linewise: bool, change: bool) {
        let (start, mut end, del_start) = if linewise {
            let r = self.selections[0].range();
            let (span, del_start) = self
                .linewise_del_span(self.rope.char_to_line(r.start), self.rope.char_to_line(r.end));
            // A change keeps the newline before the range and gives up the
            // range's own last one instead, so the insert lands on an empty
            // line rather than joining the neighbours.
            (span.start, span.end, if change { span.start } else { del_start })
        } else {
            let (start, end) = self.selection_span(false);
            (start, end, start)
        };
        if start < end {
            self.set_register(self.rope.slice(start..end).to_string(), linewise);
            if change && linewise && self.rope.char(end - 1) == '\n' {
                end -= 1;
            }
            self.rope.remove(del_start..end);
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
        let (l0, l1) = self.fold_expand(l0, l1);
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
        let last = self.line_below(line, count.max(1) - 1);
        let (span, del_start) = self.linewise_del_span(line, last);
        let start = span.start;
        if del_start < span.end {
            self.set_register(self.rope.slice(span.clone()).to_string(), true);
            self.rope.remove(del_start..span.end);
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
        // "No line that way" is measured in display lines: `dj` on the last
        // line of a document ending in a closed fold is still a failed motion.
        if (up && self.fold_start(line) == 0) || (!up && self.fold_end(line) == last) {
            return;
        }
        let (first, last_del) = if up {
            (self.line_above(line, count), line)
        } else {
            (line, self.line_below(line, count))
        };
        let (span, del_start) = self.linewise_del_span(first, last_del);
        let start = span.start;
        if del_start < span.end {
            self.set_register(self.rope.slice(span.clone()).to_string(), true);
            self.rope.remove(del_start..span.end);
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
            self.touch_line(line, -((to - from) as isize));
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
        self.touch_line(line, 0); // `count` chars replace `count` chars
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
        self.touch_line(line, 0); // case mapping keeps one char per char
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
        let (line, last) = self.fold_expand(line, self.line_below(line, count.max(1) - 1));
        let start = self.rope.line_to_char(line);
        let end_line = (last + 1).min(self.rope.len_lines());
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
            // Vertical motion counts *display* lines: a closed fold is one
            // step, which is what makes the relative line numbers beside it
            // actionable (`5j` lands where the gutter says 5). `fold_start` on
            // the landing line catches a caret that began inside a fold.
            Motion::LineUp => {
                let (line, _) = self.line_col_of(from);
                self.offset_in_line(self.line_above(line, count), self.goal_col)
            }
            Motion::LineDown => {
                let (line, _) = self.line_col_of(from);
                self.offset_in_line(self.line_below(line, count), self.goal_col)
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

/// A save the editor declined to perform, as opposed to one the filesystem
/// rejected. `AlreadyExists` is the marker: the message is already a complete
/// vim-style error, so `Editor::save` shows it without a `save failed:` prefix.
fn refused(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::AlreadyExists, message)
}

/// Write `contents` to `path` without ever leaving it partial: fill a temp file
/// beside it, flush that to disk, then rename over the target. An interruption —
/// a full disk, the OOM killer, power loss — costs the *new* contents and leaves
/// the previous version intact. Writing in place would instead truncate first,
/// so the same interruption leaves a zero-length or half-written note whose only
/// complete copy was in a process that no longer exists.
///
/// The temp lives in the target's own directory, since `rename` is only atomic
/// within a filesystem. Its name is dot-prefixed so `Vault::scan` skips it and a
/// crashed save can't surface as a note, and pid-tagged so two instances saving
/// the same note don't share one scratch file.
// ponytail: no directory fsync, so a crash right after `rename` can still lose
// the save — but what survives is the intact previous version, which is the
// property worth paying for. Add one if losing a *confirmed* save ever matters.
pub(crate) fn atomic_write(path: &Path, contents: &str) -> io::Result<()> {
    // A symlinked note is written through to its target. Renaming onto the link
    // path would replace the link itself with a regular file, silently cutting
    // whatever the user pointed at it.
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let dir = target.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, format!("{} has no parent", target.display()))
    })?;
    let name = target.file_name().unwrap_or_default().to_string_lossy();
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));

    let replace = || -> io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(contents.as_bytes())?;
        // `rename` is atomic but orders metadata only: without this the blocks
        // can still be unwritten when the crash lands, which is the corruption
        // this whole function exists to avoid.
        f.sync_all()?;
        // Closed before the rename: Windows refuses to rename an open file.
        drop(f);
        // Keep the target's mode. A note the user chmod'd to 0600 must not
        // widen to a fresh temp file's 0644.
        if let Ok(meta) = std::fs::metadata(&target) {
            std::fs::set_permissions(&tmp, meta.permissions())?;
        }
        std::fs::rename(&tmp, &target)
    };
    replace().inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp); // no scratch files left in the vault
    })
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

/// A heading line's level, read from the markdown parse. The scanner pushes a
/// line's whole-line role span first, so the leading span decides. Reading the
/// parse rather than scanning for `#` is what keeps a comment inside a fence,
/// or a `#` in frontmatter, from looking like a heading.
fn heading_level_at(spans: &markdown::Parsed, line: usize) -> Option<u8> {
    match spans.get(line)?.first()?.kind {
        markdown::SpanKind::Heading(level) => Some(level),
        _ => None,
    }
}

/// The title of the heading on `line`, markers stripped, or `None` when the
/// line isn't a heading.
fn heading_title(spans: &markdown::Parsed, rope: &Rope, line: usize) -> Option<String> {
    heading_level_at(spans, line)?;
    markdown::heading_text(&markdown::line_text(rope, line)).map(str::to_string)
}

/// Line of the first heading titled `title`, case-insensitively — a fold
/// anchor's repair path. Duplicate titles resolve to the first, as they do for
/// a `[[note#Heading]]` link.
// ponytail: ASCII case folding and a whole-document walk, like that link
// resolution. Only runs for an anchor whose line no longer holds its heading.
fn find_heading(spans: &markdown::Parsed, rope: &Rope, title: &str) -> Option<usize> {
    (0..rope.len_lines())
        .find(|&l| heading_title(spans, rope, l).is_some_and(|t| t.eq_ignore_ascii_case(title)))
}

#[cfg(test)]
mod folds {
    use super::*;

    /// A plan-shaped note: two H2 phases with an H3 inside the first, a fenced
    /// block whose `#` comment must not read as a heading, and one blank line
    /// separating each section from the next.
    const PLAN: &str = "\
# Plan

intro

## Phase 1

body one

### Detail

detail body

```sh
# not a heading
```

## Phase 2

body two
";

    fn doc(text: &str) -> Document {
        let mut d = Document::new(text);
        let spans = markdown::parse(&d.rope);
        d.sync_folds(&spans);
        d
    }

    fn spans_of(d: &Document) -> markdown::Parsed {
        markdown::parse(&d.rope)
    }

    fn line_of(d: &Document, needle: &str) -> usize {
        (0..d.rope.len_lines())
            .find(|&l| markdown::line_text(&d.rope, l).contains(needle))
            .unwrap_or_else(|| panic!("no line matching {needle:?}"))
    }

    #[test]
    fn extent_swallows_subheadings_and_leaves_the_separating_blank() {
        let d = doc(PLAN);
        let spans = spans_of(&d);
        let (p1, p2) = (line_of(&d, "## Phase 1"), line_of(&d, "## Phase 2"));

        // Phase 1 runs to its last non-blank line: the `### Detail` subsection
        // and the fence are inside it, the blank before `## Phase 2` is not.
        let (start, end) = d.fold_extent(&spans, p1).unwrap();
        assert_eq!(start, p1);
        assert_eq!(end, p2 - 2);
        assert!(d.line_is_blank(p2 - 1), "the separating blank stays visible");

        // A subheading folds only its own section, stopping at the next H2.
        let detail = line_of(&d, "### Detail");
        assert_eq!(d.fold_extent(&spans, detail).unwrap(), (detail, p2 - 2));

        // The H1 swallows everything below it; the last section reaches the end.
        assert_eq!(d.fold_extent(&spans, 0).unwrap().1, line_of(&d, "body two"));
        assert_eq!(d.fold_extent(&spans, p2).unwrap(), (p2, p2 + 2));

        // A `#` inside a fence is code, not a heading — the parse says so, which
        // is why fold extents read it instead of scanning for `#`.
        assert!(d.fold_extent(&spans, line_of(&d, "not a heading")).is_none());
        // Nor is a non-heading line, or a heading with nothing to hide.
        assert!(d.fold_extent(&spans, line_of(&d, "intro")).is_none());
        let empty = doc("## Empty\n\n## Next\n\nbody\n");
        assert!(empty.fold_extent(&spans_of(&empty), 0).is_none());
    }

    #[test]
    fn frontmatter_hashes_are_not_headings() {
        let d = doc("---\ntitle: # not a heading\n---\n\n## Real\n\nbody\n");
        let spans = spans_of(&d);
        assert!(d.fold_extent(&spans, 1).is_none());
        assert_eq!(d.fold_extent(&spans, 4).unwrap(), (4, 6));
    }

    #[test]
    fn closing_and_opening_track_the_caret_line() {
        let mut d = doc(PLAN);
        let spans = spans_of(&d);
        let p1 = line_of(&d, "## Phase 1");

        // `za` from *inside* a section closes the section it belongs to.
        d.toggle_fold(&spans, p1 + 2);
        assert_eq!(d.folds(), &[(p1, line_of(&d, "## Phase 2") - 2)]);
        assert_eq!(d.fold_titles(), vec!["Phase 1".to_string()]);
        // The ranges are live immediately, not one render behind.
        assert_eq!(d.fold_start(p1 + 2), p1);
        assert_eq!(d.fold_end(p1), line_of(&d, "## Phase 2") - 2);

        // …and toggling again from the header opens it.
        d.toggle_fold(&spans, p1);
        assert!(d.folds().is_empty());

        // `zM` closes every foldable heading; a nested one is swallowed by its
        // ancestor's range, so the ranges stay disjoint.
        d.close_all_folds(&spans);
        assert_eq!(d.folds(), &[(0, line_of(&d, "body two"))]);
        assert!(d.fold_titles().len() > 1, "the inner headings are closed too");
        // Opening the outer one exposes the inner folds, still closed.
        d.open_fold(&spans, 0);
        assert_eq!(d.folds().first(), Some(&(p1, line_of(&d, "## Phase 2") - 2)));

        d.open_all_folds(&spans);
        assert!(d.folds().is_empty() && d.fold_titles().is_empty());
    }

    #[test]
    fn anchors_repair_against_a_moved_or_deleted_heading() {
        let mut d = doc(PLAN);
        let spans = spans_of(&d);
        let p2 = line_of(&d, "## Phase 2");
        d.close_fold(&spans, p2);
        assert_eq!(d.folds(), &[(p2, p2 + 2)]);

        // An edit above shifts the heading: the anchor re-finds it by title.
        d.jump_to(0);
        d.insert("added\n");
        let spans = spans_of(&d);
        d.sync_folds(&spans);
        assert_eq!(d.folds(), &[(p2 + 1, p2 + 3)]);

        // A session restore knows only titles; the first sync resolves them.
        let mut d = doc(PLAN);
        d.set_fold_titles(vec!["Phase 2".into(), "Gone".into()]);
        d.sync_folds(&spans_of(&d));
        assert_eq!(d.folds(), &[(p2, p2 + 2)], "the missing heading is dropped");
        assert_eq!(d.fold_titles(), vec!["Phase 2".to_string()]);

        // Two anchors resolving onto one heading collapse to a single fold.
        let mut d = doc(PLAN);
        d.set_fold_titles(vec!["Phase 2".into(), "phase 2".into()]);
        d.sync_folds(&spans_of(&d));
        assert_eq!(d.folds(), &[(p2, p2 + 2)]);
    }

    #[test]
    fn vertical_motion_and_linewise_ops_count_a_fold_as_one_line() {
        let mut d = doc(PLAN);
        let spans = spans_of(&d);
        let (p1, p2) = (line_of(&d, "## Phase 1"), line_of(&d, "## Phase 2"));
        d.close_fold(&spans, p1);

        // `j` from the line above the fold lands on its header, and one more
        // clears the whole section — the blank line before Phase 2.
        d.jump_to(d.rope.line_to_char(p1 - 1));
        d.move_motion(Motion::LineDown, 1);
        assert_eq!(d.caret_line_col().0, p1);
        d.move_motion(Motion::LineDown, 1);
        assert_eq!(d.caret_line_col().0, p2 - 1);
        // `k` back over it, and a count crossing it in one go.
        d.move_motion(Motion::LineUp, 2);
        assert_eq!(d.caret_line_col().0, p1 - 1);
        d.move_motion(Motion::LineDown, 3);
        assert_eq!(d.caret_line_col().0, p2);

        // `yy` on the closed fold takes the whole section, not the header line.
        d.jump_to(d.rope.line_to_char(p1));
        d.yank_lines(1);
        let yanked = d.register_text().to_string();
        assert!(yanked.starts_with("## Phase 1"), "{yanked:?}");
        assert!(yanked.contains("detail body"), "{yanked:?}");
        assert!(!yanked.contains("Phase 2"), "{yanked:?}");

        // …and so does `dd`, leaving the blank line and Phase 2 behind.
        d.delete_lines(1);
        let left = d.rope.to_string();
        assert!(!left.contains("Phase 1") && !left.contains("detail body"), "{left:?}");
        assert!(left.contains("## Phase 2"));
        assert_eq!(d.caret_line_col().0, p1);
    }

    #[test]
    fn dj_on_a_trailing_fold_is_a_failed_motion() {
        // No trailing newline, so the fold really does reach the last line.
        let mut d = doc("body\n\n## Last\n\ntail");
        let spans = spans_of(&d);
        d.close_fold(&spans, 2);
        assert_eq!(d.folds(), &[(2, 4)]);
        d.jump_to(d.rope.line_to_char(2));
        let before = d.rope.to_string();
        d.delete_lines_dir(1, false); // `dj` with no display line below
        assert_eq!(d.rope.to_string(), before);
        // `dk` from there still works, and takes the whole fold with it. The
        // span reaches the end of the buffer, so it swallows the newline
        // *before* it rather than leaving a trailing blank line.
        d.delete_lines_dir(1, true);
        assert_eq!(d.rope.to_string(), "body");
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
    fn visual_linewise_delete_through_eof_leaves_no_empty_line() {
        // `Vd` on the last line takes the newline before it, like `dd` — the
        // buffer ends up one line shorter, not one line plus an empty one.
        let mut d = Document::new("abc\ndef");
        d.move_motion(Motion::LineDown, 1);
        d.delete_selection(true, false);
        assert_eq!(d.rope.to_string(), "abc");
        assert_eq!(d.rope.len_lines(), 1);
        assert_eq!(d.caret_line_col(), (0, 2)); // last real char, as vim
        // The line itself, not the newline taken from before it. Normalized to
        // end in one by `set_register`, so `p` pastes it as a whole line.
        assert_eq!(d.register.text, "def\n");

        // Deleting the only line empties the buffer rather than underflowing.
        let mut one = Document::new("solo");
        one.delete_selection(true, false);
        assert_eq!(one.rope.to_string(), "");

        // A trailing newline means a final empty line, so line 0 is not the
        // last line: its delete must leave that newline alone.
        let mut trailing = Document::new("ab\ncd\n");
        trailing.move_motion(Motion::LineDown, 1);
        trailing.delete_selection(true, false);
        assert_eq!(trailing.rope.to_string(), "ab\n");
    }

    #[test]
    fn visual_linewise_change_through_eof_keeps_a_line_to_type_on() {
        // `Vc` gives up the range's own newline instead of the preceding one,
        // so the insert lands on an empty line (vim `Vc`, same rule as `cip`).
        let mut d = Document::new("abc\ndef");
        d.move_motion(Motion::LineDown, 1);
        d.delete_selection(true, true);
        assert_eq!(d.rope.to_string(), "abc\n");
        assert_eq!(d.caret_line_col(), (1, 0));
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
        assert_eq!(d.single_line_edit(before).map(|e| e.line), Some(1));
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
        assert_eq!(d.single_line_edit(before).map(|e| e.line), Some(1));

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
        assert_eq!(d.single_line_edit(before).map(|e| e.line), Some(1));
        let before = rev(&d);
        d.toggle_case(1);
        assert_eq!(d.single_line_edit(before).map(|e| e.line), Some(1));

        // A checkbox toggle reports the line it flipped, not the caret's.
        let mut d = Document::new("- [ ] a\n- [ ] b");
        let before = rev(&d);
        assert!(d.toggle_task(1));
        assert_eq!(d.single_line_edit(before).map(|e| e.line), Some(1));
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
    fn insert_fence_close_lands_between_the_fences() {
        let mut d = Document::new("```rust");
        d.move_motion(Motion::LineEnd, 1);
        d.insert_fence_close("```");
        assert_eq!(d.rope.to_string(), "```rust\n\n```");
        assert_eq!(d.caret_line_col(), (1, 0));
    }

    #[test]
    fn smart_newline_carries_quote_prefix() {
        // A quote continues like a list item, and a list inside it carries too.
        let mut d = Document::new("> quoted");
        d.move_motion(Motion::LineEnd, 1);
        d.insert_newline(true, 2);
        assert_eq!(d.rope.to_string(), "> quoted\n> ");
        assert_eq!(d.caret_line_col(), (1, 2));

        // Enter on the empty quote drops back to plain text.
        d.insert_newline(true, 2);
        assert_eq!(d.rope.to_string(), "> quoted\n");

        // Mid-line: the prefix lands at the caret, so the tail stays quoted.
        let mut d = Document::new("> one two");
        d.jump_to(6);
        d.insert_newline(true, 2);
        assert_eq!(d.rope.to_string(), "> one \n> two");

        // `o` (clear_empty=false) repeats an empty quote instead of clearing it.
        let mut d = Document::new("> - a");
        d.move_motion(Motion::LineEnd, 1);
        d.insert_newline(false, 2);
        assert_eq!(d.rope.to_string(), "> - a\n> - ");
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
        d.save(false).unwrap();
        assert!(!d.is_dirty());

        let reopened = Document::open(&path).unwrap();
        assert_eq!(reopened.rope.to_string(), "hello");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_refuses_to_overwrite_an_external_change_until_forced() {
        let dir = std::env::temp_dir().join("darknotes_save_conflict_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("note.md");

        std::fs::write(&path, "theirs\n").unwrap();
        let mut d = Document::open(&path).unwrap();
        d.insert("mine");
        // No divergence yet: disk still holds what `open` read.
        d.save(false).unwrap();

        // Someone else writes the file behind us.
        std::fs::write(&path, "theirs again\n").unwrap();
        let err = d.save(false).expect_err("a diverged file must not be overwritten");
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "theirs again\n");

        // `:w!` is the way through, and it re-syncs `disk_hash` so the next
        // plain save is not still blocked by the old divergence.
        d.save(true).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), d.rope.to_string());
        d.insert("more");
        d.save(false).unwrap();

        // A file deleted out from under the buffer is recreated, not refused.
        std::fs::remove_file(&path).unwrap();
        d.insert("!");
        d.save(false).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), d.rope.to_string());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_as_refuses_an_existing_target_until_forced() {
        let dir = std::env::temp_dir().join("darknotes_save_as_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (mine, theirs) = (dir.join("mine.md"), dir.join("theirs.md"));
        std::fs::write(&theirs, "not mine\n").unwrap();

        let mut d = Document::open(&mine).unwrap();
        d.insert("mine");
        d.save(false).unwrap();

        // `:w theirs.md` must not blow away a file this buffer never loaded.
        let err = d.save_as(theirs.clone(), false).expect_err("existing target");
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(&theirs).unwrap(), "not mine\n");
        // The refusal must not have retargeted the buffer either.
        assert_eq!(d.path(), Some(mine.as_path()));

        d.save_as(theirs.clone(), true).unwrap();
        assert_eq!(std::fs::read_to_string(&theirs).unwrap(), "mine");
        assert_eq!(d.path(), Some(theirs.as_path()));

        // Naming the file it already has is a plain save, not an "exists" error.
        d.insert("!");
        d.save_as(theirs.clone(), false).unwrap();
        assert_eq!(std::fs::read_to_string(&theirs).unwrap(), "mine!");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_write_replaces_content_and_leaves_no_scratch_file() {
        let dir = std::env::temp_dir().join("darknotes_atomic_write_test");
        let _ = std::fs::remove_dir_all(&dir); // a previous failed run
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("note.md");

        // A fresh file, then a replacement — the two paths through the helper,
        // since only the second has an existing target to take a mode from.
        atomic_write(&path, "first").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first");
        atomic_write(&path, "second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");

        // The rename must consume the temp file: a leftover would show up in
        // the vault, and a dot-prefix only hides it from the sidebar.
        let left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(left, vec!["note.md".to_string()]);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // A note the user restricted stays restricted across a save.
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            atomic_write(&path, "third").unwrap();
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "save widened the note's permissions");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disk_hash_tells_own_save_from_external_write() {
        let mut path = std::env::temp_dir();
        path.push("darknotes_disk_hash_test.md");

        let mut d = Document::open(&path).unwrap();
        d.insert("hello");
        d.save(false).unwrap();
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
