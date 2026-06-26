use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    div, fill, point, prelude::*, px, relative, size, uniform_list, AnyElement, App, Bounds,
    ContentMask, Context, FocusHandle, Focusable, Font, FontId, GlobalElementId, GlyphId, Hsla,
    InspectorElementId, KeyDownEvent, LayoutId, MouseButton, MouseUpEvent, Pixels, ScrollStrategy,
    ShapedLine, SharedString, Style, Task, TextRun, UniformListScrollHandle, Window,
};

use crate::config::Config;
use crate::document::Document;
use crate::theme::Theme;
use crate::vault::Vault;
use crate::vim::{Action, Mode, Scroll, Vim};

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
    /// Editor font, from config.
    font_family: SharedString,
    font_size: f32,
    /// Pending insert-exit timeout. Held so it stays alive; dropping/replacing it
    /// cancels the timer (gpui cancels a dropped `Task`). On fire it flushes the
    /// buffered lead keys as text.
    exit_timer: Option<Task<()>>,
}

impl Editor {
    pub fn new(
        vault_root: PathBuf,
        initial: Option<PathBuf>,
        config: Config,
        cx: &mut Context<Self>,
    ) -> Self {
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
        let vim = Vim::new(
            config.tab_width,
            &config.keymap.insert_exit,
            config.keymap.timeoutlen,
        );
        Self {
            doc,
            vim,
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
            font_family: config.font_family.into(),
            font_size: config.font_size,
            exit_timer: None,
        }
    }

    fn open_index(&mut self, i: usize, window: &mut Window) {
        let Some(path) = self.vault.files.get(i).cloned() else {
            return;
        };
        self.load(open_or_empty(&path), Some(i), window);
    }

    /// Swap in `doc` as the open buffer: reset vim/scroll state, point `current`
    /// at its vault index (`None` when it isn't a vault file), and refocus the
    /// editor so keys keep flowing after a click or command.
    fn load(&mut self, doc: Document, current: Option<usize>, window: &mut Window) {
        self.doc = doc;
        self.vim.reset(); // clears transient state, keeps config (tab/keymap)
        self.exit_timer = None;
        self.current = current;
        self.scroll.scroll_to_item(0, ScrollStrategy::Top); // a fresh buffer starts at the top
        self.last_line = 0;
        window.focus(&self.focus);
    }

    /// Guard before replacing the buffer: `true` if it's safe to discard, else
    /// sets the vim E37 message and returns `false`. `bang` (`:e!`/`:enew!`)
    /// forces it through.
    fn may_discard(&mut self, bang: bool) -> bool {
        if self.doc.is_dirty() && !bang {
            self.message = Some("E37: No write since last change (add ! to override)".into());
            false
        } else {
            true
        }
    }

    /// `:e {path}` — open `path` for editing. A nonexistent file opens as a
    /// blank buffer that `:w` creates (`Document::open` is vim-lazy). Relative
    /// names resolve under the vault root, so a new note lands in — and shows up
    /// in — the vault. Refuses to abandon unsaved changes unless `bang` (`:e!`).
    fn edit(&mut self, name: &str, bang: bool, window: &mut Window) {
        if name.is_empty() {
            self.message = Some("E32: No file name".into());
            return;
        }
        if !self.may_discard(bang) {
            return;
        }
        let path = resolve(&self.vault.root, name);
        let idx = self.vault.files.iter().position(|f| f == &path);
        self.load(open_or_empty(&path), idx, window);
    }

