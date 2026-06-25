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
    DeleteCharUnder(usize),
    YankMotion(Motion, usize),
    YankLines(usize),
    /// `p` (after) / `P` (before).
    Paste { after: bool },
    /// Visual-mode `d`/`x`/`y` over the current selection.
    DeleteSelection { linewise: bool },
    YankSelection { linewise: bool },
    /// Collapse the selection back to a caret (leaving visual mode).
    CollapseSelection,
    InsertText(String),
    DeleteBackward,
    DeleteForward,
    Undo,
    /// A submitted `:` command line (without the leading colon). The editor,
    /// not the grammar, decides what `w`/`q`/… mean.
    ExecuteCommand(String),
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
                | Action::DeleteCharUnder(..)
                | Action::DeleteSelection { .. }
                | Action::Paste { .. }
                | Action::InsertText(..)
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
enum Operator {
    Delete,
    Yank,
}

/// The vim grammar: a mode-aware state machine that consumes keystrokes — some
/// of which (counts, an operator) build up pending state — and emits zero or
/// more `Action`s once a complete command is recognized.
pub struct Vim {
    pub mode: Mode,
    count: Option<usize>,
    operator: Option<Operator>,
    /// The `:` command line being typed, valid only in `Mode::Command`.
    command: String,
}

impl Vim {
    pub fn new() -> Self {
        Self {
            mode: Mode::Normal,
            count: None,
            operator: None,
            command: String::new(),
        }
    }

    /// The text typed after `:` so far (for rendering the command line).
    pub fn command_line(&self) -> &str {
        &self.command
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
            self.operator = None;
            return vec![];
        }

        // Chord combos (Ctrl/Alt/Cmd) have no normal-mode vim command yet, and
        // the editor intercepts the ones it cares about (Ctrl-S/N/P) first.
        // Without this, `Ctrl-a` would fall through and trigger `a`.
        if m.control || m.alt || m.platform {
            self.count = None;
            self.operator = None;
            return vec![];
        }

        // Count digits (no chord modifier). '0' counts only mid-count; with no
        // count pending it's the line-start motion handled below.
        if !m.control && !m.alt && !m.platform && key.len() == 1 {
            if let Some(d) = key.chars().next().unwrap().to_digit(10) {
                let d = d as usize;
                if d != 0 || self.count.is_some() {
                    self.count = Some(self.count.unwrap_or(0) * 10 + d);
                    return vec![];
                }
            }
        }

        // A pending operator turns this keystroke into its motion (or `dd`).
        if let Some(op) = self.operator.take() {
            let count = self.count.take().unwrap_or(1);
            return operate(op, key, count);
        }

        // Letters arrive lowercased with `shift` separate, so capitals are
        // matched as (key, true).
        match (key, shift) {
            ("h", _) | ("left", _) => vec![Action::Move(Motion::CharLeft, self.take_count())],
            ("l", _) | ("right", _) => vec![Action::Move(Motion::CharRight, self.take_count())],
            ("k", _) | ("up", _) => vec![Action::Move(Motion::LineUp, self.take_count())],
            ("j", _) | ("down", _) => vec![Action::Move(Motion::LineDown, self.take_count())],
            ("w", _) => vec![Action::Move(Motion::WordForward, self.take_count())],
            ("b", _) => vec![Action::Move(Motion::WordBackward, self.take_count())],
            ("e", _) => vec![Action::Move(Motion::WordEnd, self.take_count())],
            ("0", _) => {
                self.count = None;
                vec![Action::Move(Motion::LineStart, 1)]
            }
            ("$", _) => {
                self.count = None;
                vec![Action::Move(Motion::LineEnd, 1)]
            }
            ("g", true) => {
                self.count = None;
                vec![Action::Move(Motion::FileEnd, 1)]
            }
            ("x", _) => vec![Action::DeleteCharUnder(self.take_count())],
            ("d", false) => {
                self.operator = Some(Operator::Delete);
                vec![]
            }
            ("y", false) => {
                self.operator = Some(Operator::Yank);
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
                Action::InsertText("\n".into()),
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
            (":", _) => {
                self.count = None;
                self.command.clear();
                self.mode = Mode::Command;
                vec![]
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
                vec![Action::ExecuteCommand(std::mem::take(&mut self.command))]
            }
            // Backspacing past the colon exits command mode.
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
            _ => {}
        }

        if let Some(motion) = motion_for_key(key, shift) {
            return vec![Action::Move(motion, self.take_count())];
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
            "enter" => vec![Action::InsertText("\n".into())],
            // Opinionated: a markdown buffer has no literal tabs — Tab inserts two
            // spaces. Shift-Tab is left unhandled, reserved for dedent.
            "tab" if !m.shift => vec![Action::InsertText("  ".into())],
            _ if !m.control && !m.platform && !m.alt => match &ks.key_char {
                Some(s) => vec![Action::InsertText(s.clone())],
                None => vec![],
            },
            _ => vec![],
        }
    }
}

