use gpui::Keystroke;

use crate::document::{Motion, TextObject};

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
    /// `d{object}`/`c{object}` (`change` leaves an empty line on linewise
    /// objects and is followed by insert mode).
    DeleteObject { obj: TextObject, change: bool },
    YankObject(TextObject),
    /// Visual-mode `i`/`a` (`viw`, `va(`): select the object around the caret.
    SelectObject(TextObject),
    /// `p` (after) / `P` (before).
    Paste { after: bool },
    /// Visual-mode `d`/`x`/`y`/`c` over the current selection. `change` (`c`)
    /// spares a linewise span's last newline and is followed by insert mode.
    DeleteSelection { linewise: bool, change: bool },
    YankSelection { linewise: bool },
    /// `J` (`space`) / `gJ`: join `count` lines into one.
    JoinLines { count: usize, space: bool },
    /// `r{char}`: overwrite `count` chars at the caret.
    ReplaceChar(char, usize),
    /// `~`: flip the case of `count` chars at the caret.
    ToggleCase(usize),
    /// Visual `>`/`<`: shift every selected line by `width` spaces (a count
    /// multiplies the width, vim's `2>`).
    IndentSelection { width: usize, dedent: bool },
    /// Normal `>>`/`<<`: shift the caret's line plus `count - 1` below it by
    /// `width` spaces (vim: the count names lines, not widths).
    IndentLines { width: usize, dedent: bool, count: usize },
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
    /// `Ctrl-E`/`Ctrl-Y`: slide the viewport `count` visual rows down/up. The
    /// caret stays put until the view would drop it, then it's pulled to the
    /// nearest edge. Resolved by the editor — only its row cache knows visual
    /// rows and viewport height.
    ScrollLines { down: bool, count: usize },
    /// `Ctrl-D`/`Ctrl-U`: half a viewport down/up, caret and view moving
    /// together so the caret keeps its screen position.
    ScrollHalf { down: bool },
    /// A submitted `:` command line (without the leading colon). The editor,
    /// not the grammar, decides what `w`/`q`/… mean.
    ExecuteCommand(String),
    /// A submitted search (`/` = forward, `?` = backward). An empty query
    /// repeats the last search. The editor owns matching and the search state.
    Search { query: String, backward: bool },
    /// `n`/`N`: jump to the next/previous match of the last search.
    SearchNext { reverse: bool, count: usize },
    /// `gt`/`gT`: cycle to the next/previous buffer. The buffer list and its
    /// wrap live in the editor; the grammar only names the direction.
    BufferNext,
    BufferPrev,
    /// `gd`/`gf`/`gx`: follow the link under the caret (wikilink → note,
    /// URL → browser). Link detection lives in the editor.
    FollowLink,
    /// `gj`/`gk`: move one *visual* row (a wrapped line's rows count
    /// individually). Resolved by the editor — only its row cache knows
    /// wrap boundaries.
    MoveDisplay { down: bool },
    /// Normal-mode Enter: flip the `[ ]`/`[x]` task box on the caret's line.
    /// The editor owns detection (the grammar can't see buffer text) and
    /// checkpoints undo only when a box is present — which is why this is
    /// deliberately absent from `mutates()`.
    ToggleTask,
    /// `.`: replay the last change. The editor owns the recording (it sees
    /// the applied actions, insert session included) and expands this before
    /// dispatch; the grammar only names the request.
    Repeat,
    /// `Ctrl-O`/`Ctrl-I`: walk the jump history older/newer. The editor owns
    /// the list (it spans buffers) and the file switching.
    JumpBack,
    JumpForward,
    /// `m{a}`: name the caret's position. Lowercase marks live on the buffer,
    /// uppercase on the editor — the editor owns both stores.
    SetMark(char),
    /// `` `{a} `` / `'{a}`: to the mark, exactly or to its line's first
    /// non-blank. `''`/`` `` `` name the jumplist's newest entry.
    JumpToMark { name: char, line: bool },
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
                | Action::DeleteObject { .. }
                | Action::DeleteSelection { .. }
                | Action::IndentSelection { .. }
                | Action::IndentLines { .. }
                | Action::JoinLines { .. }
                | Action::ReplaceChar(..)
                | Action::ToggleCase(..)
                | Action::Paste { .. }
                | Action::InsertText(..)
                | Action::Newline { .. }
                | Action::Tab { .. }
                | Action::DeleteBackward
                | Action::DeleteForward
        )
    }

    /// Whether this action stores text in the unnamed register (deletes and
    /// yanks). The editor mirrors the register to the system clipboard after
    /// these, vim's `clipboard=unnamed`.
    pub fn writes_register(&self) -> bool {
        matches!(
            self,
            Action::DeleteMotion(..)
                | Action::DeleteLines(..)
                | Action::DeleteLinesVertical { .. }
                | Action::DeleteCharUnder(..)
                | Action::DeleteObject { .. }
                | Action::DeleteSelection { .. }
                | Action::YankMotion(..)
                | Action::YankLines(..)
                | Action::YankObject(..)
                | Action::YankSelection { .. }
        )
    }

    /// Whether this records a jumplist entry — the position it leaves becomes a
    /// `Ctrl-O` target. Only within-buffer jumps: anything that opens a file by
    /// path records in `Editor::open_path` instead, and recording a
    /// `JumpBack`/`JumpForward` would truncate the history being walked.
    /// `JumpToMark` may or may not cross files, so it records in its handler.
    pub fn is_jump(&self) -> bool {
        match self {
            Action::Move(m, _) => m.is_jump(),
            Action::Search { .. } | Action::SearchNext { .. } => true,
            _ => false,
        }
    }

    /// Deletes that can remove or merge list lines — the editor renumbers the
    /// surrounding ordered list after these. Insert-mode `DeleteBackward`/
    /// `DeleteForward` are excluded: renumbering per keystroke would fight a
    /// hand-edit of a marker's digits mid-typing.
    pub fn renumbers(&self) -> bool {
        matches!(
            self,
            Action::DeleteMotion(..)
                | Action::DeleteLines(..)
                | Action::DeleteLinesVertical { .. }
                | Action::DeleteCharUnder(..)
                | Action::DeleteObject { .. }
                | Action::DeleteSelection { .. }
                // A join can merge two list items into one.
                | Action::JoinLines { .. }
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
    /// `f`/`F`/`t`/`T` was pressed; the next key is the literal target char.
    /// `make` builds the motion from it, so the four keys differ only by which
    /// constructor they park here. `op` is `None` for a bare motion (normal or
    /// visual mode); the count rides along because whoever armed this consumed
    /// it.
    Find { make: fn(char) -> Motion, op: Option<Op>, count: usize },
    /// `r` was pressed; the next key is the literal replacement char.
    Replace { count: usize },
    /// `i`/`a` was pressed; the next key names the text object (`diw`, `cap`,
    /// `vi(`). `op` is `None` in visual mode, where the object becomes the
    /// selection instead of an operator's target.
    Object { op: Option<Op>, around: bool },
    /// `g` was pressed; the next key completes a `g`-sequence (`gg`).
    GPrefix,
    /// `z` was pressed; the next key completes a `z`-sequence (`zz`/`zt`/`zb`).
    ZPrefix,
    /// `>`/`<` awaiting its double (`>>`/`<<`); any other key aborts.
    Indent { dedent: bool },
    /// `m` / `'` / `` ` `` was pressed; the next key names the mark. `set`
    /// distinguishes `m{a}` from a jump, `line` the `'{a}` (first non-blank of
    /// the line) form from `` `{a} `` (exact position).
    Mark { set: bool, line: bool },
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
    pub tab_width: usize,
    /// The last `f`/`F`/`t`/`T` motion, with its target char, for `;`/`,` to
    /// repeat. Survives `reset()` (buffer switches) like the settings do —
    /// vim's `;` reaches across windows too.
    last_find: Option<Motion>,
    /// Read-only reading posture (`:view`): `on_key` drops buffer-mutating
    /// actions and makes insert entry a dead end, leaving navigation, yanks,
    /// search, `:` commands, and task toggling live. A posture, not a mode —
    /// it survives `reset()` (buffer switches) like config does.
    pub view: bool,
}

impl Vim {
    pub fn new(tab_width: usize) -> Self {
        Self {
            mode: Mode::Normal,
            count: None,
            pending: Pending::None,
            command: String::new(),
            prompt: ':',
            tab_width,
            last_find: None,
            view: false,
        }
    }

    /// Reset transient editing state on a buffer switch, keeping config
    /// (tab width).
    /// Mid-sequence (operator/`g`/`z`/till pending): the next key belongs to
    /// this grammar, so the keymap layer must not start a binding match on it
    /// (vim semantics: mappings apply at command start only — the `f` of `gf`
    /// is never the lead of an `f f` binding).
    pub fn in_sequence(&self) -> bool {
        !matches!(self.pending, Pending::None)
    }

    pub fn reset(&mut self) {
        self.mode = Mode::Normal;
        self.count = None;
        self.pending = Pending::None;
        self.command.clear();
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
        let mut actions = match self.mode {
            Mode::Insert => self.insert_key(ks),
            Mode::Normal => self.normal_key(ks),
            Mode::Command => self.command_key(ks),
            Mode::Visual => self.visual_key(ks, false),
            Mode::VisualLine => self.visual_key(ks, true),
        };
        // View filter — the one choke point every mode's commands exit
        // through. A command that would enter insert is a dead end (its setup
        // motions drop with the rest). `mutates()` is maintained for undo
        // checkpointing, so it already names every buffer-editing action;
        // `Undo`/`Repeat` edit without being in it (undo applies history,
        // repeat expands in the editor), so they're named here. `ToggleTask`
        // passing through is deliberate: checking a box isn't editing.
        if self.view {
            if self.mode == Mode::Insert {
                self.mode = Mode::Normal;
                return vec![];
            }
            actions.retain(|a| !a.mutates() && !matches!(a, Action::Undo | Action::Repeat));
        }
        actions
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

        // The viewport scrolls and the jumplist walk are the only chord
        // commands; every other combo (Ctrl/Alt/Cmd) clears state and drops —
        // the editor intercepts the chords it cares about (Ctrl-S/N/P) before
        // the grammar, and without the drop `Ctrl-a` would fall through and
        // trigger `a`.
        if m.control || m.alt || m.platform {
            self.pending = Pending::None;
            let acts = if m.control && !m.alt && !m.platform {
                match key {
                    "d" => vec![Action::ScrollHalf { down: true }],
                    "u" => vec![Action::ScrollHalf { down: false }],
                    "e" => vec![Action::ScrollLines { down: true, count: self.take_count() }],
                    "y" => vec![Action::ScrollLines { down: false, count: self.take_count() }],
                    "o" => vec![Action::JumpBack],
                    // Only `i`. Terminals fold Ctrl-I into Ctrl-Tab; claiming
                    // `tab` here would squat on the chord that wants to mean
                    // cycle-tabs (`buffer-next`).
                    "i" => vec![Action::JumpForward],
                    _ => vec![],
                }
            } else {
                vec![]
            };
            self.count = None;
            return acts;
        }

        // A pending literal-char state (`f`/`t`'s target, `r`'s replacement)
        // consumes this key before count parsing, so `dt3` reads the `3` as the
        // target rather than as a count.
        if let Some(actions) = self.resolve_literal(ks) {
            return actions;
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
            Pending::Object { op, around } => {
                return self.apply_object(op, around, key, shift)
            }
            Pending::GPrefix => return self.complete_g_prefix(key, shift),
            Pending::ZPrefix => return self.complete_z_prefix(key),
            Pending::Indent { dedent } => return self.complete_indent(dedent, key),
            // Resolved above, before count parsing.
            Pending::Find { .. } | Pending::Replace { .. } | Pending::Mark { .. }
            | Pending::None => {}
        }

        // Motions come from one table shared with visual and operator-pending;
        // letters arrive lowercased with `shift` separate, so capitals match as
        // (key, true).
        if let Some(spec) = motion(key, shift) {
            return self.move_action(spec.motion);
        }
        // `f`/`F`/`t`/`T`: the next key is the target char.
        if let Some(make) = find_ctor(key, shift) {
            self.pending = Pending::Find { make, op: None, count: self.take_count() };
            return vec![];
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
            (";", _) | (",", _) => {
                let n = self.take_count();
                self.repeat_find(key == ",", None, n)
            }
            ("x", false) => vec![Action::DeleteCharUnder(self.take_count())],
            // `X`: delete the char before the caret — `dh`, which already
            // stops at the line start rather than joining lines.
            ("x", true) => vec![Action::DeleteMotion(Motion::CharLeft, self.take_count())],
            // `r`: the next key is the replacement char.
            ("r", false) => {
                self.pending = Pending::Replace { count: self.take_count() };
                vec![]
            }
            // `m`/`'`/`` ` ``: the next key names the mark. Both quote keys are
            // otherwise only consumed after `i`/`a` (`i'`, `` i` ``), never bare.
            ("m", false) => {
                self.pending = Pending::Mark { set: true, line: false };
                vec![]
            }
            ("'", _) => {
                self.pending = Pending::Mark { set: false, line: true };
                vec![]
            }
            ("`", _) => {
                self.pending = Pending::Mark { set: false, line: false };
                vec![]
            }
            ("~", _) => vec![Action::ToggleCase(self.take_count())],
            // `J`: join lines. The count names lines, so `J` and `2J` both make
            // one join (`gJ`, the verbatim splice, is a `g`-sequence).
            ("j", true) => vec![Action::JoinLines { count: self.take_count(), space: true }],
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
            (">", _) | ("<", _) => {
                self.pending = Pending::Indent { dedent: key == "<" };
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
            // ponytail: count (`3.`) ignored — repeat once.
            (".", _) => {
                self.count = None;
                vec![Action::Repeat]
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
            // `D`: delete to end of line. Same as `d$`.
            // ponytail: count (`2D`) ignored — extend when multi-line D matters.
            ("d", true) => {
                self.count = None;
                vec![Action::DeleteMotion(Motion::LineEnd, 1)]
            }
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
            // Enter: toggle the task box on the caret's line (no-op off one).
            ("enter", _) => {
                self.count = None;
                vec![Action::ToggleTask]
            }
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
            // Mid-sequence (`vf`, `vi`), escape aborts just that sequence and
            // the selection stays — only a bare escape leaves visual mode.
            if self.in_sequence() {
                self.pending = Pending::None;
                return vec![];
            }
            self.mode = Mode::Normal;
            return vec![Action::CollapseSelection];
        }
        if m.control || m.alt || m.platform {
            self.count = None;
            return vec![];
        }
        // A pending `f`/`t` target consumes this key before count parsing, same
        // as in normal mode.
        if let Some(actions) = self.resolve_literal(ks) {
            return actions;
        }
        if let Pending::Object { op, around } = self.pending {
            self.pending = Pending::None;
            return self.apply_object(op, around, key, shift);
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
            ("d", false) | ("x", false) => {
                self.count = None;
                self.mode = Mode::Normal;
                return vec![Action::DeleteSelection { linewise: line, change: false }];
            }
            // `X` deletes the selected lines whatever the submode (vim).
            ("x", true) => {
                self.count = None;
                self.mode = Mode::Normal;
                return vec![Action::DeleteSelection { linewise: true, change: false }];
            }
            // `c`: delete the selection and enter insert. A linewise span keeps
            // one empty line to type into (vim `Vc`).
            ("c", false) => {
                self.count = None;
                return self
                    .enter_insert(vec![Action::DeleteSelection { linewise: line, change: true }]);
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
            // `i`/`a` start a text object (`viw`, `va(`), which replaces the
            // selection rather than extending it.
            ("i", false) | ("a", false) => {
                self.count = None;
                self.pending = Pending::Object { op: None, around: key == "a" };
                return vec![];
            }
            (";", _) | (",", _) => {
                let n = self.take_count();
                return self.repeat_find(key == ",", None, n);
            }
            _ => {}
        }

        if let Some(spec) = motion(key, shift) {
            return self.move_action(spec.motion);
        }
        if let Some(make) = find_ctor(key, shift) {
            self.pending = Pending::Find { make, op: None, count: self.take_count() };
            return vec![];
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

        // Plain printable input (jk-style exit sequences are the keymap
        // layer's job, resolved in the editor before keys reach the grammar).
        // Tab/Enter are excluded even though gpui's macOS backend populates
        // `key_char` for them (`\t`/`\n`) — its Linux backend doesn't, since
        // both are control characters there. Matching on `key` instead of
        // `key_char` keeps their smart-indent/newline handling below
        // platform-independent instead of degrading to a literal char on Mac.
        if !m.control && !m.platform && !m.alt && ks.key != "tab" && ks.key != "enter" {
            if let Some(s) = &ks.key_char {
                return vec![Action::InsertText(s.clone())];
            }
        }

        match ks.key.as_str() {
            "escape" => {
                self.mode = Mode::Normal;
                vec![Action::Move(Motion::CharLeft, 1)] // vim nudges left on exit
            }
            "left" => vec![Action::Move(Motion::CharLeft, 1)],
            "right" => vec![Action::Move(Motion::CharRight, 1)],
            "up" => vec![Action::Move(Motion::LineUp, 1)],
            "down" => vec![Action::Move(Motion::LineDown, 1)],
            "backspace" => vec![Action::DeleteBackward],
            "delete" => vec![Action::DeleteForward],
            "enter" => vec![Action::Newline { clear_empty: true }],
            // A markdown buffer has no literal tabs. On a list line the editor
            // shifts the whole line; elsewhere Tab inserts `tab_width` spaces.
            // Shift-Tab dedents.
            "tab" if !m.shift => vec![Action::Tab { width: self.tab_width, dedent: false }],
            "tab" if m.shift => vec![Action::Tab { width: self.tab_width, dedent: true }],
            _ => vec![],
        }
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
        // `f`/`F`/`t`/`T` need one more key (their target char); park the
        // operator + count until it arrives.
        if let Some(make) = find_ctor(key, shift) {
            self.pending = Pending::Find { make, op: Some(op), count };
            return vec![];
        }
        // `;`/`,` repeat the last find as this operator's target (`d;`).
        if key == ";" || key == "," {
            return self.repeat_find(key == ",", Some(op), count);
        }
        // `i`/`a` start a text object (`diw`/`dap`); the next key names it.
        // ponytail: the count (`d2iw`) is dropped — objects act once.
        if !shift && (key == "i" || key == "a") {
            self.pending = Pending::Object { op: Some(op), around: key == "a" };
            return vec![];
        }
        match motion(key, shift) {
            Some(spec) if spec.op_target => match op {
                Op::Delete => vec![Action::DeleteMotion(spec.motion, count)],
                Op::Yank => vec![Action::YankMotion(spec.motion, count)],
                Op::Change => {
                    // `cw`/`cW` swap in the ChangeWord motions (vim special
                    // case: don't take the space/newline after the word).
                    let m = match spec.motion {
                        Motion::WordForward => Motion::ChangeWord,
                        Motion::BigWordForward => Motion::ChangeBigWord,
                        m => m,
                    };
                    self.enter_insert(vec![Action::DeleteMotion(m, count)])
                }
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

    /// Resolve a pending text object against the key naming it; anything else
    /// aborts. Bracket objects take either delimiter plus vim's `b`/`B`
    /// aliases — `B` arrives as `b` with shift, unlike `{`/`}`, which the
    /// backend already reports as the shifted symbol. With no operator (visual
    /// mode) the object becomes the selection instead.
    fn apply_object(&mut self, op: Option<Op>, around: bool, key: &str, shift: bool) -> Vec<Action> {
        let block = |open, close| TextObject::Block { open, close, around };
        let obj = match (key, shift) {
            ("w", _) => TextObject::Word { around },
            ("p", _) => TextObject::Paragraph { around },
            ("\"", _) | ("'", _) | ("`", _) => {
                TextObject::Quote { ch: key.chars().next().unwrap(), around }
            }
            ("(", _) | (")", _) | ("b", false) => block('(', ')'),
            ("{", _) | ("}", _) | ("b", true) => block('{', '}'),
            ("[", _) | ("]", _) => block('[', ']'),
            ("<", _) | (">", _) => block('<', '>'),
            _ => return vec![],
        };
        match op {
            None => vec![Action::SelectObject(obj)],
            Some(Op::Delete) => vec![Action::DeleteObject { obj, change: false }],
            Some(Op::Yank) => vec![Action::YankObject(obj)],
            Some(Op::Change) => {
                self.enter_insert(vec![Action::DeleteObject { obj, change: true }])
            }
        }
    }

    /// Resolve a pending state whose next key is a literal character — the
    /// target of `f`/`F`/`t`/`T`, the replacement of `r`, or the name of a mark
    /// (`ma`, `` `a ``). `None` means no such state is pending and the caller
    /// handles the key itself. A key that produces no char (arrows, Enter,
    /// Escape) aborts.
    fn resolve_literal(&mut self, ks: &Keystroke) -> Option<Vec<Action>> {
        let ch = ks.key_char.as_ref().and_then(|s| s.chars().next());
        match std::mem::replace(&mut self.pending, Pending::None) {
            Pending::Replace { count } => {
                Some(ch.map_or(vec![], |c| vec![Action::ReplaceChar(c, count)]))
            }
            Pending::Find { make, op, count } => Some(match ch {
                Some(c) => {
                    let m = make(c);
                    self.last_find = Some(m);
                    self.find_actions(m, op, count)
                }
                None => vec![],
            }),
            Pending::Mark { set, line } => Some(match ch {
                Some(c) if set => vec![Action::SetMark(c)],
                Some(c) => vec![Action::JumpToMark { name: c, line }],
                None => vec![],
            }),
            other => {
                self.pending = other;
                None
            }
        }
    }

    /// `;`/`,`: re-run the last `f`/`F`/`t`/`T`, `reverse` (`,`) flipping its
    /// direction. Neither key updates the stored motion, so a chain of `;`
    /// keeps going the same way.
    ///
    /// ponytail: `;` after `t{char}` doesn't skip a match sitting immediately
    /// ahead, so it can stall one char short (vim's `cpo-;` behavior). Needs
    /// the repeat to be distinguishable from a first press at the geometry
    /// layer — a flag on the till motions — if the stall annoys.
    fn repeat_find(&mut self, reverse: bool, op: Option<Op>, count: usize) -> Vec<Action> {
        match self.last_find {
            Some(m) => {
                let m = if reverse { flip_find(m) } else { m };
                self.find_actions(m, op, count)
            }
            None => vec![],
        }
    }

    /// Emit the command for a resolved find motion: an operator sweeps to it,
    /// otherwise it's a plain move — which visual mode turns into an extend,
    /// since the editor routes `Move` by the current mode.
    fn find_actions(&mut self, m: Motion, op: Option<Op>, count: usize) -> Vec<Action> {
        match op {
            None => vec![Action::Move(m, count)],
            Some(Op::Delete) => vec![Action::DeleteMotion(m, count)],
            Some(Op::Yank) => vec![Action::YankMotion(m, count)],
            Some(Op::Change) => self.enter_insert(vec![Action::DeleteMotion(m, count)]),
        }
    }

    /// A table motion's `Move`, consuming any pending count. `G` and `gg` with
    /// a count are goto-line instead of a file edge (vim `5G`); the count rides
    /// in the motion so a bare `G` stays distinct from `1G`.
    fn move_action(&mut self, m: Motion) -> Vec<Action> {
        if let (Motion::FileEnd | Motion::FileStart, Some(n)) = (m, self.count) {
            self.count = None;
            return vec![Action::Move(Motion::GotoLine(n), 1)];
        }
        vec![Action::Move(m, self.take_count())]
    }

    /// Complete a `g`-sequence: `gg` jumps to file start (or to `{count}gg`'s
    /// line), `gt`/`gT` cycle buffers, `gd`/`gf`/`gx` follow the link under the
    /// caret, `gj`/`gk` move by visual row, `gJ` joins lines without a
    /// separator; anything else aborts.
    fn complete_g_prefix(&mut self, key: &str, shift: bool) -> Vec<Action> {
        // `gg` and `gJ` read the count; the rest ignore it.
        // ponytail: `2gt`/`3gj` (counts) ignored.
        match (key, shift) {
            ("g", _) => self.move_action(Motion::FileStart),
            ("j", true) => vec![Action::JoinLines { count: self.take_count(), space: false }],
            _ => {
                self.count = None;
                match (key, shift) {
                    ("t", false) => vec![Action::BufferNext],
                    ("t", true) => vec![Action::BufferPrev],
                    ("d" | "f" | "x", false) => vec![Action::FollowLink],
                    ("j", false) => vec![Action::MoveDisplay { down: true }],
                    ("k", false) => vec![Action::MoveDisplay { down: false }],
                    _ => vec![],
                }
            }
        }
    }

    /// Complete `>`/`<`: the doubled key shifts `count` lines by one tab
    /// width; anything else aborts. Motion targets (`>j`, `>ap`) wait until
    /// they're missed.
    fn complete_indent(&mut self, dedent: bool, key: &str) -> Vec<Action> {
        let count = self.take_count();
        if key == if dedent { "<" } else { ">" } {
            vec![Action::IndentLines { width: self.tab_width, dedent, count }]
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
/// here. `j`/`k`/`e`/`G`/`%` are non-targets (`dj` is linewise; `de`/`dG` are
/// just a flag flip away; `d%`'s span is inclusive at both ends, which the
/// `[min, max)` sweep can't express). `G` (`shift+g`) is file-end; lowercase
/// `g` is a prefix. Capitals must match as `(key, true)` explicitly — a `_`
/// shift pattern would swallow the shifted command on that key (`J` for `j`,
/// `W` for `w`).
struct MotionSpec {
    motion: Motion,
    op_target: bool,
}

/// The motion constructor `f`/`F`/`t`/`T` parks while waiting for its target
/// char. Tuple-variant constructors double as `fn(char) -> Motion`, so the four
/// keys differ only by which one they store.
fn find_ctor(key: &str, shift: bool) -> Option<fn(char) -> Motion> {
    Some(match (key, shift) {
        ("f", false) => Motion::FindChar,
        ("f", true) => Motion::FindCharBack,
        ("t", false) => Motion::TillChar,
        ("t", true) => Motion::TillCharBack,
        _ => return None,
    })
}

/// Reverse a find motion's direction, for `,`.
fn flip_find(m: Motion) -> Motion {
    match m {
        Motion::FindChar(c) => Motion::FindCharBack(c),
        Motion::FindCharBack(c) => Motion::FindChar(c),
        Motion::TillChar(c) => Motion::TillCharBack(c),
        Motion::TillCharBack(c) => Motion::TillChar(c),
        m => m,
    }
}

fn motion(key: &str, shift: bool) -> Option<MotionSpec> {
    let (motion, op_target) = match (key, shift) {
        ("h", _) | ("left", _) => (Motion::CharLeft, true),
        ("l", _) | ("right", _) => (Motion::CharRight, true),
        ("k", false) | ("up", _) => (Motion::LineUp, false),
        ("j", false) | ("down", _) => (Motion::LineDown, false),
        ("w", false) => (Motion::WordForward, true),
        ("w", true) => (Motion::BigWordForward, true),
        ("b", false) => (Motion::WordBackward, true),
        ("b", true) => (Motion::BigWordBackward, true),
        ("e", false) => (Motion::WordEnd, false),
        ("e", true) => (Motion::BigWordEnd, false),
        ("0", _) => (Motion::LineStart, true),
        ("^", _) => (Motion::FirstNonBlank, true),
        ("$", _) => (Motion::LineEnd, true),
        ("%", _) => (Motion::MatchBracket, false),
        ("{", _) => (Motion::ParaBackward, true),
        ("}", _) => (Motion::ParaForward, true),
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

    /// Default test grammar: 2-space tabs.
    fn vim() -> Vim {
        Vim::new(2)
    }

    #[test]
    fn normal_enter_emits_toggle_task() {
        let mut v = vim();
        assert_eq!(v.on_key(&named("enter")), vec![Action::ToggleTask]);
        // A pending count clears rather than leaking onto the next command.
        assert!(v.on_key(&k("3")).is_empty());
        assert_eq!(v.on_key(&named("enter")), vec![Action::ToggleTask]);
        assert_eq!(v.on_key(&k("j")), vec![Action::Move(Motion::LineDown, 1)]);
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
    fn capital_d_deletes_to_eol() {
        let mut v = vim();
        let d_shift = Keystroke {
            key: "d".into(),
            key_char: Some("D".into()),
            modifiers: Modifiers { shift: true, ..Default::default() },
        };
        assert_eq!(v.on_key(&d_shift), vec![Action::DeleteMotion(Motion::LineEnd, 1)]);
        assert_eq!(v.mode, Mode::Normal);
    }

    #[test]
    fn ct_changes_till_char() {
        let mut v = vim();
        v.on_key(&k("c"));
        assert!(v.on_key(&k("t")).is_empty()); // awaiting the target char
        assert_eq!(
            v.on_key(&k("x")),
            vec![Action::DeleteMotion(Motion::TillChar('x'), 1)]
        );
        assert_eq!(v.mode, Mode::Insert);

        // The target is literal — a digit after `t` is not a count.
        let mut v = vim();
        v.on_key(&k("d"));
        v.on_key(&k("t"));
        assert_eq!(
            v.on_key(&k("3")),
            vec![Action::DeleteMotion(Motion::TillChar('3'), 1)]
        );
        assert_eq!(v.mode, Mode::Normal);

        // `2ctx`: the count rides along.
        let mut v = vim();
        v.on_key(&k("2"));
        v.on_key(&k("c"));
        v.on_key(&k("t"));
        assert_eq!(
            v.on_key(&k("x")),
            vec![Action::DeleteMotion(Motion::TillChar('x'), 2)]
        );

        // A non-printing key aborts the operator.
        let mut v = vim();
        v.on_key(&k("c"));
        v.on_key(&k("t"));
        assert!(v.on_key(&named("escape")).is_empty());
        assert_eq!(v.mode, Mode::Normal);
        assert_eq!(v.on_key(&k("x")), vec![Action::DeleteCharUnder(1)]); // grammar clean
    }

    #[test]
    fn bare_find_motions() {
        // `fx` moves onto the char; the target key is literal, so a digit
        // after `f` is the target rather than a count.
        let mut v = vim();
        assert!(v.on_key(&k("f")).is_empty()); // awaiting the target
        assert_eq!(v.on_key(&k("3")), vec![Action::Move(Motion::FindChar('3'), 1)]);

        // A count before `f` rides through to the motion.
        v.on_key(&k("2"));
        v.on_key(&k("f"));
        assert_eq!(v.on_key(&k("x")), vec![Action::Move(Motion::FindChar('x'), 2)]);

        // Capitals go backward; `t`/`T` land beside the char.
        v.on_key(&shift("f", "F"));
        assert_eq!(v.on_key(&k("x")), vec![Action::Move(Motion::FindCharBack('x'), 1)]);
        v.on_key(&k("t"));
        assert_eq!(v.on_key(&k("x")), vec![Action::Move(Motion::TillChar('x'), 1)]);
        v.on_key(&shift("t", "T"));
        assert_eq!(v.on_key(&k("x")), vec![Action::Move(Motion::TillCharBack('x'), 1)]);

        // A non-printing key aborts, leaving the grammar clean.
        v.on_key(&k("f"));
        assert!(v.on_key(&named("escape")).is_empty());
        assert_eq!(v.on_key(&k("x")), vec![Action::DeleteCharUnder(1)]);
    }

    #[test]
    fn find_as_operator_target() {
        for (key, ctor) in [
            ("f", Motion::FindChar as fn(char) -> Motion),
            ("t", Motion::TillChar),
        ] {
            let mut v = vim();
            v.on_key(&k("d"));
            assert!(v.on_key(&k(key)).is_empty());
            assert_eq!(v.on_key(&k("z")), vec![Action::DeleteMotion(ctor('z'), 1)]);
        }
        // Backward, and with the change operator entering insert.
        let mut v = vim();
        v.on_key(&k("c"));
        v.on_key(&shift("f", "F"));
        assert_eq!(
            v.on_key(&k("z")),
            vec![Action::DeleteMotion(Motion::FindCharBack('z'), 1)]
        );
        assert_eq!(v.mode, Mode::Insert);
    }

    #[test]
    fn semicolon_and_comma_repeat_find() {
        let mut v = vim();
        // Nothing recorded yet → no-op.
        assert!(v.on_key(&k(";")).is_empty());

        v.on_key(&k("f"));
        v.on_key(&k("x"));
        assert_eq!(v.on_key(&k(";")), vec![Action::Move(Motion::FindChar('x'), 1)]);
        // `,` reverses without replacing the stored motion, so `;` still goes
        // the original way.
        assert_eq!(v.on_key(&k(",")), vec![Action::Move(Motion::FindCharBack('x'), 1)]);
        assert_eq!(v.on_key(&k(";")), vec![Action::Move(Motion::FindChar('x'), 1)]);
        // Counts apply, and the repeat works as an operator target (`d;`).
        v.on_key(&k("3"));
        assert_eq!(v.on_key(&k(";")), vec![Action::Move(Motion::FindChar('x'), 3)]);
        v.on_key(&k("d"));
        assert_eq!(v.on_key(&k(";")), vec![Action::DeleteMotion(Motion::FindChar('x'), 1)]);
    }

    #[test]
    fn shifted_keys_beat_their_motion_table_entry() {
        // `J`/`W`/`X` must not fall through to `j`/`w`/`x`'s table entry.
        let mut v = vim();
        assert_eq!(
            v.on_key(&shift("j", "J")),
            vec![Action::JoinLines { count: 1, space: true }]
        );
        assert_eq!(v.on_key(&shift("w", "W")), vec![Action::Move(Motion::BigWordForward, 1)]);
        assert_eq!(v.on_key(&shift("b", "B")), vec![Action::Move(Motion::BigWordBackward, 1)]);
        assert_eq!(v.on_key(&shift("e", "E")), vec![Action::Move(Motion::BigWordEnd, 1)]);
        assert_eq!(
            v.on_key(&shift("x", "X")),
            vec![Action::DeleteMotion(Motion::CharLeft, 1)]
        );
        // Lowercase still reaches the table.
        assert_eq!(v.on_key(&k("j")), vec![Action::Move(Motion::LineDown, 1)]);
        assert_eq!(v.on_key(&k("w")), vec![Action::Move(Motion::WordForward, 1)]);
    }

    #[test]
    fn gj_joins_without_a_separator() {
        let mut v = vim();
        v.on_key(&k("g"));
        assert_eq!(
            v.on_key(&shift("j", "J")),
            vec![Action::JoinLines { count: 1, space: false }]
        );
        // A count rides along; plain `gj` is still the display move.
        v.on_key(&k("3"));
        v.on_key(&k("g"));
        assert_eq!(
            v.on_key(&shift("j", "J")),
            vec![Action::JoinLines { count: 3, space: false }]
        );
        v.on_key(&k("g"));
        assert_eq!(v.on_key(&k("j")), vec![Action::MoveDisplay { down: true }]);
    }

    #[test]
    fn count_turns_file_edges_into_goto_line() {
        // Bare `G`/`gg` stay file edges; a count makes them goto-line, so `1G`
        // is distinguishable from `G`.
        let mut v = vim();
        assert_eq!(v.on_key(&shift("g", "G")), vec![Action::Move(Motion::FileEnd, 1)]);
        v.on_key(&k("1"));
        v.on_key(&k("2"));
        assert_eq!(v.on_key(&shift("g", "G")), vec![Action::Move(Motion::GotoLine(12), 1)]);
        v.on_key(&k("g"));
        assert_eq!(v.on_key(&k("g")), vec![Action::Move(Motion::FileStart, 1)]);
        v.on_key(&k("5"));
        v.on_key(&k("g"));
        assert_eq!(v.on_key(&k("g")), vec![Action::Move(Motion::GotoLine(5), 1)]);
    }

    #[test]
    fn replace_char_takes_a_literal_key() {
        let mut v = vim();
        assert!(v.on_key(&k("r")).is_empty()); // awaiting the replacement
        assert_eq!(v.on_key(&k("z")), vec![Action::ReplaceChar('z', 1)]);
        // The replacement is literal — a digit is not a count.
        v.on_key(&k("3"));
        v.on_key(&k("r")); // 3r
        assert_eq!(v.on_key(&k("4")), vec![Action::ReplaceChar('4', 3)]);
        // A non-printing key aborts.
        v.on_key(&k("r"));
        assert!(v.on_key(&named("escape")).is_empty());
        assert_eq!(v.on_key(&k("x")), vec![Action::DeleteCharUnder(1)]);
    }

    #[test]
    fn tilde_toggles_case() {
        let mut v = vim();
        assert_eq!(v.on_key(&k("~")), vec![Action::ToggleCase(1)]);
        v.on_key(&k("4"));
        assert_eq!(v.on_key(&k("~")), vec![Action::ToggleCase(4)]);
    }

    #[test]
    fn caret_and_percent_motions() {
        let mut v = vim();
        assert_eq!(v.on_key(&k("^")), vec![Action::Move(Motion::FirstNonBlank, 1)]);
        assert_eq!(v.on_key(&k("%")), vec![Action::Move(Motion::MatchBracket, 1)]);
        // `^` is an operator target; `%` isn't (its span is inclusive at both
        // ends, which the sweep can't express), so `d%` aborts.
        v.on_key(&k("d"));
        assert_eq!(
            v.on_key(&k("^")),
            vec![Action::DeleteMotion(Motion::FirstNonBlank, 1)]
        );
        v.on_key(&k("d"));
        assert!(v.on_key(&k("%")).is_empty());
    }

    #[test]
    fn visual_text_objects_replace_the_selection() {
        let mut v = vim();
        v.on_key(&k("v"));
        assert!(v.on_key(&k("i")).is_empty()); // awaiting the object key
        assert_eq!(
            v.on_key(&k("w")),
            vec![Action::SelectObject(TextObject::Word { around: false })]
        );
        assert_eq!(v.mode, Mode::Visual); // still selecting

        // Bracket objects and their aliases resolve the same in visual mode.
        v.on_key(&k("a"));
        assert_eq!(
            v.on_key(&k("(")),
            vec![Action::SelectObject(TextObject::Block {
                open: '(',
                close: ')',
                around: true
            })]
        );
        // Escape mid-sequence aborts the object, keeping the selection.
        v.on_key(&k("i"));
        assert!(v.on_key(&named("escape")).is_empty());
        assert_eq!(v.mode, Mode::Visual);
        // A bare escape then leaves visual.
        assert_eq!(v.on_key(&named("escape")), vec![Action::CollapseSelection]);
        assert_eq!(v.mode, Mode::Normal);
    }

    #[test]
    fn bracket_object_aliases() {
        // `b`/`B` alias the paren/brace pairs, and either delimiter works.
        for (key, sh, open, close) in [
            ("b", false, '(', ')'),
            (")", false, '(', ')'),
            ("b", true, '{', '}'),
            ("}", false, '{', '}'),
            ("]", false, '[', ']'),
            ("<", false, '<', '>'),
            ("\"", false, '"', '"'),
        ] {
            let mut v = vim();
            v.on_key(&k("d"));
            v.on_key(&k("i"));
            let ks = if sh { shift(key, "B") } else { k(key) };
            let obj = if open == '"' {
                TextObject::Quote { ch: '"', around: false }
            } else {
                TextObject::Block { open, close, around: false }
            };
            assert_eq!(
                v.on_key(&ks),
                vec![Action::DeleteObject { obj, change: false }],
                "object key {key:?} shift={sh}"
            );
        }
    }

    #[test]
    fn visual_change_enters_insert() {
        let mut v = vim();
        v.on_key(&k("v"));
        assert_eq!(
            v.on_key(&k("c")),
            vec![Action::DeleteSelection { linewise: false, change: true }]
        );
        assert_eq!(v.mode, Mode::Insert);

        // Linewise `Vc` spares a line to type into.
        let mut v = vim();
        v.on_key(&shift("v", "V"));
        assert_eq!(
            v.on_key(&k("c")),
            vec![Action::DeleteSelection { linewise: true, change: true }]
        );
        assert_eq!(v.mode, Mode::Insert);
    }

    #[test]
    fn visual_find_extends_the_selection() {
        // In visual mode a find is still a `Move`; the editor routes it to
        // `extend_motion` because the mode is visual.
        let mut v = vim();
        v.on_key(&k("v"));
        assert!(v.on_key(&k("f")).is_empty());
        assert_eq!(v.on_key(&k("z")), vec![Action::Move(Motion::FindChar('z'), 1)]);
        assert_eq!(v.mode, Mode::Visual);
        assert_eq!(v.on_key(&k(";")), vec![Action::Move(Motion::FindChar('z'), 1)]);
        // Shifted motions reach the table from visual mode too.
        assert_eq!(v.on_key(&shift("w", "W")), vec![Action::Move(Motion::BigWordForward, 1)]);
    }

    #[test]
    fn text_object_grammar() {
        // diw
        let mut v = vim();
        v.on_key(&k("d"));
        assert!(v.on_key(&k("i")).is_empty()); // awaiting the object key
        assert_eq!(
            v.on_key(&k("w")),
            vec![Action::DeleteObject { obj: TextObject::Word { around: false }, change: false }]
        );
        assert_eq!(v.mode, Mode::Normal);

        // caw → change enters insert
        let mut v = vim();
        v.on_key(&k("c"));
        v.on_key(&k("a"));
        assert_eq!(
            v.on_key(&k("w")),
            vec![Action::DeleteObject { obj: TextObject::Word { around: true }, change: true }]
        );
        assert_eq!(v.mode, Mode::Insert);

        // yap
        let mut v = vim();
        v.on_key(&k("y"));
        v.on_key(&k("a"));
        assert_eq!(
            v.on_key(&k("p")),
            vec![Action::YankObject(TextObject::Paragraph { around: true })]
        );

        // An unknown object key aborts, leaving the grammar clean.
        let mut v = vim();
        v.on_key(&k("d"));
        v.on_key(&k("i"));
        assert!(v.on_key(&k("q")).is_empty());
        assert_eq!(v.on_key(&k("x")), vec![Action::DeleteCharUnder(1)]);
    }

    #[test]
    fn gg_moves_to_file_start() {
        let mut v = vim();
        assert!(v.on_key(&k("g")).is_empty()); // prefix armed, no action yet
        assert_eq!(v.on_key(&k("g")), vec![Action::Move(Motion::FileStart, 1)]);
    }

    #[test]
    fn gt_cycles_buffers() {
        let mut v = vim();
        assert_eq!(v.on_key(&k("g")), vec![]); // prefix armed
        assert_eq!(v.on_key(&k("t")), vec![Action::BufferNext]);

        // `gT` (shift) goes the other way — capitals arrive lowercase + shift.
        let g_t = Keystroke {
            key: "t".into(),
            key_char: Some("T".into()),
            modifiers: Modifiers { shift: true, ..Default::default() },
        };
        v.on_key(&k("g"));
        assert_eq!(v.on_key(&g_t), vec![Action::BufferPrev]);
    }

    #[test]
    fn g_then_other_key_aborts() {
        let mut v = vim();
        v.on_key(&k("g"));
        assert!(v.on_key(&k("q")).is_empty()); // `gq` unbound → no-op, not delete
        assert_eq!(v.mode, Mode::Normal);
    }

    #[test]
    fn g_follow_keys_emit_follow_link() {
        for key in ["d", "f", "x"] {
            let mut v = vim();
            assert!(v.on_key(&k("g")).is_empty()); // prefix armed
            assert_eq!(v.on_key(&k(key)), vec![Action::FollowLink]);
        }
    }

    #[test]
    fn in_sequence_tracks_pending() {
        let mut v = vim();
        assert!(!v.in_sequence());
        v.on_key(&k("g"));
        assert!(v.in_sequence()); // next key belongs to the grammar
        v.on_key(&k("f"));
        assert!(!v.in_sequence()); // `gf` resolved — the `f` never armed a find
        v.on_key(&k("2")); // a bare count is not a sequence
        assert!(!v.in_sequence());
    }

    #[test]
    fn literal_targets_are_hidden_from_the_keymap_layer() {
        // A find's target and `r`'s replacement belong to the grammar, so the
        // user-keymap resolver must not start a binding match on them — that's
        // what `in_sequence` gates. Without it, `f,` would fire a `,` leader.
        let mut v = vim();
        v.on_key(&k("f"));
        assert!(v.in_sequence());
        v.on_key(&k(","));
        assert!(!v.in_sequence());
        v.on_key(&k("r"));
        assert!(v.in_sequence());
        v.on_key(&k(","));
        assert!(!v.in_sequence());
        // The `,` was consumed as data, not as a repeat-find.
        assert_eq!(v.on_key(&k(";")), vec![Action::Move(Motion::FindChar(','), 1)]);
    }

    #[test]
    fn visual_capital_x_deletes_lines() {
        let mut v = vim();
        v.on_key(&k("v")); // charwise, but `X` is linewise regardless
        assert_eq!(
            v.on_key(&shift("x", "X")),
            vec![Action::DeleteSelection { linewise: true, change: false }]
        );
        assert_eq!(v.mode, Mode::Normal);
    }

    #[test]
    fn cw_changes_word() {
        let mut v = vim();
        assert!(v.on_key(&k("c")).is_empty());
        assert_eq!(
            v.on_key(&k("w")),
            vec![Action::DeleteMotion(Motion::ChangeWord, 1)]
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
    fn dot_emits_repeat() {
        let mut v = vim();
        assert_eq!(v.on_key(&k(".")), vec![Action::Repeat]);
        // A pending count is dropped, not leaked onto the next command.
        v.on_key(&k("3"));
        assert_eq!(v.on_key(&k(".")), vec![Action::Repeat]);
        assert_eq!(v.on_key(&k("j")), vec![Action::Move(Motion::LineDown, 1)]);
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
    fn tab_and_enter_emit_actions_even_with_key_char_set() {
        // gpui's macOS backend populates `key_char` ("\t"/"\n") for Tab/Enter,
        // unlike its Linux backend — insert_key must match on `key`, not take
        // the key_char fast path, or these degrade to a literal-char insert.
        let mut v = vim();
        v.on_key(&k("i"));
        let mac_tab = Keystroke {
            key: "tab".into(),
            key_char: Some("\t".into()),
            modifiers: Modifiers::default(),
        };
        assert_eq!(v.on_key(&mac_tab), vec![Action::Tab { width: 2, dedent: false }]);
        let mac_enter = Keystroke {
            key: "enter".into(),
            key_char: Some("\n".into()),
            modifiers: Modifiers::default(),
        };
        assert_eq!(v.on_key(&mac_enter), vec![Action::Newline { clear_empty: true }]);
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
            vec![Action::DeleteSelection { linewise: true, change: false }]
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
    fn double_indent_shifts_lines() {
        // `>>` indents one line by tab_width; the count names lines (`3<<`).
        let mut v = vim();
        assert!(v.on_key(&shift(">", ">")).is_empty()); // awaiting the double
        assert_eq!(
            v.on_key(&shift(">", ">")),
            vec![Action::IndentLines { width: 2, dedent: false, count: 1 }]
        );

        v.on_key(&k("3"));
        v.on_key(&shift("<", "<"));
        assert_eq!(
            v.on_key(&shift("<", "<")),
            vec![Action::IndentLines { width: 2, dedent: true, count: 3 }]
        );

        // A mismatched second key aborts, dropping the count with it.
        let mut v = vim();
        v.on_key(&k("3"));
        v.on_key(&shift(">", ">"));
        assert!(v.on_key(&shift("<", "<")).is_empty());
        assert_eq!(v.on_key(&k("j")), vec![Action::Move(Motion::LineDown, 1)]);
    }

    #[test]
    fn gj_gk_move_by_display_row() {
        let mut v = vim();
        v.on_key(&k("g"));
        assert_eq!(v.on_key(&k("j")), vec![Action::MoveDisplay { down: true }]);
        v.on_key(&k("g"));
        assert_eq!(v.on_key(&k("k")), vec![Action::MoveDisplay { down: false }]);
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
    fn ctrl_scroll_commands() {
        let mut v = vim();
        let ctrl = |key: &str| Keystroke {
            key: key.into(),
            key_char: Some(key.into()),
            modifiers: Modifiers { control: true, ..Default::default() },
        };
        assert_eq!(v.on_key(&ctrl("d")), vec![Action::ScrollHalf { down: true }]);
        assert_eq!(v.on_key(&ctrl("u")), vec![Action::ScrollHalf { down: false }]);
        assert_eq!(v.on_key(&ctrl("e")), vec![Action::ScrollLines { down: true, count: 1 }]);
        // A count multiplies the line scroll.
        v.on_key(&k("3"));
        assert_eq!(v.on_key(&ctrl("y")), vec![Action::ScrollLines { down: false, count: 3 }]);
    }

    #[test]
    fn ctrl_o_and_i_walk_the_jumplist() {
        let mut v = vim();
        let ctrl = |key: &str| Keystroke {
            key: key.into(),
            key_char: Some(key.into()),
            modifiers: Modifiers { control: true, ..Default::default() },
        };
        assert_eq!(v.on_key(&ctrl("o")), vec![Action::JumpBack]);
        assert_eq!(v.on_key(&ctrl("i")), vec![Action::JumpForward]);
        // Ctrl-Tab is left free for a cycle-tabs binding.
        assert_eq!(v.on_key(&ctrl("tab")), vec![]);
    }

    #[test]
    fn only_jump_motions_record() {
        assert!(Action::Move(Motion::FileEnd, 1).is_jump());
        assert!(Action::Move(Motion::GotoLine(12), 1).is_jump()); // `3G`
        assert!(Action::SearchNext { reverse: false, count: 1 }.is_jump());
        assert!(!Action::Move(Motion::WordForward, 1).is_jump());
        assert!(!Action::ScrollHalf { down: true }.is_jump());
        assert!(!Action::JumpBack.is_jump()); // walking is not jumping
        assert!(!Action::JumpToMark { name: 'a', line: false }.is_jump()); // records itself
    }

    #[test]
    fn mark_set_and_jump() {
        let mut v = vim();
        assert_eq!(v.on_key(&k("m")), vec![]); // awaiting the name
        assert_eq!(v.on_key(&k("a")), vec![Action::SetMark('a')]);
        assert_eq!(v.on_key(&k("`")), vec![]);
        assert_eq!(v.on_key(&k("a")), vec![Action::JumpToMark { name: 'a', line: false }]);
        assert_eq!(v.on_key(&k("'")), vec![]);
        assert_eq!(v.on_key(&k("a")), vec![Action::JumpToMark { name: 'a', line: true }]);
        // `''` — the position before the latest jump.
        v.on_key(&k("'"));
        assert_eq!(v.on_key(&k("'")), vec![Action::JumpToMark { name: '\'', line: true }]);
        // A digit after `m` is the mark name, not a count.
        v.on_key(&k("m"));
        assert_eq!(v.on_key(&k("2")), vec![Action::SetMark('2')]);
        // Escape abandons the pending name.
        v.on_key(&k("m"));
        assert_eq!(v.on_key(&named("escape")), vec![]);
        assert_eq!(v.on_key(&k("j")), vec![Action::Move(Motion::LineDown, 1)]);
        // Quote text objects still work — `'` is only a mark key when bare.
        v.on_key(&k("d"));
        v.on_key(&k("i"));
        assert_eq!(
            v.on_key(&k("'")),
            vec![Action::DeleteObject {
                obj: TextObject::Quote { ch: '\'', around: false },
                change: false
            }]
        );
    }

    #[test]
    fn view_drops_mutations_keeps_navigation() {
        let mut v = vim();
        v.view = true;
        // Navigation, task toggling, and scrolling stay live.
        assert_eq!(v.on_key(&k("j")), vec![Action::Move(Motion::LineDown, 1)]);
        assert_eq!(v.on_key(&named("enter")), vec![Action::ToggleTask]);
        // Yanks stay live (copying while reading).
        v.on_key(&k("y"));
        assert_eq!(v.on_key(&k("y")), vec![Action::YankLines(1)]);
        // Edits drop: x, p, u, dd.
        assert!(v.on_key(&k("x")).is_empty());
        assert!(v.on_key(&k("p")).is_empty());
        assert!(v.on_key(&k("u")).is_empty());
        v.on_key(&k("d"));
        assert!(v.on_key(&k("d")).is_empty());
        // Insert entry is a dead end: `o` emits nothing, mode stays normal.
        assert!(v.on_key(&k("o")).is_empty());
        assert_eq!(v.mode, Mode::Normal);
        // The `:` prompt still works (it's how `:view` exits).
        v.on_key(&k(":"));
        assert_eq!(v.mode, Mode::Command);
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
    fn insert_mode_types_printables() {
        let mut v = vim();
        v.on_key(&k("i"));
        assert_eq!(v.on_key(&k("j")), vec![Action::InsertText("j".into())]);
    }
}
