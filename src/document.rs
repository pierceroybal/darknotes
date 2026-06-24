use ropey::Rope;
use std::io;
use std::path::{Path, PathBuf};

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

    #[allow(dead_code)]
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
    LineStart,
    LineEnd,
    FileEnd,
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
    /// Last delete/yank, for `p`/`P`.
    register: Register,
    /// States before each change (`u` pops); `redo` is the inverse (`Ctrl-R`).
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
}

impl Document {
    pub fn new(text: &str) -> Self {
        Self {
            rope: Rope::from_str(text),
            selections: vec![Selection::caret(0)],
            path: None,
            dirty: false,
            register: Register::default(),
            undo: Vec::new(),
            redo: Vec::new(),
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
            register: Register::default(),
            undo: Vec::new(),
            redo: Vec::new(),
        })
    }

    /// Write the buffer to its backing file. No-op for an unnamed buffer.
    pub fn save(&mut self) -> io::Result<()> {
        if let Some(path) = &self.path {
            std::fs::write(path, self.rope.to_string())?;
            self.dirty = false;
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

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    fn caret(&self) -> usize {
        self.selections[0].head
    }

    fn set_caret(&mut self, at: usize) {
        self.selections[0] = Selection::caret(at);
    }

    pub fn insert(&mut self, text: &str) {
        let at = self.caret();
        self.rope.insert(at, text);
        self.dirty = true;
        self.set_caret(at + text.chars().count());
    }

    /// Backspace: remove the char before the caret (crosses lines).
    pub fn delete_backward(&mut self) {
        let at = self.caret();
        if at == 0 {
            return;
        }
        self.rope.remove(at - 1..at);
        self.dirty = true;
        self.set_caret(at - 1);
    }

    /// Forward-delete the char at the caret (crosses lines).
    pub fn delete_forward(&mut self) {
        let at = self.caret();
        if at < self.rope.len_chars() {
            self.rope.remove(at..at + 1);
            self.dirty = true;
        }
    }

    pub fn move_motion(&mut self, m: Motion, count: usize) {
        let target = self.motion_target(m, self.caret(), count);
        self.set_caret(target);
    }

    /// `d{motion}`: delete the char range the motion sweeps over.
    pub fn delete_motion(&mut self, m: Motion, count: usize) {
        let from = self.caret();
        let to = self.motion_target(m, from, count);
        let (a, b) = (from.min(to), from.max(to));
        if a < b {
            self.set_register(self.rope.slice(a..b).to_string(), false);
            self.rope.remove(a..b);
            self.dirty = true;
            self.set_caret(a);
        }
    }

    /// `dd`: delete `count` whole lines starting at the caret's line.
    pub fn delete_lines(&mut self, count: usize) {
        let (line, _) = self.line_col_of(self.caret());
        let start = self.rope.line_to_char(line);
        let end_line = (line + count.max(1)).min(self.rope.len_lines());
        let end = if end_line >= self.rope.len_lines() {
            self.rope.len_chars()
        } else {
            self.rope.line_to_char(end_line)
        };
        if start < end {
            self.set_register(self.rope.slice(start..end).to_string(), true);
            self.rope.remove(start..end);
            self.dirty = true;
        }
        self.set_caret(start.min(self.rope.len_chars()));
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
            self.dirty = true;
        }
        // Keep the caret on a real char of the (now shorter) line.
        let last_col = self.line_len_chars(line).saturating_sub(1);
        self.set_caret(start + (from - start).min(last_col));
    }

    /// Stash text in the unnamed register. Linewise text is normalized to end in
    /// a newline so paste can treat it as whole lines regardless of EOF quirks.
    fn set_register(&mut self, text: String, linewise: bool) {
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
        let to = self.motion_target(m, from, count);
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
        self.dirty = true;
        self.set_caret(new_caret.min(self.rope.len_chars()));
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot { rope: self.rope.clone(), caret: self.caret(), dirty: self.dirty }
    }

    fn restore(&mut self, s: Snapshot) {
        self.rope = s.rope;
        self.dirty = s.dirty;
        self.set_caret(s.caret.min(self.rope.len_chars()));
    }

    /// Record the pre-change state. The editor calls this once per undoable unit
    /// (a normal-mode edit, or entering insert — the whole insert session coalesces).
    pub fn checkpoint(&mut self) {
        let snap = self.snapshot();
        self.undo.push(snap);
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
                let (line, col) = self.line_col_of(from);
                self.offset_in_line(line.saturating_sub(count), col)
            }
            Motion::LineDown => {
                let (line, col) = self.line_col_of(from);
                let last = self.rope.len_lines().saturating_sub(1);
                self.offset_in_line((line + count).min(last), col)
            }
            Motion::LineStart => {
                let (line, _) = self.line_col_of(from);
                self.rope.line_to_char(line)
            }
            Motion::LineEnd => {
                let (line, _) = self.line_col_of(from);
                self.rope.line_to_char(line) + self.line_len_chars(line)
            }
            Motion::FileEnd => {
                let last = self.rope.len_lines().saturating_sub(1);
                self.rope.line_to_char(last)
            }
            Motion::WordForward => {
                let mut p = from;
                for _ in 0..count {
                    p = self.next_word_start(p);
                }
                p
            }
            Motion::WordBackward => {
                let mut p = from;
                for _ in 0..count {
                    p = self.prev_word_start(p);
                }
                p
            }
            Motion::WordEnd => {
                let mut p = from;
                for _ in 0..count {
                    p = self.next_word_end(p);
                }
                p
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
    fn line_len_chars(&self, line: usize) -> usize {
        let slice = self.rope.line(line);
        let n = slice.len_chars();
        if slice.chars().last() == Some('\n') {
            n - 1
        } else {
            n
        }
    }

    /// Start of the next word at/after `from`. Approximates vim `w`: skip the
    /// current same-class run, then skip whitespace.
    fn next_word_start(&self, from: usize) -> usize {
        let len = self.rope.len_chars();
        let mut p = from;
        if p >= len {
            return len;
        }
        let c0 = self.rope.char(p);
        if !c0.is_whitespace() {
            let cls = char_class(c0);
            while p < len {
                let c = self.rope.char(p);
                if c.is_whitespace() || char_class(c) != cls {
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

    /// End of the next word after `from` (vim `e`): always step forward at least
    /// one char, skip whitespace, then land on the last char of that word-class
    /// run. Stays put when no word follows.
    fn next_word_end(&self, from: usize) -> usize {
        let len = self.rope.len_chars();
        let mut p = from + 1;
        while p < len && self.rope.char(p).is_whitespace() {
            p += 1;
        }
        if p >= len {
            return from;
        }
        let cls = char_class(self.rope.char(p));
        while p + 1 < len {
            let next = self.rope.char(p + 1);
            if next.is_whitespace() || char_class(next) != cls {
                break;
            }
            p += 1;
        }
        p
    }

    /// Start of the word before `from`. Approximates vim `b`.
    fn prev_word_start(&self, from: usize) -> usize {
        let mut p = from;
        while p > 0 && self.rope.char(p - 1).is_whitespace() {
            p -= 1;
        }
        if p > 0 {
            let cls = char_class(self.rope.char(p - 1));
            while p > 0 {
                let prev = self.rope.char(p - 1);
                if prev.is_whitespace() || char_class(prev) != cls {
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
    fn word_forward() {
        let d = Document::new("foo bar baz");
        assert_eq!(d.motion_target(Motion::WordForward, 0, 1), 4);
        assert_eq!(d.motion_target(Motion::WordForward, 0, 2), 8);
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
    fn delete_two_lines() {
        let mut d = Document::new("a\nb\nc");
        d.delete_lines(2);
        assert_eq!(d.rope.to_string(), "c");
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
}
