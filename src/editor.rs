use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui::{
    div, fill, point, prelude::*, px, relative, rgb, size, uniform_list, AnyElement, App, Bounds,
    ContentMask, Context, FocusHandle, Focusable, Font, FontId, GlobalElementId, GlyphId, Hsla,
    InspectorElementId, KeyDownEvent, LayoutId, MouseButton, MouseUpEvent, Pixels, ScrollStrategy,
    ShapedLine, SharedString, Style, TextRun, UniformListScrollHandle, Window,
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
    /// Drives the editor line list's scroll position (wheel + scroll-to-cursor).
    scroll: UniformListScrollHandle,
    /// Caret line at the last render; a change requests a scroll-to-cursor.
    last_line: usize,
    /// Horizontal scroll offset in pixels (lines have no soft-wrap, so they
    /// overflow right). The caret line's element nudges this in prepaint to keep
    /// the caret on screen; every line reads it in paint. Shared because the line
    /// elements that write/read it are built outside this struct.
    scroll_x: Rc<Cell<Pixels>>,
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
            scroll: UniformListScrollHandle::new(),
            last_line: 0,
            scroll_x: Rc::new(Cell::new(Pixels::ZERO)),
        }
    }

    fn open_index(&mut self, i: usize, window: &mut Window) {
        let Some(path) = self.vault.files.get(i).cloned() else {
            return;
        };
        self.doc = open_or_empty(&path);
        self.vim = Vim::new();
        self.current = Some(i);
        self.scroll.scroll_to_item(0, ScrollStrategy::Top); // new file starts at the top
        self.last_line = 0;
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

        // Keep the caret on screen, but only when it actually moved — so the
        // mouse wheel can scroll freely without snapping back every frame.
        // `scroll_to_item` snaps an offscreen line to the strategy's edge, so
        // pick the edge by direction: moving down lands it at the bottom, up at
        // the top — each a one-line scroll, never a page jump.
        if cur_line != self.last_line {
            let strategy = if cur_line > self.last_line {
                ScrollStrategy::Bottom
            } else {
                ScrollStrategy::Top
            };
            self.scroll.scroll_to_item(cur_line, strategy);
            self.last_line = cur_line;
        }

        let rope = self.doc.rope.clone(); // ropey clone is cheap (shared, CoW)
        let line_count = rope.len_lines();
        let mode = self.vim.mode;
        let scroll_x = self.scroll_x.clone();

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
            // ponytail: hardcoded to a font that's actually installed. GPUI's
            // default family triggers per-line fallback scanning when absent
            // (~8ms/cold line). Move to user config + per-OS defaults later.
            .font_family("DejaVu Sans Mono")
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
                                .map(|i| LineElement {
                                    text: line_text(&rope, i).into(),
                                    caret: (i == cur_line).then_some(LineCaret {
                                        col: cur_col,
                                        block: mode != Mode::Insert,
                                    }),
                                    scroll_x: scroll_x.clone(),
                                })
                                .collect()
                        })
                        .track_scroll(self.scroll.clone())
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

/// One caret on the cursor line. `block` is vim's normal/command block caret
/// (inverts the char under it); otherwise it's the insert-mode bar.
#[derive(Clone, Copy)]
struct LineCaret {
    col: usize,
    block: bool,
}

/// One text line, shaped as a single uniform run with the caret drawn entirely
/// as an overlay. Shaping never depends on the caret, so a line's layout-cache
/// key is identical whether or not the cursor is on it — cursor movement reuses
/// cached layouts instead of re-shaping every visible line each frame. The block
/// caret's inverted glyph is repainted from the cached layout, not re-shaped.
struct LineElement {
    text: SharedString,
    /// `Some` only on the cursor line.
    caret: Option<LineCaret>,
    /// Shared horizontal scroll offset. The cursor line writes it (prepaint),
    /// every line reads it (paint) — see `Editor::scroll_x`.
    scroll_x: Rc<Cell<Pixels>>,
}

struct LinePrepaint {
    shaped: ShapedLine,
    /// `(x within the line, width)` of the caret quad; `None` off the cursor line.
    caret: Option<(Pixels, Pixels)>,
    /// Block caret only: the glyph under the caret, repainted dark over the
    /// block. `(font, glyph, x within the line)`. `None` for the bar and at EOL.
    caret_glyph: Option<(FontId, GlyphId, Pixels)>,
}

