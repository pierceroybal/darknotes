use gpui::Keystroke;

use crate::document::Motion;

/// The editor's action vocabulary — edits decoupled from the keys that trigger
/// them. The editor only *executes* these; the `Vim` grammar *produces* them.
/// A different grammar (helix/emacs) would emit the same vocabulary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Move(Motion, usize),
    DeleteMotion(Motion, usize),
    DeleteLines(usize),
    /// `dj`/`dk`: linewise delete of the caret's line plus `count` adjacent
    /// lines (`up` = `dk`).
    DeleteLinesVertical { count: usize, up: bool },
    DeleteCharUnder(usize),
    YankMotion(Motion, usize),
    YankLines(usize),
    /// `p` (after) / `P` (before).
    Paste { after: bool },
    /// Visual-mode `d`/`x`/`y` over the current selection.
    DeleteSelection { linewise: bool },
    YankSelection { linewise: bool },
    /// Visual `>`/`<`: shift every selected line by `width` spaces (a count
    /// multiplies the width, vim's `2>`).
    IndentSelection { width: usize, dedent: bool },
    /// Collapse the selection back to a caret (leaving visual mode).
    CollapseSelection,
    InsertText(String),
    /// A newline that continues a markdown list (Enter, `o`). `clear_empty` (Enter
    /// only) drops the marker of an empty item instead of repeating it; the editor
    /// owns the list logic since the grammar can't see buffer text.
    Newline { clear_empty: bool },
    /// Insert-mode Tab (`dedent` = Shift-Tab). On a list item the editor shifts
    /// the whole line; off one, Tab inserts `width` spaces. The width travels in
    /// the action since the grammar owns the tab setting.
    Tab { width: usize, dedent: bool },
    DeleteBackward,
    DeleteForward,
    Undo,
    /// Reposition the viewport around the caret line (`zz`/`zt`/`zb`). The editor
    /// owns the scroll handle; the grammar only names the alignment.
    Scroll(Scroll),
    /// A submitted `:` command line (without the leading colon). The editor,
    /// not the grammar, decides what `w`/`q`/… mean.
    ExecuteCommand(String),
    /// A submitted search (`/` = forward, `?` = backward). An empty query
    /// repeats the last search. The editor owns matching and the search state.
    Search { query: String, backward: bool },
    /// `n`/`N`: jump to the next/previous match of the last search.
    SearchNext { reverse: bool, count: usize },
}

/// Where to place the caret line within the viewport (`z` scroll commands).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scroll {
    Center,
    Top,
    Bottom,
}

