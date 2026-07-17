//! The fuzzy picker: file switcher, buffer list, command palette, and
//! wikilink insert — one modal over the editor; keys route here while open.
//!
//! A child module of `editor` so methods can touch private `Editor` state.

use std::path::PathBuf;

use gpui::{div, hsla, prelude::*, px, uniform_list, Context, KeyDownEvent, Window};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config as NucleoConfig, Matcher, Utf32Str};

use crate::theme::Theme;

use super::{rel_display, Editor, COMMANDS};

/// What a picker row resolves to on Enter. New picker kinds add a variant
/// (jump-to-heading → `Line(usize)`).
pub(super) enum PickItem {
    File(PathBuf),
    /// A registry command, by `Command::name`.
    Command(&'static str),
    /// An open buffer, by index into `Editor::buffers`. A raw index is safe:
    /// the picker is modal, so the buffer list can't change while it's open.
    Buffer(usize),
    /// Insert `[[name]]` at the caret; payload is the `rel_display` name.
    InsertLink(String),
}

/// The open fuzzy picker (file switcher / command palette) — a modal over the
/// editor. While `Some`, keys route here (like `Pane::Sidebar`); the query
/// filters `items` via nucleo and `Enter` acts on the highlighted pick.
pub(super) struct Picker {
    /// Query-line prefix: `"> "` for files, `": "` for commands.
    title: &'static str,
    /// `(display, payload)` for every candidate, built once on open.
    items: Vec<(String, PickItem)>,
    query: String,
    /// Matches as indices into `items`, best-first; every index, in order,
    /// when the query is empty.
    results: Vec<usize>,
    pub(super) selected: usize,
}

impl Editor {
    /// `:ls` — the open-buffer picker: tab number + display name (+ `[+]`).
    pub(super) fn open_buffer_picker(&mut self) {
        let items: Vec<(String, PickItem)> = self
            .buffers
            .iter()
            .enumerate()
            .map(|(i, b)| {
                let dirty = if b.doc.is_dirty() { " [+]" } else { "" };
                (format!("{}: {}{dirty}", i + 1, self.buffer_display(b)), PickItem::Buffer(i))
            })
            .collect();
        let results = (0..items.len()).collect();
        self.picker =
            Some(Picker { title: "buf> ", items, query: String::new(), results, selected: 0 });
    }

    /// Open the fuzzy file picker over a snapshot of the current vault files.
    /// Display is the vault-relative path with `.md` dropped.
    pub(super) fn open_file_picker(&mut self) {
        let root = self.vault.root.clone();
        let items: Vec<(String, PickItem)> = self
            .vault
            .files
            .iter()
            .map(|p| (rel_display(&root, p), PickItem::File(p.clone())))
            .collect();
        let results = (0..items.len()).collect();
        self.picker = Some(Picker { title: "> ", items, query: String::new(), results, selected: 0 });
    }

    /// Open the note picker whose pick inserts a `[[wikilink]]` at the caret.
    pub(super) fn open_insert_link_picker(&mut self) {
        let root = self.vault.root.clone();
        let items: Vec<(String, PickItem)> = self
            .vault
            .files
            .iter()
            .map(|p| {
                let name = rel_display(&root, p);
                (name.clone(), PickItem::InsertLink(name))
            })
            .collect();
        let results = (0..items.len()).collect();
        self.picker =
            Some(Picker { title: "[[ ", items, query: String::new(), results, selected: 0 });
    }

    /// Open the command palette: every registry command, its primary ex alias
    /// appended to the display so typing `:w`-style names finds it too.
    pub(super) fn open_command_palette(&mut self) {
        let items: Vec<(String, PickItem)> = COMMANDS
            .iter()
            .map(|c| {
                let display = match c.ex.first() {
                    Some(ex) => format!("{}  :{}", c.name, ex),
                    None => c.name.to_string(),
                };
                (display, PickItem::Command(c.name))
            })
            .collect();
        let results = (0..items.len()).collect();
        self.picker = Some(Picker { title: ": ", items, query: String::new(), results, selected: 0 });
    }