    /// `:enew` — start a blank, unnamed buffer; name it on the first `:w {name}`.
    fn enew(&mut self, bang: bool, window: &mut Window) {
        if !self.may_discard(bang) {
            return;
        }
        self.load(Document::new(""), None, window);
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
                    self.save(None);
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
                "r" => {
                    self.doc.redo();
                    cx.notify();
                    return;
                }
                _ => {}
            }
        }
        // Checkpoint undo once per undoable unit: before a mutating normal-mode
        // command, or on entering insert (the whole insert session coalesces
        // into that one checkpoint).
        let mode_before = self.vim.mode;
        let actions = self.vim.on_key(&ev.keystroke);
        let entering_insert = mode_before == Mode::Normal && self.vim.mode == Mode::Insert;
        // Checkpoint a single undoable unit. Insert-mode edits are excluded so the
        // whole session coalesces into the entering-insert checkpoint; everything
        // else (normal- and visual-mode mutations) gets its own.
        let mutates = mode_before != Mode::Insert && actions.iter().any(Action::mutates);
        if entering_insert || mutates {
            self.doc.checkpoint();
        }
        for action in actions {
            self.apply(action, window, cx);
        }
        self.arm_exit_timer(cx);
        // Mode (hence caret style) can change with no action, so always notify.
        cx.notify();
    }

    /// (Re)arm the insert-exit timeout while a sequence lead key is buffered, or
    /// cancel it once the buffer resolves. Each lead key restarts the clock
    /// (vim's per-key `timeoutlen`); on fire, the buffered keys are inserted as
    /// literal text. Replacing/clearing the stored `Task` cancels the prior one.
    fn arm_exit_timer(&mut self, cx: &mut Context<Self>) {
        if !self.vim.exit_pending() {
            self.exit_timer = None;
            return;
        }
        let dur = Duration::from_millis(self.vim.timeoutlen());
        self.exit_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(dur).await;
            this.update(cx, |this, cx| {
                let text = this.vim.flush_pending_exit();
                if !text.is_empty() {
                    this.doc.insert(&text);
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    /// The single execution seam every input grammar funnels through.
    fn apply(&mut self, action: Action, window: &mut Window, cx: &mut Context<Self>) {
        match action {
            // In visual mode a motion drags the selection's head; otherwise it
            // just moves the caret.
            Action::Move(m, n) => {
                if self.vim.mode.is_visual() {
                    self.doc.extend_motion(m, n);
                } else {
                    self.doc.move_motion(m, n);
                }
            }
            Action::DeleteSelection { linewise } => self.doc.delete_selection(linewise),
            Action::YankSelection { linewise } => self.doc.yank_selection(linewise),
            Action::CollapseSelection => self.doc.collapse_selection(),
            Action::DeleteMotion(m, n) => self.doc.delete_motion(m, n),
            Action::DeleteLines(n) => self.doc.delete_lines(n),
            Action::DeleteCharUnder(n) => self.doc.delete_char_under(n),
            Action::YankMotion(m, n) => self.doc.yank_motion(m, n),
            Action::YankLines(n) => self.doc.yank_lines(n),
            Action::Paste { after } => self.doc.paste(after),
            Action::InsertText(s) => self.doc.insert(&s),
            Action::DeleteBackward => self.doc.delete_backward(),
            Action::DeleteForward => self.doc.delete_forward(),
            Action::Undo => self.doc.undo(),
            // `z` scroll commands don't move the caret, so the render auto-scroll
            // won't override this; `_strict` repositions the already-visible
            // cursor line (plain `scroll_to_item` no-ops when it's on screen).
            Action::Scroll(s) => {
                let line = self.doc.caret_line_col().0;
                let strategy = match s {
                    Scroll::Center => ScrollStrategy::Center,
                    Scroll::Top => ScrollStrategy::Top,
                    Scroll::Bottom => ScrollStrategy::Bottom,
                };
                self.scroll.scroll_to_item_strict(line, strategy);
            }
            Action::ExecuteCommand(cmd) => self.exec_command(&cmd, window, cx),
        }
    }

    /// Re-read the vault from disk and re-point `current` at the open file.
    /// A file saved outside the vault root simply won't be found (`current` →
    /// `None`), which is correct — it isn't part of this vault.
    fn rescan_vault(&mut self) {
        self.vault = Vault::scan(self.vault.root.clone());
        let open = self.doc.path().map(Path::to_path_buf);
        self.current = open.and_then(|p| self.vault.files.iter().position(|f| f == &p));
    }

    /// `:w` with no arg writes the backing file; `:w <name>` saves as `<name>`,
    /// defaulting a bare name to `.md`.
    fn save(&mut self, arg: Option<&str>) {
        let result = match arg {
            Some(name) => {
                let path = resolve(&self.vault.root, name);
                let display = path.display().to_string();
                let r = self.doc.save_as(path).map(|()| display);
                if r.is_ok() {
                    // A new file may now exist under the vault root — re-scan so
                    // the sidebar shows it and `current` tracks the open file.
                    self.rescan_vault();
                }
                r
            }
            None => match self.doc.path().map(|p| p.display().to_string()) {
                Some(display) => {
                    let r = self.doc.save().map(|()| display);
                    // A blank `:e`-created buffer isn't in the vault yet (it had
                    // no file on disk); once written, re-scan so the sidebar
                    // picks it up and `current` tracks it.
                    if r.is_ok() && self.current.is_none() {
                        self.rescan_vault();
                    }
                    r
                }
                None => {
                    self.message = Some("E32: No file name".into());
                    return;
                }
            },
        };
        self.message = Some(match result {
            Ok(name) => format!("\"{name}\" written"),
            Err(e) => format!("save failed: {e}"),
        });
    }

    /// Run a submitted `:` command. `:e`/`:enew` and `:q` refuse on unsaved
    /// changes (vim E37); a trailing `!` overrides; `:wq`/`:x` quit only if the
    /// save actually succeeded.
    fn exec_command(&mut self, cmd: &str, window: &mut Window, cx: &mut Context<Self>) {
        let cmd = cmd.trim();
        if let Some(name) = cmd.strip_prefix("w ") {
            self.save(Some(name.trim()));
            return;
        }
        if let Some(name) = cmd.strip_prefix("e! ") {
            self.edit(name.trim(), true, window);
            return;
        }
        if let Some(name) = cmd.strip_prefix("e ") {
            self.edit(name.trim(), false, window);
            return;
        }
        match cmd {
            "" => {}
            "w" => self.save(None),
            "enew" => self.enew(false, window),
            "enew!" => self.enew(true, window),
            "q" => {
                if self.may_discard(false) {
                    cx.quit();
                }
            }
            "q!" => cx.quit(),
            "wq" | "x" => {
                self.save(None);
                if !self.doc.is_dirty() {
                    cx.quit();
                }
            }
            other => self.message = Some(format!("E492: Not an editor command: {other}")),
        }
    }
}

/// Bare names get a `.md` extension; anything with an extension is left alone.
fn with_md_ext(name: &str) -> PathBuf {
    let p = PathBuf::from(name);
    if p.extension().is_none() {
        p.with_extension("md")
    } else {
        p
    }
}

/// Resolve a `:w`/`:e` filename to a path: default bare names to `.md`, and
/// root relative names under the vault (`root`) so the sidebar finds them after
/// a save. Absolute paths are honored as typed.
fn resolve(root: &Path, name: &str) -> PathBuf {
    let p = with_md_ext(name);
    if p.is_absolute() {
        p
    } else {
        root.join(p)
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
        let theme = *cx.global::<Theme>();
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
        // The selected char-range to highlight, `None` outside visual mode.
        let highlight: Option<(usize, usize)> =
            mode.is_visual().then(|| self.doc.selection_span(mode == Mode::VisualLine));

        let bar = if mode == Mode::Command {
            format!(":{}", self.vim.command_line())
        } else if let Some(msg) = self.message.clone() {
            msg
        } else {
            let mode_label = match mode {
                Mode::Insert => "INSERT",
                Mode::Visual => "VISUAL",
                Mode::VisualLine => "VISUAL LINE",
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
                    (theme.sidebar_cursor_background, theme.sidebar_active_foreground)
                } else if current == Some(i) {
                    (theme.sidebar_current_background, theme.sidebar_active_foreground)
                } else {
                    (theme.sidebar_background, theme.sidebar_foreground)
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
            .bg(theme.background)
            .text_color(theme.foreground)
            // Font from config. A real installed family matters: GPUI's default
            // triggers per-line fallback scanning when absent (~8ms/cold line).
            // ponytail: line_height tracks font_size at a fixed ~1.47 ratio (22px
            // at the 15px default); expose it as its own config key only if asked.
            .font_family(self.font_family.clone())
            .text_size(px(self.font_size))
            .line_height(px(self.font_size * 22.0 / 15.0))
            .child(
                div()
                    .id("sidebar") // stateful → enables overflow_y_scroll
                    .w(px(220.))
                    .h_full()
                    .flex()
                    .flex_col()
                    .bg(theme.sidebar_background)
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
                                    selection: highlight
                                        .and_then(|(lo, hi)| line_highlight(&rope, i, lo, hi)),
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
                            .bg(theme.status_background)
                            .text_color(theme.status_foreground)
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

/// The selected column span within a line (visual mode). `to_eol` means the
/// selection covers this line's newline, so the highlight fills to the edge.
#[derive(Clone, Copy)]
struct Highlight {
    start_col: usize,
    end_col: usize,
    to_eol: bool,
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
    /// `Some` when part of this line falls inside the visual selection.
    selection: Option<Highlight>,
    /// Shared horizontal scroll offset. The cursor line writes it (prepaint),
    /// every line reads it (paint) — see `Editor::scroll_x`.
    scroll_x: Rc<Cell<Pixels>>,
}

struct LinePrepaint {
    shaped: ShapedLine,
    /// `(x within the line, width)` of the selection quad; `None` if unselected.
    selection: Option<(Pixels, Pixels)>,
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

        // Resolve the highlight columns to a pixel span. `to_eol` overshoots to
        // the pane width; the content mask in paint clips it to the line box.
        let selection = self.selection.map(|h| {
            let x0 = shaped.x_for_index(caret_bytes(&self.text, h.start_col).0);
            let width = if h.to_eol {
                bounds.size.width
            } else {
                shaped.x_for_index(caret_bytes(&self.text, h.end_col).0) - x0
            };
            (x0, width)
        });

        LinePrepaint {
            shaped,
            selection,
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
        let theme = *cx.global::<Theme>();
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            // Selection highlight sits under everything; then the caret quad, the
            // line, and the inverted caret glyph on top so it reads dark against
            // the accent block.
            if let Some((x, width)) = prepaint.selection {
                let origin = point(ox + x, bounds.origin.y);
                window.paint_quad(fill(
                    Bounds::new(origin, size(width, line_height)),
                    theme.selection,
                ));
            }
            if let Some((x, width)) = prepaint.caret {
                let origin = point(ox + x, bounds.origin.y);
                window.paint_quad(fill(
                    Bounds::new(origin, size(width, line_height)),
                    theme.accent,
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
                    window.paint_glyph(baseline, font_id, glyph_id, shaped.font_size, theme.background);
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

/// Which columns of line `i` fall inside the selection char-range `[lo, hi)`.
/// `to_eol` is set when the range reaches into this line's newline, so the
/// highlight should fill past the last char (selected blank space / joined line).
fn line_highlight(rope: &ropey::Rope, i: usize, lo: usize, hi: usize) -> Option<Highlight> {
    let line_start = rope.line_to_char(i);
    let line = rope.line(i);
    let total = line.len_chars(); // includes a trailing '\n' if present
    let content = if line.chars().last() == Some('\n') { total - 1 } else { total };
    let a = lo.max(line_start);
    let b = hi.min(line_start + total); // clamp to past-the-newline
    if a >= b {
        return None;
    }
    Some(Highlight {
        start_col: a - line_start,
        end_col: (b - line_start).min(content),
        to_eol: b > line_start + content,
    })
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
    use super::{caret_bytes, resolve};
    use std::path::Path;

    #[test]
    fn resolve_roots_relative_names_and_defaults_md() {
        let root = Path::new("/vault");
        assert_eq!(resolve(root, "foo"), Path::new("/vault/foo.md"));
        assert_eq!(resolve(root, "sub/bar.md"), Path::new("/vault/sub/bar.md"));
        // Absolute paths are honored, not re-rooted under the vault.
        assert_eq!(resolve(root, "/elsewhere/baz"), Path::new("/elsewhere/baz.md"));
    }

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
