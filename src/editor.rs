use std::path::{Path, PathBuf};

use gpui::{
    div, prelude::*, px, rgb, uniform_list, AnyElement, App, Context, FocusHandle, Focusable,
    KeyDownEvent, MouseButton, MouseUpEvent, Window,
};

use crate::document::Document;
use crate::vault::Vault;
use crate::vim::{Action, Mode, Vim};

const WELCOME: &str =
    "# Welcome to darknotes\n\nOpen a vault: darknotes <folder>\nOr a file: darknotes <path.md>\n";

/// Which pane keystrokes drive. One entity owns both panes and a single focus
/// handle, so switching is a routing flag, not a GPUI focus change.
#[derive(Clone, Copy, PartialEq)]
enum Pane {
    Editor,
    Sidebar,
}

/// The app's main view. Owns the open `Document`, the `Vim` grammar, and the
/// `Vault` (folder of notes), and renders a file-tree sidebar beside the text.
/// One entity holds everything so clicks and file-switch keys need no
/// cross-entity plumbing. Splits (multiple editors under a workspace) are a
/// later refactor.
pub struct Editor {
    doc: Document,
    vim: Vim,
    focus: FocusHandle,
    vault: Vault,
    /// Index into `vault.files` of the open file, if it's one of them.
    current: Option<usize>,
    /// Transient status-line message (command result/error); cleared each key.
    message: Option<String>,
    /// Pane that receives keystrokes (`Ctrl-W h`/`l` switches).
    pane: Pane,
    /// Sidebar cursor (the row `j`/`k` move); meaningful while in `Pane::Sidebar`.
    selected: usize,
    /// `Ctrl-W` was the previous key; the next key picks a pane.
    pending_window: bool,
}

impl Editor {
    pub fn new(vault_root: PathBuf, initial: Option<PathBuf>, cx: &mut Context<Self>) -> Self {
        let vault = Vault::scan(vault_root);
        let (doc, current) = match initial {
            Some(path) => {
                let idx = vault.files.iter().position(|f| f == &path);
                (open_or_empty(&path), idx)
            }
            None => match vault.files.first() {
                Some(first) => (open_or_empty(first), Some(0)),
                None => (Document::new(WELCOME), None),
            },
        };
        Self {
            doc,
            vim: Vim::new(),
            focus: cx.focus_handle(),
            vault,
            selected: current.unwrap_or(0),
            current,
            message: None,
            pane: Pane::Editor,
            pending_window: false,
        }
    }

    fn open_index(&mut self, i: usize, window: &mut Window) {
        let Some(path) = self.vault.files.get(i).cloned() else {
            return;
        };
        self.doc = open_or_empty(&path);
        self.vim = Vim::new();
        self.current = Some(i);
        window.focus(&self.focus); // keep keys flowing to the editor after a click
    }

    fn open_relative(&mut self, delta: isize, window: &mut Window) {
        let n = self.vault.files.len();
        if n == 0 {
            return;
        }
        let cur = self.current.unwrap_or(0) as isize;
        let next = (cur + delta).rem_euclid(n as isize) as usize;
        self.open_index(next, window);
    }

    /// Navigate the sidebar (`Pane::Sidebar`): `j`/`k` move the cursor, `l`/Enter
    /// open the selection in the editor, Escape returns without opening.
    fn sidebar_key(&mut self, key: &str, window: &mut Window) {
        let n = self.vault.files.len();
        if n == 0 {
            return;
        }
        match key {
            "j" | "down" => self.selected = (self.selected + 1).min(n - 1),
            "k" | "up" => self.selected = self.selected.saturating_sub(1),
            "l" | "enter" => {
                self.open_index(self.selected, window); // refocuses the editor pane
                self.pane = Pane::Editor;
            }
            "escape" => self.pane = Pane::Editor,
            _ => {}
        }
    }

    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.message = None; // a fresh keystroke clears the previous result
        let m = &ev.keystroke.modifiers;
        let key = ev.keystroke.key.as_str();