    fn refilter_picker(&mut self) {
        if let Some(p) = self.picker.as_mut() {
            p.results = filter_items(&p.query, &p.items);
            p.selected = 0; // a new query invalidates the old highlight
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
                    if let Some(&idx) = p.results.get(p.selected) {
                        match p.items.swap_remove(idx).1 {
                            PickItem::File(path) => self.open_path(path, window), // refocuses the editor
                            PickItem::Command(name) => self.run_picked_command(name, window, cx),
                            PickItem::Buffer(i) => self.activate(i, window),
                            PickItem::InsertLink(name) => {
                                // Bypasses feed_vim's undo checkpointing, so
                                // checkpoint here or `u` swallows earlier edits.
                                self.doc_mut().checkpoint();
                                self.doc_mut().insert(&format!("[[{name}]]"));
                            }
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

    fn move_picker(&mut self, delta: isize) {
        if let Some(p) = self.picker.as_mut() {
            let n = p.results.len();
            if n == 0 {
                return;
            }
            p.selected = (p.selected as isize + delta).clamp(0, n as isize - 1) as usize;
        }
    }

    /// The fuzzy-picker overlay: a scrim + centered panel (query line + results
    /// list). `None` when the picker is closed.
    pub(super) fn render_picker(&self, theme: &Theme) -> Option<impl IntoElement> {
        let p = self.picker.as_ref()?;
        let theme = *theme;
        let prompt = format!("{}{}", p.title, p.query);
        let selected = p.selected;
        // Resolved display rows, moved into the list closure.
        let results: Vec<String> = p.results.iter().map(|&i| p.items[i].0.clone()).collect();
        let count = results.len();
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
                                        let (bg, fg) = if i == selected {
                                            (
                                                theme.sidebar_cursor_background,
                                                theme.sidebar_active_foreground,
                                            )
                                        } else {
                                            (theme.background, theme.foreground)
                                        };
                                        div()
                                            .px_2()
                                            .bg(bg)
                                            .text_color(fg)
                                            .child(results[i].clone())
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

/// Filter `items` by `query`, returning matching indices best-first (indices,
/// not displays, so duplicate display strings can't mispick). Empty query
/// passes every index through in original order.
fn filter_items(query: &str, items: &[(String, PickItem)]) -> Vec<usize> {
    if query.is_empty() {
        return (0..items.len()).collect();
    }
    // ponytail: fresh Matcher per keystroke (a few scratch allocs); cache it on
    // Picker if a 10k-note vault ever stutters.
    let mut matcher = Matcher::new(NucleoConfig::DEFAULT.match_paths());
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut buf = Vec::new();
    let mut scored: Vec<(u32, usize)> = items
        .iter()
        .enumerate()
        .filter_map(|(i, (d, _))| {
            pattern.score(Utf32Str::new(d, &mut buf), &mut matcher).map(|s| (s, i))
        })
        .collect();
    // Best score first; ties keep original item order.
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, i)| i).collect()
}


#[cfg(test)]
mod tests {
    use super::{filter_items, PickItem};
    use std::path::PathBuf;

    #[test]
    fn fuzzy_filter_ranks_and_passes_through() {
        let items = vec![
            ("projects/ideas".to_string(), PickItem::File(PathBuf::from("/v/projects/ideas.md"))),
            ("archive/old".to_string(), PickItem::File(PathBuf::from("/v/archive/old.md"))),
            ("daily/today".to_string(), PickItem::File(PathBuf::from("/v/daily/today.md"))),
        ];
        // a subsequence match ranks first
        assert_eq!(filter_items("idea", &items).first(), Some(&0));
        // empty query returns every index, original order
        assert_eq!(filter_items("", &items), vec![0, 1, 2]);
    }
}