fn operate(op: Operator, key: &str, count: usize) -> Vec<Action> {
    match op {
        Operator::Delete => {
            if key == "d" {
                vec![Action::DeleteLines(count)]
            } else if let Some(motion) = motion_for_op(key) {
                vec![Action::DeleteMotion(motion, count)]
            } else {
                vec![] // unsupported motion after `d` → abort the operator
            }
        }
        Operator::Yank => {
            if key == "y" {
                vec![Action::YankLines(count)]
            } else if let Some(motion) = motion_for_op(key) {
                vec![Action::YankMotion(motion, count)]
            } else {
                vec![] // unsupported motion after `y` → abort
            }
        }
    }
}

/// The cursor-movement keys shared by normal and visual mode. `G` (`shift+g`)
/// is file-end; lowercase `g` has no standalone motion yet.
fn motion_for_key(key: &str, shift: bool) -> Option<Motion> {
    Some(match (key, shift) {
        ("h", _) | ("left", _) => Motion::CharLeft,
        ("l", _) | ("right", _) => Motion::CharRight,
        ("k", _) | ("up", _) => Motion::LineUp,
        ("j", _) | ("down", _) => Motion::LineDown,
        ("w", _) => Motion::WordForward,
        ("b", _) => Motion::WordBackward,
        ("e", _) => Motion::WordEnd,
        ("0", _) => Motion::LineStart,
        ("$", _) => Motion::LineEnd,
        ("g", true) => Motion::FileEnd,
        _ => return None,
    })
}