        // Window commands (vim `Ctrl-W h`/`l`): the prefix arms `pending_window`,
        // the next key picks a pane. Handled before vim/mode logic so it works
        // from any mode. Modifiers on the second key are ignored (Ctrl-W Ctrl-H).
        if self.pending_window {
            self.pending_window = false;
            match key {
                "h" => {
                    self.pane = Pane::Sidebar;
                    self.selected = self.current.unwrap_or(0);
                }
                "l" => self.pane = Pane::Editor,
                _ => {}
            }
            cx.notify();
            return;
        }
        if m.control && !m.alt && !m.platform && key == "w" {
            self.pending_window = true;
            return;
        }
        if self.pane == Pane::Sidebar {
            self.sidebar_key(key, window);
            cx.notify();
            return;
        }

        // App-level shortcuts. ponytail: Ctrl-N/P file-cycling is a stopgap until
        // a fuzzy switcher lands; Ctrl-S mirrors `:w`.
        if m.control && !m.alt && !m.platform {
            match ev.keystroke.key.as_str() {
                "s" => {
                    self.save();
                    cx.notify();
                    return;
                }
                "n" => {
                    self.open_relative(1, window);
                    cx.notify();
                    return;
                }
                "p" => {
                    self.open_relative(-1, window);
                    cx.notify();
                    return;
                }
                _ => {}
            }
        }
        for action in self.vim.on_key(&ev.keystroke) {
            self.apply(action, window, cx);
        }
        // Mode (hence caret style) can change with no action, so always notify.
        cx.notify();
    }

    /// The single execution seam every input grammar funnels through.
    fn apply(&mut self, action: Action, window: &mut Window, cx: &mut Context<Self>) {
        match action {
            Action::Move(m, n) => self.doc.move_motion(m, n),
            Action::DeleteMotion(m, n) => self.doc.delete_motion(m, n),
            Action::DeleteLines(n) => self.doc.delete_lines(n),
            Action::DeleteCharUnder(n) => self.doc.delete_char_under(n),
            Action::InsertText(s) => self.doc.insert(&s),
            Action::DeleteBackward => self.doc.delete_backward(),
            Action::DeleteForward => self.doc.delete_forward(),
            Action::ExecuteCommand(cmd) => self.exec_command(&cmd, window, cx),
        }
    }

    fn save(&mut self) {
        let Some(name) = self.doc.path().map(|p| p.display().to_string()) else {
            self.message = Some("E32: No file name".into());
            return;
        };
        self.message = Some(match self.doc.save() {
            Ok(()) => format!("\"{name}\" written"),
            Err(e) => format!("save failed: {e}"),
        });
    }

    /// Run a submitted `:` command. `:q` refuses on unsaved changes (vim E37);
    /// `:q!` overrides; `:wq`/`:x` quit only if the save actually succeeded.
    fn exec_command(&mut self, cmd: &str, _window: &mut Window, cx: &mut Context<Self>) {
        match cmd.trim() {
            "" => {}
            "w" => self.save(),
            "q" => {
                if self.doc.is_dirty() {
                    self.message =
                        Some("E37: No write since last change (add ! to override)".into());
                } else {
                    cx.quit();
                }
            }
            "q!" => cx.quit(),
            "wq" | "x" => {
                self.save();
                if !self.doc.is_dirty() {
                    cx.quit();
                }
            }
            other => self.message = Some(format!("E492: Not an editor command: {other}")),
        }
    }
}

fn open_or_empty(path: &Path) -> Document {
    Document::open(path).unwrap_or_else(|e| {
        eprintln!("darknotes: could not open {}: {e}", path.display());
        Document::new("")
    })
}