impl IntoElement for LineElement {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for LineElement {
    type RequestLayoutState = ();
    type PrepaintState = LinePrepaint;

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = window.line_height().into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _layout: &mut (),
        window: &mut Window,
        _cx: &mut App,
    ) -> LinePrepaint {
        let style = window.text_style();
        let font = style.font();
        let fg = style.color;
        let font_size = style.font_size.to_pixels(window.rem_size());

        // One uniform run: the line's shaping key never depends on the caret, so
        // the layout cache keeps hitting as the cursor moves. The caret is an
        // overlay below; nothing here re-shapes per keystroke.
        let runs = [run(&font, self.text.len(), fg)];
        let shaped = window
            .text_system()
            .shape_line(self.text.clone(), font_size, &runs, None);

        let (caret, caret_glyph) = match self.caret {
            None => (None, None),
            Some(c) => {
                let (caret_byte, under_end) = caret_bytes(&self.text, c.col);
                let x = shaped.x_for_index(caret_byte);
                // Follow the caret horizontally: keep it `margin` inside both
                // edges of the pane. Only the cursor line writes scroll_x; every
                // line reads it in paint. A short line (caret near x=0) snaps the
                // offset back to 0 on its own.
                // ponytail: margin ≈ 2 chars; no mouse-wheel/`zh`/`zl` scroll yet.
                let margin = font_size * 2.;
                let viewport = bounds.size.width;
                let mut s = self.scroll_x.get();
                if x < s + margin {
                    s = x - margin;
                    if s < Pixels::ZERO {
                        s = Pixels::ZERO;
                    }
                } else if x > s + viewport - margin {
                    s = x - viewport + margin;
                }
                self.scroll_x.set(s);
                match (c.block, under_end) {
                    // Block caret over a char: full-cell quad, and grab that
                    // glyph from the cached layout to repaint it dark on top.
                    (true, Some(end)) => {
                        let glyph = shaped.runs.iter().find_map(|r| {
                            r.glyphs
                                .iter()
                                .find(|g| g.index == caret_byte)
                                .map(|g| (r.font_id, g.id, g.position.x))
                        });
                        (Some((x, shaped.x_for_index(end) - x)), glyph)
                    }
                    // ponytail: EOL block width is a font-size estimate; it only
                    // shows past the last glyph, where exactness doesn't matter.
                    (true, None) => (Some((x, font_size * 0.5)), None),
                    // Insert-mode bar.
                    (false, _) => (Some((x, px(2.))), None),
                }
            }
        };

        LinePrepaint {
            shaped,
            caret,
            caret_glyph,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _layout: &mut (),
        prepaint: &mut LinePrepaint,
        window: &mut Window,
        cx: &mut App,
    ) {
        let line_height = window.line_height();
        // Shift everything left by the horizontal scroll offset, then clip to the
        // line's box so left-overflow doesn't bleed onto the sidebar and
        // right-overflow stops at the pane edge.
        let ox = bounds.origin.x - self.scroll_x.get();
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            // Quad first (under the text), then the line, then the inverted caret
            // glyph on top so it reads dark against the yellow block.
            if let Some((x, width)) = prepaint.caret {
                let origin = point(ox + x, bounds.origin.y);
                window.paint_quad(fill(
                    Bounds::new(origin, size(width, line_height)),
                    rgb(0xffcc00),
                ));
            }
            let shaped = &prepaint.shaped;
            let _ = shaped.paint(point(ox, bounds.origin.y), line_height, window, cx);
            if let Some((font_id, glyph_id, gx)) = prepaint.caret_glyph {
                // Match the baseline `ShapedLine::paint` uses: line is vertically
                // centered, glyph sits on the baseline (`paint_glyph` y is baseline).
                let padding_top = (line_height - shaped.ascent - shaped.descent) / 2.;
                let baseline = point(ox + gx, bounds.origin.y + padding_top + shaped.ascent);
                let _ =
                    window.paint_glyph(baseline, font_id, glyph_id, shaped.font_size, rgb(0x1a1a1a).into());
            }
        });
    }
}

fn run(font: &Font, len: usize, color: Hsla) -> TextRun {
    TextRun {
        len,
        font: font.clone(),
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    }
}

/// Text of line `i` without its trailing newline.
fn line_text(rope: &ropey::Rope, i: usize) -> String {
    rope.line(i).chars().filter(|c| *c != '\n').collect()
}

/// `(byte offset of char column `col`, byte offset just past the char under it)`.
/// The second is `None` at or past end-of-line, where no char sits under the caret.
fn caret_bytes(text: &str, col: usize) -> (usize, Option<usize>) {
    let caret_byte = text
        .char_indices()
        .nth(col)
        .map(|(b, _)| b)
        .unwrap_or(text.len());
    let under_end = text[caret_byte..]
        .chars()
        .next()
        .map(|c| caret_byte + c.len_utf8());
    (caret_byte, under_end)
}

#[cfg(test)]
mod tests {
    use super::caret_bytes;

    #[test]
    fn caret_bytes_handles_unicode_and_eol() {
        // "aé": col 0 → byte 0, 'a' ends at 1; col 1 → byte 1, 'é' (2 bytes)
        // ends at 3; col 2 → EOL, byte 3, nothing under.
        assert_eq!(caret_bytes("aé", 0), (0, Some(1)));
        assert_eq!(caret_bytes("aé", 1), (1, Some(3)));
        assert_eq!(caret_bytes("aé", 2), (3, None));
        assert_eq!(caret_bytes("", 0), (0, None));
    }
}
