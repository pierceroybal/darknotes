//! The command registry: every named editor command, with its ex aliases and
//! default key bindings. Adding a command = one `Command` entry here (plus a
//! `DEFAULT_BINDINGS` row if it gets a stock key); it is then reachable from
//! the `:` line, the command palette, and `[keymap.*]` config bindings.
//!
//! A child module of `editor` so handlers can call private `Editor` methods.

use gpui::{Context, Keystroke, Modifiers, Window};

use super::{Editor, Pane};
use crate::keymap::Ctx;

/// A named editor command: one registry, three front doors (`:` line, palette,
/// key bindings).
pub(super) struct Command {
    /// Palette display and primary dispatch name.
    pub(super) name: &'static str,
    /// Ex-line aliases (`:w`, `:write`); empty = palette/binding-only.
    pub(super) ex: &'static [&'static str],
    /// Accepts a `:cmd {name}` argument. From the palette these pre-fill the
    /// ex prompt rather than run, since there's no argument to pass yet.
    pub(super) takes_arg: bool,
    /// Thin call into an `Editor` method. All handlers take the full signature
    /// even where `Window`/`Context` go unused, so the table stays uniform.
    pub(super) run: fn(&mut Editor, &CmdArgs, &mut Window, &mut Context<Editor>),
}

pub(super) struct CmdArgs {
    pub(super) bang: bool,
    pub(super) arg: Option<String>,
}

pub(super) const COMMANDS: &[Command] = &[
    Command {
        name: "save",
        ex: &["w", "write"],
        takes_arg: true,
        run: |ed, a, _win, _cx| ed.save(a.arg.as_deref()),
    },
    Command {
        name: "edit",
        ex: &["e", "edit"],
        takes_arg: true,
        run: |ed, a, win, _cx| ed.edit(a.arg.as_deref().unwrap_or(""), a.bang, win),
    },
    Command {
        name: "enew",
        ex: &["enew"],
        takes_arg: false,
        run: |ed, _a, win, _cx| ed.enew(win),
    },
    Command {
        name: "buffer",
        ex: &["b", "bu", "buffer"],
        takes_arg: true,
        run: |ed, a, win, _cx| ed.buffer_switch(a.arg.as_deref(), win),
    },
    Command {
        name: "buffer-next",
        ex: &["bn", "bnext"],
        takes_arg: false,
        run: |ed, _a, win, _cx| ed.buffer_next(win),
    },
    Command {
        name: "buffer-prev",
        ex: &["bp", "bprev", "bprevious"],
        takes_arg: false,
        run: |ed, _a, win, _cx| ed.buffer_prev(win),
    },
    Command {
        name: "buffer-delete",
        ex: &["bd", "bdelete"],
        takes_arg: false,
        run: |ed, a, win, _cx| ed.close_buffer(ed.active, a.bang, win),
    },
    Command {
        name: "buffer-alternate",
        ex: &["b#"],
        takes_arg: false,
        run: |ed, _a, win, _cx| ed.buffer_alternate(win),
    },
    Command {
        name: "buffer-picker",
        ex: &["ls", "buffers"],
        takes_arg: false,
        run: |ed, _a, _win, _cx| ed.open_buffer_picker(),
    },
    Command {
        name: "quit",
        ex: &["q", "quit"],
        takes_arg: false,
        run: |ed, a, _win, cx| ed.quit(a.bang, cx),
    },
    Command {
        name: "write-quit",
        ex: &["wq", "x"],
        takes_arg: false,
        // Save first; quit only if the save stuck (it can fail) — and `quit`
        // still refuses if some *other* buffer holds unsaved changes.
        run: |ed, _a, _win, cx| {
            ed.save(None);
            if !ed.doc().is_dirty() {
                ed.quit(false, cx);
            }
        },
    },
    Command {
        name: "set",
        ex: &["se", "set"],
        takes_arg: true,
        run: |ed, a, _win, _cx| ed.set_option(a.arg.as_deref()),
    },
    Command {
        name: "nohlsearch",
        ex: &["noh", "nohl", "nohlsearch"],
        takes_arg: false,
        // Unlight search highlights until the next search / `n` / `N`.
        run: |ed, _a, _win, _cx| ed.search.hl = false,
    },
    Command {
        name: "open-file",
        ex: &[],
        takes_arg: false,
        run: |ed, _a, _win, _cx| ed.open_file_picker(),
    },
    Command {
        name: "redo",
        ex: &[],
        takes_arg: false,
        run: |ed, _a, _win, _cx| ed.doc_mut().redo(),
    },
    Command {
        name: "command-palette",
        ex: &[],
        takes_arg: false,
        run: |ed, _a, _win, _cx| ed.open_command_palette(),
    },
    Command {
        name: "normal-mode",
        ex: &[],
        takes_arg: false,
        // A synthesized <Esc> through the grammar — leaves insert/visual/
        // command exactly like the key, caret nudge included. The target of
        // jk-style insert-exit bindings.
        run: |ed, _a, win, cx| {
            let esc =
                Keystroke { key: "escape".into(), key_char: None, modifiers: Modifiers::default() };
            ed.feed_vim(&esc, win, cx);
        },
    },
    Command {
        name: "focus-sidebar",
        ex: &[],
        takes_arg: false,
        run: |ed, _a, _win, _cx| {
            ed.pane = Pane::Sidebar;
            ed.reveal_current();
        },
    },
];

/// Built-in chords, merged under the user's `[keymap.*]` tables (a user
/// binding on the same keys shadows these). `Ctrl-W h`/`l` window nav stays
/// hardcoded in `on_key` — a prefix state machine, not a binding.
pub(super) const DEFAULT_BINDINGS: &[(Ctx, &str, &str)] = &[
    (Ctx::Global, "ctrl-s", "save"),
    (Ctx::Global, "ctrl-p", "open-file"),
    (Ctx::Global, "ctrl-shift-p", "command-palette"),
    (Ctx::Global, "ctrl-r", "redo"),
    // Vim's Ctrl-^ alternate-buffer toggle, on its US-layout key.
    (Ctx::Global, "ctrl-6", "buffer-alternate"),
];

/// Split a trimmed ex line into `(name, bang, arg)`: the name runs to the
/// first space, a trailing `!` on it sets bang (`q!`, `e! x`), the remainder —
/// trimmed — is the arg (`None` when empty).
pub(super) fn parse_ex(cmd: &str) -> (&str, bool, Option<&str>) {
    let (head, rest) = cmd.split_once(' ').unwrap_or((cmd, ""));
    let (name, bang) = match head.strip_suffix('!') {
        Some(n) => (n, true),
        None => (head, false),
    };
    let rest = rest.trim();
    (name, bang, (!rest.is_empty()).then_some(rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ex_splits_name_bang_arg() {
        assert_eq!(parse_ex("w"), ("w", false, None));
        assert_eq!(parse_ex("w x"), ("w", false, Some("x")));
        assert_eq!(parse_ex("e! notes"), ("e", true, Some("notes")));
        assert_eq!(parse_ex("q!"), ("q", true, None));
        // extra inner whitespace trims off the arg
        assert_eq!(parse_ex("e!  x"), ("e", true, Some("x")));
        assert_eq!(parse_ex("nohlsearch"), ("nohlsearch", false, None));
    }

    #[test]
    fn default_bindings_name_real_commands() {
        // A typo here would silently dead-key a default chord.
        for (_, keys, name) in DEFAULT_BINDINGS {
            assert!(
                COMMANDS.iter().any(|c| c.name == *name),
                "{keys} bound to unknown command {name}"
            );
        }
    }
}