impl Focusable for Editor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Editor {
    fn render(&mut self, _win: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (cur_line, cur_col) = self.doc.caret_line_col();
        let rope = self.doc.rope.clone(); // ropey clone is cheap (shared, CoW)
        let line_count = rope.len_lines();
        let mode = self.vim.mode;

        let bar = if mode == Mode::Command {
            format!(":{}", self.vim.command_line())
        } else if let Some(msg) = self.message.clone() {
            msg
        } else {
            let mode_label = match mode {
                Mode::Insert => "INSERT",
                _ => "NORMAL",
            };
            let name = self
                .doc
                .path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "[No Name]".to_string());
            let dirty = if self.doc.is_dirty() { " [+]" } else { "" };
            format!("{mode_label}  {name}{dirty}")
        };

        let root = self.vault.root.clone();
        let current = self.current;
        let cursor = (self.pane == Pane::Sidebar).then_some(self.selected);
        let rows: Vec<AnyElement> = self
            .vault
            .files
            .iter()
            .enumerate()
            .map(|(i, path)| {
                let label = path
                    .strip_prefix(&root)
                    .unwrap_or(path.as_path())
                    .display()
                    .to_string();
                // Sidebar cursor (active pane) outranks the open-file highlight.
                let (bg, fg) = if cursor == Some(i) {
                    (rgb(0x3a3a5a), rgb(0xffffff))
                } else if current == Some(i) {
                    (rgb(0x2a2a40), rgb(0xffffff))
                } else {
                    (rgb(0x141414), rgb(0x9a9a9a))
                };
                div()
                    .px_2()
                    .bg(bg)
                    .text_color(fg)
                    .child(label)
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |this, _ev: &MouseUpEvent, window, cx| {
                            this.selected = i;
                            this.open_index(i, window);
                            cx.notify();
                        }),
                    )
                    .into_any_element()
            })
            .collect();

        div()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .size_full()
            .flex()
            .bg(rgb(0x1a1a1a))
            .text_color(rgb(0xcccccc))
            .text_size(px(15.))
            .line_height(px(22.))
            .child(
                div()
                    .id("sidebar") // stateful → enables overflow_y_scroll
                    .w(px(220.))
                    .h_full()
                    .flex()
                    .flex_col()
                    .bg(rgb(0x141414))
                    .overflow_y_scroll()
                    .children(rows),
            )
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(
                        uniform_list("lines", line_count, move |range, _win, _cx| {
                            range
                                .map(|i| render_line(&rope, i, cur_line, cur_col, mode))
                                .collect()
                        })
                        .flex_1(),
                    )
                    .child(
                        div()
                            .w_full()
                            .px_2()
                            .bg(rgb(0x2a2a2a))
                            .text_color(rgb(0x888888))
                            .child(bar),
                    ),
            )
    }
}

fn render_line(
    rope: &ropey::Rope,
    i: usize,
    cur_line: usize,
    cur_col: usize,
    mode: Mode,
) -> AnyElement {
    let text: String = rope.line(i).chars().filter(|c| *c != '\n').collect();
    if i != cur_line {
        return div().child(text).into_any_element();
    }
    let before: String = text.chars().take(cur_col).collect();
    let mut rest = text.chars().skip(cur_col);
    match mode {
        // ponytail: caret height hardcoded to ~line_height; derive from
        // window.line_height() when the view moves to a custom Element.
        Mode::Insert => {
            let after: String = rest.collect();
            div()
                .flex()
                .child(before)
                .child(div().w(px(2.)).h(px(18.)).bg(rgb(0xffcc00)))
                .child(after)
                .into_any_element()
        }
        // Command mode keeps the normal block caret in the text.
        Mode::Normal | Mode::Command => {
            let under = rest.next();
            let after: String = rest.collect();
            let caret = match under {
                Some(c) => div()
                    .bg(rgb(0xffcc00))
                    .text_color(rgb(0x1a1a1a))
                    .child(c.to_string()),
                None => div().w(px(9.)).h(px(18.)).bg(rgb(0xffcc00)),
            };
            div()
                .flex()
                .child(before)
                .child(caret)
                .child(after)
                .into_any_element()
        }
    }
}