/// Motions usable as an operator target. `j`/`k` are excluded — `dj` is
/// linewise in real vim and char-range deletion would be wrong.
fn motion_for_op(key: &str) -> Option<Motion> {
    Some(match key {
        "h" => Motion::CharLeft,
        "l" => Motion::CharRight,
        "w" => Motion::WordForward,
        "b" => Motion::WordBackward,
        "0" => Motion::LineStart,
        "$" => Motion::LineEnd,
        _ => return None,
    })
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

    #[test]
    fn count_then_motion() {
        let mut v = Vim::new();
        assert!(v.on_key(&k("3")).is_empty());
        assert_eq!(v.on_key(&k("j")), vec![Action::Move(Motion::LineDown, 3)]);
    }

    #[test]
    fn dd_deletes_lines() {
        let mut v = Vim::new();
        assert!(v.on_key(&k("d")).is_empty());
        assert_eq!(v.on_key(&k("d")), vec![Action::DeleteLines(1)]);
    }

    #[test]
    fn count_before_operator() {
        let mut v = Vim::new();
        v.on_key(&k("3"));
        v.on_key(&k("d"));
        assert_eq!(v.on_key(&k("d")), vec![Action::DeleteLines(3)]);
    }

    #[test]
    fn dw_deletes_word() {
        let mut v = Vim::new();
        v.on_key(&k("d"));
        assert_eq!(v.on_key(&k("w")), vec![Action::DeleteMotion(Motion::WordForward, 1)]);
    }

    #[test]
    fn capital_c_changes_to_eol() {
        let mut v = Vim::new();
        let c_shift = Keystroke {
            key: "c".into(),
            key_char: Some("C".into()),
            modifiers: Modifiers { shift: true, ..Default::default() },
        };
        assert_eq!(v.on_key(&c_shift), vec![Action::DeleteMotion(Motion::LineEnd, 1)]);
        assert_eq!(v.mode, Mode::Insert);
    }

    #[test]
    fn yy_yanks_lines() {
        let mut v = Vim::new();
        assert!(v.on_key(&k("y")).is_empty());
        assert_eq!(v.on_key(&k("y")), vec![Action::YankLines(1)]);
    }

    #[test]
    fn yw_yanks_word() {
        let mut v = Vim::new();
        v.on_key(&k("y"));
        assert_eq!(v.on_key(&k("w")), vec![Action::YankMotion(Motion::WordForward, 1)]);
    }

    #[test]
    fn p_and_capital_p_paste() {
        let mut v = Vim::new();
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
        let mut v = Vim::new();
        assert_eq!(v.on_key(&k("u")), vec![Action::Undo]);
    }

    #[test]
    fn enter_and_exit_insert() {
        let mut v = Vim::new();
        v.on_key(&k("i"));
        assert_eq!(v.mode, Mode::Insert);
        v.on_key(&named("escape"));
        assert_eq!(v.mode, Mode::Normal);
    }

    #[test]
    fn insert_emits_text() {
        let mut v = Vim::new();
        v.on_key(&k("i"));
        assert_eq!(v.on_key(&k("x")), vec![Action::InsertText("x".into())]);
    }

    #[test]
    fn tab_inserts_two_spaces() {
        let mut v = Vim::new();
        v.on_key(&k("i"));
        assert_eq!(v.on_key(&named("tab")), vec![Action::InsertText("  ".into())]);
    }

    #[test]
    fn normal_mode_letters_are_not_text() {
        // Pressing a printable in normal mode must never insert it.
        let mut v = Vim::new();
        assert!(v.on_key(&k("z")).is_empty());
    }

    #[test]
    fn ex_command_buffers_and_submits() {
        let mut v = Vim::new();
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
        let mut v = Vim::new();
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
        let mut v = Vim::new();
        v.on_key(&k("v"));
        assert_eq!(v.mode, Mode::Visual);
        let mut v = Vim::new();
        v.on_key(&shift("v", "V"));
        assert_eq!(v.mode, Mode::VisualLine);
    }

    #[test]
    fn visual_motion_extends_then_delete_returns_to_normal() {
        let mut v = Vim::new();
        v.on_key(&shift("v", "V"));
        assert_eq!(v.on_key(&k("j")), vec![Action::Move(Motion::LineDown, 1)]);
        assert_eq!(
            v.on_key(&k("d")),
            vec![Action::DeleteSelection { linewise: true }]
        );
        assert_eq!(v.mode, Mode::Normal);
    }

    #[test]
    fn visual_count_then_motion() {
        let mut v = Vim::new();
        v.on_key(&k("v"));
        v.on_key(&k("3"));
        assert_eq!(v.on_key(&k("j")), vec![Action::Move(Motion::LineDown, 3)]);
    }

    #[test]
    fn visual_escape_collapses() {
        let mut v = Vim::new();
        v.on_key(&k("v"));
        assert_eq!(v.on_key(&named("escape")), vec![Action::CollapseSelection]);
        assert_eq!(v.mode, Mode::Normal);
    }

    #[test]
    fn v_toggles_off_capital_v_switches_submode() {
        let mut v = Vim::new();
        v.on_key(&k("v"));
        v.on_key(&k("v")); // same key → leave visual
        assert_eq!(v.mode, Mode::Normal);

        let mut v = Vim::new();
        v.on_key(&k("v"));
        v.on_key(&shift("v", "V")); // other key → switch to linewise
        assert_eq!(v.mode, Mode::VisualLine);
    }

    #[test]
    fn ctrl_chords_ignored_in_normal() {
        // Ctrl-a must not fall through to the `a` (enter-insert) command.
        let mut v = Vim::new();
        let ctrl_a = Keystroke {
            key: "a".into(),
            key_char: Some("a".into()),
            modifiers: Modifiers { control: true, ..Default::default() },
        };
        assert!(v.on_key(&ctrl_a).is_empty());
        assert_eq!(v.mode, Mode::Normal);
    }
}