impl Action {
    /// Whether this action changes buffer text. The editor checkpoints undo
    /// before a mutating normal-mode command; `Undo` is excluded so it never
    /// records itself.
    pub fn mutates(&self) -> bool {
        matches!(
            self,
            Action::DeleteMotion(..)
                | Action::DeleteLines(..)
                | Action::DeleteLinesVertical { .. }
                | Action::DeleteCharUnder(..)
                | Action::DeleteSelection { .. }
                | Action::IndentSelection { .. }
                | Action::Paste { .. }
                | Action::InsertText(..)
                | Action::Newline { .. }
                | Action::Tab { .. }
                | Action::DeleteBackward
                | Action::DeleteForward
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    Command,
    /// Charwise (`v`) and linewise (`V`) visual selection.
    Visual,
    VisualLine,
}

impl Mode {
    pub fn is_visual(self) -> bool {
        matches!(self, Mode::Visual | Mode::VisualLine)
    }
}

#[derive(Clone, Copy)]
enum Op {
    Delete,
    Yank,
    Change,
}

/// Mid-sequence grammar state beyond a pending count (which is tracked
/// separately, since a count coexists with an operator — `d3w`). One key-press
/// resolves whichever variant is active. New multi-key forms (`f`/`t`, more
/// `g`-sequences) slot in as variants here, not as ad-hoc fields.
#[derive(Clone, Copy)]
enum Pending {
    None,
    /// An operator (`d`/`y`/`c`) awaiting its motion, or a doubled key (`dd`).
    Operator(Op),
    /// `g` was pressed; the next key completes a `g`-sequence (`gg`).
    GPrefix,
    /// `z` was pressed; the next key completes a `z`-sequence (`zz`/`zt`/`zb`).
    ZPrefix,
}

/// The vim grammar: a mode-aware state machine that consumes keystrokes — some
/// of which (counts, a pending sequence) build up state — and emits zero or
/// more `Action`s once a complete command is recognized.
pub struct Vim {
    pub mode: Mode,
    count: Option<usize>,
    pending: Pending,
    /// The `:`/`/`/`?` line being typed, valid only in `Mode::Command`.
    command: String,
    /// Which prompt `Mode::Command` is serving: `:` (ex command), `/` (search
    /// forward), or `?` (search backward). Decides what Enter emits.
    prompt: char,
    /// Tab width in spaces (markdown has no literal tabs).
    tab_width: usize,
    /// Insert-mode key sequence that leaves insert mode (`<Esc>`); empty = off.
    insert_exit: Vec<char>,
    /// `timeoutlen` (ms): how long the editor waits for the sequence to finish
    /// before flushing the buffered lead keys as text.
    timeoutlen: u64,
    /// Buffered lead keys of an in-progress `insert_exit` match (a proper prefix
    /// of it). The editor arms a timeout whenever this is non-empty.
    exit_buf: Vec<char>,
}

impl Vim {
    pub fn new(tab_width: usize, insert_exit: &str, timeoutlen: u64) -> Self {
        Self {
            mode: Mode::Normal,
            count: None,
            pending: Pending::None,
            command: String::new(),
            prompt: ':',
            tab_width,
            insert_exit: insert_exit.chars().collect(),
            timeoutlen,
            exit_buf: Vec::new(),
        }
    }

    /// Reset transient editing state on a buffer switch, keeping config (tab
    /// width, the insert-exit sequence, timeoutlen).
    pub fn reset(&mut self) {
        self.mode = Mode::Normal;
        self.count = None;
        self.pending = Pending::None;
        self.command.clear();
        self.exit_buf.clear();
    }

    /// `true` while the start of the insert-exit sequence is buffered, awaiting
    /// completion or a timeout flush — the editor arms its timer on this.
    pub fn exit_pending(&self) -> bool {
        !self.exit_buf.is_empty()
    }

    pub fn timeoutlen(&self) -> u64 {
        self.timeoutlen
    }

    /// Insert-exit timeout fired: surrender the buffered lead keys as literal
    /// text. Idempotent — empty once drained.
    pub fn flush_pending_exit(&mut self) -> String {
        self.exit_buf.drain(..).collect()
    }

    /// The text typed after the prompt char so far (for rendering the command
    /// line and incremental search).
    pub fn command_line(&self) -> &str {
        &self.command
    }

    /// Which prompt `Mode::Command` is serving (`:`, `/`, or `?`).
    pub fn prompt(&self) -> char {
        self.prompt
    }

    /// Enter the `:` prompt with `prefill` already typed — a palette pick that
    /// still needs an argument (e.g. `:e `) hands the line to the user here.
    pub fn start_command(&mut self, prefill: &str) {
        self.count = None;
        self.command.clear();
        self.command.push_str(prefill);
        self.prompt = ':';
        self.mode = Mode::Command;
    }

    pub fn on_key(&mut self, ks: &Keystroke) -> Vec<Action> {
        match self.mode {
            Mode::Insert => self.insert_key(ks),
            Mode::Normal => self.normal_key(ks),
            Mode::Command => self.command_key(ks),
            Mode::Visual => self.visual_key(ks, false),
            Mode::VisualLine => self.visual_key(ks, true),
        }
    }

    fn take_count(&mut self) -> usize {
        self.count.take().unwrap_or(1)
    }

    fn normal_key(&mut self, ks: &Keystroke) -> Vec<Action> {
        let key = ks.key.as_str();
        let m = &ks.modifiers;
        let shift = m.shift;

        if key == "escape" {
            self.count = None;
            self.pending = Pending::None;
            return vec![];
        }

        // Chord combos (Ctrl/Alt/Cmd) have no normal-mode vim command yet, and
        // the editor intercepts the ones it cares about (Ctrl-S/N/P) first.
        // Without this, `Ctrl-a` would fall through and trigger `a`.
        if m.control || m.alt || m.platform {
            self.count = None;
            self.pending = Pending::None;
            return vec![];
        }

        // Count digits. '0' counts only mid-count; with no count pending it's the
        // line-start motion handled below. Runs before the pending-sequence step
        // so a count between operator and motion accumulates (`d3w`).
        if key.len() == 1 {
            if let Some(d) = key.chars().next().unwrap().to_digit(10) {
                let d = d as usize;
                if d != 0 || self.count.is_some() {
                    self.count = Some(self.count.unwrap_or(0) * 10 + d);
                    return vec![];
                }
            }
        }

        // An active sequence (operator-pending, `g`-prefix) consumes this key.
        match std::mem::replace(&mut self.pending, Pending::None) {
            Pending::Operator(op) => return self.apply_operator(op, key, shift),
            Pending::GPrefix => return self.complete_g_prefix(key),
            Pending::ZPrefix => return self.complete_z_prefix(key),
            Pending::None => {}
        }

        // Motions come from one table shared with visual and operator-pending;
        // letters arrive lowercased with `shift` separate, so capitals match as
        // (key, true).
        if let Some(spec) = motion(key, shift) {
            return vec![Action::Move(spec.motion, self.take_count())];
        }
        match (key, shift) {
            // `g` starts a sequence (`gg`); `G` is a single-key motion in the table.
            ("g", false) => {
                self.pending = Pending::GPrefix;
                vec![]
            }
            ("z", false) => {
                self.pending = Pending::ZPrefix;
                vec![]
            }
            ("x", _) => vec![Action::DeleteCharUnder(self.take_count())],
            // `s`: substitute — delete char(s) under cursor, then insert.
            ("s", false) => {
                let n = self.take_count();
                self.enter_insert(vec![Action::DeleteCharUnder(n)])
            }
            ("d", false) => {
                self.pending = Pending::Operator(Op::Delete);
                vec![]
            }
            ("y", false) => {
                self.pending = Pending::Operator(Op::Yank);
                vec![]
            }
            ("c", false) => {
                self.pending = Pending::Operator(Op::Change);
                vec![]
            }
            ("y", true) => vec![Action::YankLines(self.take_count())], // Y == yy
            // ponytail: count (`3p`) ignored — paste once. Add repeat when needed.
            ("p", false) => {
                self.count = None;
                vec![Action::Paste { after: true }]
            }
            ("p", true) => {
                self.count = None;
                vec![Action::Paste { after: false }]
            }
            // ponytail: count (`3u`) ignored — undo one step.
            ("u", false) => {
                self.count = None;
                vec![Action::Undo]
            }
            ("i", false) => self.enter_insert(vec![]),
            ("i", true) => self.enter_insert(vec![Action::Move(Motion::LineStart, 1)]),
            ("a", false) => self.enter_insert(vec![Action::Move(Motion::CharRight, 1)]),
            ("a", true) => self.enter_insert(vec![Action::Move(Motion::LineEnd, 1)]),
            ("o", false) => self.enter_insert(vec![
                Action::Move(Motion::LineEnd, 1),
                Action::Newline { clear_empty: false },
            ]),
            ("o", true) => self.enter_insert(vec![
                Action::Move(Motion::LineStart, 1),
                Action::InsertText("\n".into()),
                Action::Move(Motion::LineUp, 1),
            ]),
            // `C`: change to end of line — delete to EOL, then insert. Same as `c$`.
            ("c", true) => self.enter_insert(vec![Action::DeleteMotion(Motion::LineEnd, 1)]),
            // The current caret is already the selection's anchor (normal-mode
            // ops leave a bare caret), so entering visual just flips the mode.
            ("v", false) => {
                self.count = None;
                self.mode = Mode::Visual;
                vec![]
            }
            ("v", true) => {
                self.count = None;
                self.mode = Mode::VisualLine;
                vec![]
            }
            (":", _) | ("/", _) | ("?", _) => {
                self.count = None;
                self.command.clear();
                self.prompt = key.chars().next().unwrap();
                self.mode = Mode::Command;
                vec![]
            }
            ("n", false) => vec![Action::SearchNext { reverse: false, count: self.take_count() }],
            ("n", true) => vec![Action::SearchNext { reverse: true, count: self.take_count() }],
            _ => {
                self.count = None;
                vec![]
            }
        }
    }

    fn command_key(&mut self, ks: &Keystroke) -> Vec<Action> {
        let m = &ks.modifiers;
        match ks.key.as_str() {
            "escape" => {
                self.command.clear();
                self.mode = Mode::Normal;
                vec![]
            }
            "enter" => {
                self.mode = Mode::Normal;
                let text = std::mem::take(&mut self.command);
                if self.prompt == ':' {
                    vec![Action::ExecuteCommand(text)]
                } else {
                    vec![Action::Search { query: text, backward: self.prompt == '?' }]
                }
            }
            // Backspacing past the prompt char exits command mode.
            "backspace" => {
                if self.command.is_empty() {
                    self.mode = Mode::Normal;
                } else {
                    self.command.pop();
                }
                vec![]
            }
            _ if !m.control && !m.platform && !m.alt => {
                if let Some(s) = &ks.key_char {
                    self.command.push_str(s);
                }
                vec![]
            }
            _ => vec![],
        }
    }

    /// Visual mode (`line` = linewise `V`). Motions extend the selection's head
    /// (the editor routes `Move` to `extend_motion` while visual); `d`/`x`/`y`
    /// act on the span and return to normal; `v`/`V` toggle or switch submode.
    fn visual_key(&mut self, ks: &Keystroke, line: bool) -> Vec<Action> {
        let key = ks.key.as_str();
        let m = &ks.modifiers;
        let shift = m.shift;

        if key == "escape" {
            self.count = None;
            self.mode = Mode::Normal;
            return vec![Action::CollapseSelection];
        }
        if m.control || m.alt || m.platform {
            self.count = None;
            return vec![];
        }
        // Count digits (mid-count `0` included; a bare `0` is the line-start motion).
        if key.len() == 1 {
            if let Some(d) = key.chars().next().unwrap().to_digit(10) {
                let d = d as usize;
                if d != 0 || self.count.is_some() {
                    self.count = Some(self.count.unwrap_or(0) * 10 + d);
                    return vec![];
                }
            }
        }

        // Toggle off (same key) or switch submode (the other key).
        let toggle = |me: &mut Self, target: Mode| {
            me.count = None;
            me.mode = target;
            if target == Mode::Normal {
                vec![Action::CollapseSelection]
            } else {
                vec![]
            }
        };
        match (key, shift) {
            ("v", false) => return toggle(self, if line { Mode::Visual } else { Mode::Normal }),
            ("v", true) => return toggle(self, if line { Mode::Normal } else { Mode::VisualLine }),
            ("d", false) | ("x", _) => {
                self.count = None;
                self.mode = Mode::Normal;
                return vec![Action::DeleteSelection { linewise: line }];
            }
            ("y", false) => {
                self.count = None;
                self.mode = Mode::Normal;
                return vec![Action::YankSelection { linewise: line }];
            }
            (">", _) | ("<", _) => {
                let width = self.tab_width * self.take_count();
                self.mode = Mode::Normal;
                return vec![Action::IndentSelection { width, dedent: key == "<" }];
            }
            _ => {}
        }

        if let Some(spec) = motion(key, shift) {
            return vec![Action::Move(spec.motion, self.take_count())];
        }
        self.count = None;
        vec![]
    }

    fn enter_insert(&mut self, actions: Vec<Action>) -> Vec<Action> {
        self.count = None;
        self.mode = Mode::Insert;
        actions
    }

    fn insert_key(&mut self, ks: &Keystroke) -> Vec<Action> {
        let m = &ks.modifiers;
        let key = ks.key.as_str();

        // Plain printable input drives the insert-exit matcher: a single char may
        // extend a pending sequence, complete it (→ leave insert), or break it.
        if !m.control && !m.platform && !m.alt {
            if let Some(s) = &ks.key_char {
                if s.chars().count() == 1 {
                    return self.feed_insert_char(s.chars().next().unwrap());
                }
                // Multi-char input (IME, etc.) can't be part of the sequence.
                let mut out = self.flush_exit_buf();
                out.push(Action::InsertText(s.clone()));
                return out;
            }
        }

        // Any other key ends a pending sequence: flush the buffered keys as text,
        // then handle the key itself.
        let mut out = self.flush_exit_buf();
        match key {
            "escape" => {
                self.mode = Mode::Normal;
                out.push(Action::Move(Motion::CharLeft, 1)); // vim nudges left on exit
            }
            "left" => out.push(Action::Move(Motion::CharLeft, 1)),
            "right" => out.push(Action::Move(Motion::CharRight, 1)),
            "up" => out.push(Action::Move(Motion::LineUp, 1)),
            "down" => out.push(Action::Move(Motion::LineDown, 1)),
            "backspace" => out.push(Action::DeleteBackward),
            "delete" => out.push(Action::DeleteForward),
            "enter" => out.push(Action::Newline { clear_empty: true }),
            // A markdown buffer has no literal tabs. On a list line the editor
            // shifts the whole line; elsewhere Tab inserts `tab_width` spaces.
            // Shift-Tab dedents.
            "tab" if !m.shift => out.push(Action::Tab { width: self.tab_width, dedent: false }),
            "tab" if m.shift => out.push(Action::Tab { width: self.tab_width, dedent: true }),
            _ => {}
        }
        out
    }

    /// Run one printable char through the insert-exit matcher. With no sequence
    /// configured it's a plain insert. Otherwise a char that extends the match
    /// is buffered (completing it leaves insert mode, emitting nothing); a char
    /// that breaks the match flushes the buffer, then either starts a fresh
    /// match or inserts.
    fn feed_insert_char(&mut self, ch: char) -> Vec<Action> {
        if self.insert_exit.is_empty() {
            return vec![Action::InsertText(ch.to_string())];
        }
        if self.insert_exit.get(self.exit_buf.len()) == Some(&ch) {
            self.exit_buf.push(ch);
            if self.exit_buf.len() == self.insert_exit.len() {
                self.exit_buf.clear();
                self.mode = Mode::Normal;
                return vec![Action::Move(Motion::CharLeft, 1)];
            }
            return vec![]; // partial match; the editor arms the timeout
        }
        // ch doesn't extend the buffer — flush it, then judge ch on its own (it
        // may itself begin a fresh match, e.g. the second `j` of `jj` after a
        // broken `jk`).
        let mut out = self.flush_exit_buf();
        if self.insert_exit.first() == Some(&ch) {
            self.exit_buf.push(ch);
            if self.insert_exit.len() == 1 {
                self.exit_buf.clear();
                self.mode = Mode::Normal;
                out.push(Action::Move(Motion::CharLeft, 1));
            }
        } else {
            out.push(Action::InsertText(ch.to_string()));
        }
        out
    }

    /// Surrender buffered insert-exit lead keys as literal text (the sequence
    /// broke or was interrupted). Empty when nothing is pending.
    fn flush_exit_buf(&mut self) -> Vec<Action> {
        if self.exit_buf.is_empty() {
            return vec![];
        }
        vec![Action::InsertText(self.exit_buf.drain(..).collect())]
    }
}

impl Vim {
    /// Resolve a pending operator against the key that follows it: a doubled
    /// operator key is linewise (`dd`/`yy`/`cc`); otherwise the key must name an
    /// operator-target motion. `c` deletes then enters insert (like `C` = `c$`).
    fn apply_operator(&mut self, op: Op, key: &str, shift: bool) -> Vec<Action> {
        let count = self.count.take().unwrap_or(1);
        let doubled = matches!(
            (op, key),
            (Op::Delete, "d") | (Op::Yank, "y") | (Op::Change, "c")
        );
        if doubled {
            return match op {
                Op::Delete => vec![Action::DeleteLines(count)],
                Op::Yank => vec![Action::YankLines(count)],
                // ponytail: single-line `cc` (count ignored) — clear the line,
                // enter insert. Multi-line `2cc` when it's wanted.
                Op::Change => self.enter_insert(vec![
                    Action::Move(Motion::LineStart, 1),
                    Action::DeleteMotion(Motion::LineEnd, 1),
                ]),
            };
        }
        match motion(key, shift) {
            Some(spec) if spec.op_target => match op {
                Op::Delete => vec![Action::DeleteMotion(spec.motion, count)],
                Op::Yank => vec![Action::YankMotion(spec.motion, count)],
                Op::Change => self.enter_insert(vec![Action::DeleteMotion(spec.motion, count)]),
            },
            // `dj`/`dk`: the line motion isn't an op-target (charwise would be
            // surprising), so handle it here as a linewise delete. `yj`/`cj` etc.
            // would join this arm when wanted.
            Some(spec) if matches!(spec.motion, Motion::LineUp | Motion::LineDown) => match op {
                Op::Delete => vec![Action::DeleteLinesVertical {
                    count,
                    up: matches!(spec.motion, Motion::LineUp),
                }],
                _ => vec![],
            },
            _ => vec![], // unsupported target → abort the operator
        }
    }

    /// Complete a `g`-sequence: `gg` jumps to file start; anything else aborts.
    fn complete_g_prefix(&mut self, key: &str) -> Vec<Action> {
        self.count = None; // ponytail: `2gg` (go to line N) ignored — file start.
        if key == "g" {
            vec![Action::Move(Motion::FileStart, 1)]
        } else {
            vec![]
        }
    }

    /// Complete a `z`-sequence: position the caret line in the viewport
    /// (`zz` center, `zt` top, `zb` bottom); anything else aborts. The
    /// `+first-non-blank` variants (`z.`/`z<CR>`/`z-`) wait on a `^` motion;
    /// `zh`/`zl` (horizontal) and `zf`/`zo` (folds) wait on those features.
    fn complete_z_prefix(&mut self, key: &str) -> Vec<Action> {
        self.count = None;
        match key {
            "z" => vec![Action::Scroll(Scroll::Center)],
            "t" => vec![Action::Scroll(Scroll::Top)],
            "b" => vec![Action::Scroll(Scroll::Bottom)],
            _ => vec![],
        }
    }
}

/// A cursor motion plus whether it's a valid operator target. One table read by
/// normal mode, visual mode, and operator-pending — a new motion is one entry
/// here. `j`/`k`/`e`/`G` are non-targets (`dj` is linewise; `de`/`dG` are just a
/// flag flip away). `G` (`shift+g`) is file-end; lowercase `g` is a prefix.
struct MotionSpec {
    motion: Motion,
    op_target: bool,
}

fn motion(key: &str, shift: bool) -> Option<MotionSpec> {
    let (motion, op_target) = match (key, shift) {
        ("h", _) | ("left", _) => (Motion::CharLeft, true),
        ("l", _) | ("right", _) => (Motion::CharRight, true),
        ("k", _) | ("up", _) => (Motion::LineUp, false),
        ("j", _) | ("down", _) => (Motion::LineDown, false),
        ("w", _) => (Motion::WordForward, true),
        ("b", _) => (Motion::WordBackward, true),
        ("e", _) => (Motion::WordEnd, false),
        ("0", _) => (Motion::LineStart, true),
        ("$", _) => (Motion::LineEnd, true),
        ("g", true) => (Motion::FileEnd, false),
        _ => return None,
    };
    Some(MotionSpec { motion, op_target })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Keystroke, Modifiers};

    fn k(key: &str) -> Keystroke {
        Keystroke {
            key: key.into(),
            key_char: Some(key.into()),
            modifiers: Modifiers::default(),
        }
    }

    fn named(key: &str) -> Keystroke {
        Keystroke { key: key.into(), key_char: None, modifiers: Modifiers::default() }
    }

    /// Default test grammar: 2-space tabs, no insert-exit sequence.
    fn vim() -> Vim {
        Vim::new(2, "", 1000)
    }

    #[test]
    fn count_then_motion() {
        let mut v = vim();
        assert!(v.on_key(&k("3")).is_empty());
        assert_eq!(v.on_key(&k("j")), vec![Action::Move(Motion::LineDown, 3)]);
    }

    #[test]
    fn dd_deletes_lines() {
        let mut v = vim();
        assert!(v.on_key(&k("d")).is_empty());
        assert_eq!(v.on_key(&k("d")), vec![Action::DeleteLines(1)]);
    }

    #[test]
    fn count_before_operator() {
        let mut v = vim();
        v.on_key(&k("3"));
        v.on_key(&k("d"));
        assert_eq!(v.on_key(&k("d")), vec![Action::DeleteLines(3)]);
    }

    #[test]
    fn dw_deletes_word() {
        let mut v = vim();
        v.on_key(&k("d"));
        assert_eq!(v.on_key(&k("w")), vec![Action::DeleteMotion(Motion::WordForward, 1)]);
    }

    #[test]
    fn dj_dk_delete_adjacent_lines() {
        let mut v = vim();
        v.on_key(&k("d"));
        assert_eq!(
            v.on_key(&k("j")),
            vec![Action::DeleteLinesVertical { count: 1, up: false }]
        );
        v.on_key(&k("2"));
        v.on_key(&k("d"));
        assert_eq!(
            v.on_key(&k("k")),
            vec![Action::DeleteLinesVertical { count: 2, up: true }]
        );
    }

    #[test]
    fn capital_c_changes_to_eol() {
        let mut v = vim();
        let c_shift = Keystroke {
            key: "c".into(),
            key_char: Some("C".into()),
            modifiers: Modifiers { shift: true, ..Default::default() },
        };
        assert_eq!(v.on_key(&c_shift), vec![Action::DeleteMotion(Motion::LineEnd, 1)]);
        assert_eq!(v.mode, Mode::Insert);
    }

    #[test]
    fn gg_moves_to_file_start() {
        let mut v = vim();
        assert!(v.on_key(&k("g")).is_empty()); // prefix armed, no action yet
        assert_eq!(v.on_key(&k("g")), vec![Action::Move(Motion::FileStart, 1)]);
    }

    #[test]
    fn g_then_other_key_aborts() {
        let mut v = vim();
        v.on_key(&k("g"));
        assert!(v.on_key(&k("x")).is_empty()); // `gx` unbound → no-op, not delete
        assert_eq!(v.mode, Mode::Normal);
    }

    #[test]
    fn cw_changes_word() {
        let mut v = vim();
        assert!(v.on_key(&k("c")).is_empty());
        assert_eq!(
            v.on_key(&k("w")),
            vec![Action::DeleteMotion(Motion::WordForward, 1)]
        );
        assert_eq!(v.mode, Mode::Insert); // change = delete then insert
    }

    #[test]
    fn cc_changes_line() {
        let mut v = vim();
        v.on_key(&k("c"));
        assert_eq!(
            v.on_key(&k("c")),
            vec![
                Action::Move(Motion::LineStart, 1),
                Action::DeleteMotion(Motion::LineEnd, 1)
            ]
        );
        assert_eq!(v.mode, Mode::Insert);
    }

    #[test]
    fn z_scroll_commands() {
        let mut v = vim();
        assert!(v.on_key(&k("z")).is_empty()); // prefix armed
        assert_eq!(v.on_key(&k("z")), vec![Action::Scroll(Scroll::Center)]);
        v.on_key(&k("z"));
        assert_eq!(v.on_key(&k("t")), vec![Action::Scroll(Scroll::Top)]);
        v.on_key(&k("z"));
        assert_eq!(v.on_key(&k("b")), vec![Action::Scroll(Scroll::Bottom)]);
    }

    #[test]
    fn z_then_unknown_aborts() {
        let mut v = vim();
        v.on_key(&k("z"));
        assert!(v.on_key(&k("q")).is_empty());
        assert_eq!(v.mode, Mode::Normal);
    }

    #[test]
    fn yy_yanks_lines() {
        let mut v = vim();
        assert!(v.on_key(&k("y")).is_empty());
        assert_eq!(v.on_key(&k("y")), vec![Action::YankLines(1)]);
    }

    #[test]
    fn yw_yanks_word() {
        let mut v = vim();
        v.on_key(&k("y"));
        assert_eq!(v.on_key(&k("w")), vec![Action::YankMotion(Motion::WordForward, 1)]);
    }

    #[test]
    fn p_and_capital_p_paste() {
        let mut v = vim();
        assert_eq!(v.on_key(&k("p")), vec![Action::Paste { after: true }]);
        let p_shift = Keystroke {
            key: "p".into(),
            key_char: Some("P".into()),
            modifiers: Modifiers { shift: true, ..Default::default() },
        };
        assert_eq!(v.on_key(&p_shift), vec![Action::Paste { after: false }]);
    }

    #[test]
    fn u_undoes() {
        let mut v = vim();
        assert_eq!(v.on_key(&k("u")), vec![Action::Undo]);
    }

    #[test]
    fn enter_and_exit_insert() {
        let mut v = vim();
        v.on_key(&k("i"));
        assert_eq!(v.mode, Mode::Insert);
        v.on_key(&named("escape"));
        assert_eq!(v.mode, Mode::Normal);
    }

    #[test]
    fn insert_emits_text() {
        let mut v = vim();
        v.on_key(&k("i"));
        assert_eq!(v.on_key(&k("x")), vec![Action::InsertText("x".into())]);
    }

    #[test]
    fn tab_and_shift_tab_emit_tab_action() {
        let mut v = vim();
        v.on_key(&k("i"));
        assert_eq!(
            v.on_key(&named("tab")),
            vec![Action::Tab { width: 2, dedent: false }]
        );
        let shift_tab = Keystroke {
            key: "tab".into(),
            key_char: None,
            modifiers: Modifiers { shift: true, ..Default::default() },
        };
        assert_eq!(v.on_key(&shift_tab), vec![Action::Tab { width: 2, dedent: true }]);
    }

    #[test]
    fn normal_mode_letters_are_not_text() {
        // Pressing a printable in normal mode must never insert it.
        let mut v = vim();
        assert!(v.on_key(&k("z")).is_empty());
    }

    #[test]
    fn ex_command_buffers_and_submits() {
        let mut v = vim();
        assert!(v.on_key(&k(":")).is_empty());
        assert_eq!(v.mode, Mode::Command);
        v.on_key(&k("w"));
        v.on_key(&k("q"));
        assert_eq!(v.command_line(), "wq");
        let out = v.on_key(&named("enter"));
        assert_eq!(out, vec![Action::ExecuteCommand("wq".into())]);
        assert_eq!(v.mode, Mode::Normal);
    }

    #[test]
    fn ex_command_escape_cancels() {
        let mut v = vim();
        v.on_key(&k(":"));
        v.on_key(&k("q"));
        v.on_key(&named("escape"));
        assert_eq!(v.mode, Mode::Normal);
        assert_eq!(v.command_line(), "");
    }

    fn shift(key: &str, ch: &str) -> Keystroke {
        Keystroke {
            key: key.into(),
            key_char: Some(ch.into()),
            modifiers: Modifiers { shift: true, ..Default::default() },
        }
    }

    #[test]
    fn v_and_capital_v_enter_visual() {
        let mut v = vim();
        v.on_key(&k("v"));
        assert_eq!(v.mode, Mode::Visual);
        let mut v = vim();
        v.on_key(&shift("v", "V"));
        assert_eq!(v.mode, Mode::VisualLine);
    }

    #[test]
    fn visual_motion_extends_then_delete_returns_to_normal() {
        let mut v = vim();
        v.on_key(&shift("v", "V"));
        assert_eq!(v.on_key(&k("j")), vec![Action::Move(Motion::LineDown, 1)]);
        assert_eq!(
            v.on_key(&k("d")),
            vec![Action::DeleteSelection { linewise: true }]
        );
        assert_eq!(v.mode, Mode::Normal);
    }

    #[test]
    fn visual_indent_returns_to_normal() {
        // `>` indents by tab_width and leaves visual; a count multiplies (`2<`).
        let mut v = vim();
        v.on_key(&shift("v", "V"));
        assert_eq!(
            v.on_key(&shift(">", ">")),
            vec![Action::IndentSelection { width: 2, dedent: false }]
        );
        assert_eq!(v.mode, Mode::Normal);

        let mut v = vim();
        v.on_key(&shift("v", "V"));
        v.on_key(&k("2"));
        assert_eq!(
            v.on_key(&shift("<", "<")),
            vec![Action::IndentSelection { width: 4, dedent: true }]
        );
        assert_eq!(v.mode, Mode::Normal);
    }

    #[test]
    fn visual_count_then_motion() {
        let mut v = vim();
        v.on_key(&k("v"));
        v.on_key(&k("3"));
        assert_eq!(v.on_key(&k("j")), vec![Action::Move(Motion::LineDown, 3)]);
    }

    #[test]
    fn visual_escape_collapses() {
        let mut v = vim();
        v.on_key(&k("v"));
        assert_eq!(v.on_key(&named("escape")), vec![Action::CollapseSelection]);
        assert_eq!(v.mode, Mode::Normal);
    }

    #[test]
    fn v_toggles_off_capital_v_switches_submode() {
        let mut v = vim();
        v.on_key(&k("v"));
        v.on_key(&k("v")); // same key → leave visual
        assert_eq!(v.mode, Mode::Normal);

        let mut v = vim();
        v.on_key(&k("v"));
        v.on_key(&shift("v", "V")); // other key → switch to linewise
        assert_eq!(v.mode, Mode::VisualLine);
    }

    #[test]
    fn ctrl_chords_ignored_in_normal() {
        // Ctrl-a must not fall through to the `a` (enter-insert) command.
        let mut v = vim();
        let ctrl_a = Keystroke {
            key: "a".into(),
            key_char: Some("a".into()),
            modifiers: Modifiers { control: true, ..Default::default() },
        };
        assert!(v.on_key(&ctrl_a).is_empty());
        assert_eq!(v.mode, Mode::Normal);
    }

    #[test]
    fn insert_exit_sequence_leaves_insert() {
        let mut v = Vim::new(2, "jk", 1000);
        v.on_key(&k("i"));
        assert!(v.on_key(&k("j")).is_empty()); // lead key buffered, nothing typed
        assert!(v.exit_pending());
        assert_eq!(v.on_key(&k("k")), vec![Action::Move(Motion::CharLeft, 1)]);
        assert_eq!(v.mode, Mode::Normal);
        assert!(!v.exit_pending());
    }

    #[test]
    fn insert_exit_broken_flushes_lead_then_char() {
        let mut v = Vim::new(2, "jk", 1000);
        v.on_key(&k("i"));
        v.on_key(&k("j"));
        // `ju`: not the sequence — emit the buffered `j`, then `u`.
        assert_eq!(
            v.on_key(&k("u")),
            vec![Action::InsertText("j".into()), Action::InsertText("u".into())]
        );
        assert_eq!(v.mode, Mode::Insert);
    }

    #[test]
    fn insert_exit_timeout_flushes_lead() {
        let mut v = Vim::new(2, "jk", 1000);
        v.on_key(&k("i"));
        v.on_key(&k("j"));
        assert_eq!(v.flush_pending_exit(), "j");
        assert_eq!(v.flush_pending_exit(), ""); // idempotent
        assert!(!v.exit_pending());
    }

    #[test]
    fn insert_exit_repeated_lead_char() {
        // `jk` sequence: typing `jj` flushes the first `j`, then re-buffers the
        // second as a fresh lead key.
        let mut v = Vim::new(2, "jk", 1000);
        v.on_key(&k("i"));
        v.on_key(&k("j"));
        assert_eq!(v.on_key(&k("j")), vec![Action::InsertText("j".into())]);
        assert!(v.exit_pending());
    }

    #[test]
    fn search_prompt_buffers_and_submits() {
        let mut v = vim();
        assert!(v.on_key(&k("/")).is_empty());
        assert_eq!(v.mode, Mode::Command);
        assert_eq!(v.prompt(), '/');
        v.on_key(&k("f"));
        v.on_key(&k("o"));
        assert_eq!(v.command_line(), "fo");
        assert_eq!(
            v.on_key(&named("enter")),
            vec![Action::Search { query: "fo".into(), backward: false }]
        );
        assert_eq!(v.mode, Mode::Normal);
    }

    #[test]
    fn question_mark_searches_backward() {
        let mut v = vim();
        v.on_key(&k("?"));
        assert_eq!(v.prompt(), '?');
        v.on_key(&k("x"));
        assert_eq!(
            v.on_key(&named("enter")),
            vec![Action::Search { query: "x".into(), backward: true }]
        );
    }

    #[test]
    fn search_prompt_escape_cancels() {
        let mut v = vim();
        v.on_key(&k("/"));
        v.on_key(&k("q"));
        assert!(v.on_key(&named("escape")).is_empty());
        assert_eq!(v.mode, Mode::Normal);
        assert_eq!(v.command_line(), "");
    }

    #[test]
    fn colon_after_search_is_still_a_command() {
        // The prompt char must reset when `:` reopens command mode.
        let mut v = vim();
        v.on_key(&k("/"));
        v.on_key(&named("escape"));
        v.on_key(&k(":"));
        v.on_key(&k("w"));
        assert_eq!(v.on_key(&named("enter")), vec![Action::ExecuteCommand("w".into())]);
    }

    #[test]
    fn n_and_capital_n_repeat_search() {
        let mut v = vim();
        assert_eq!(
            v.on_key(&k("n")),
            vec![Action::SearchNext { reverse: false, count: 1 }]
        );
        assert_eq!(
            v.on_key(&shift("n", "N")),
            vec![Action::SearchNext { reverse: true, count: 1 }]
        );
        v.on_key(&k("3"));
        assert_eq!(
            v.on_key(&k("n")),
            vec![Action::SearchNext { reverse: false, count: 3 }]
        );
    }

    #[test]
    fn insert_exit_disabled_inserts_immediately() {
        let mut v = vim(); // no sequence configured
        v.on_key(&k("i"));
        assert_eq!(v.on_key(&k("j")), vec![Action::InsertText("j".into())]);
        assert!(!v.exit_pending());
    }
}
