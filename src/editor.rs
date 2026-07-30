mod buffers;
mod command;
mod line_element;
mod picker;
mod row_list;
mod rows;
mod search;
mod sidebar;

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    div, hsla, prelude::*, px, svg, uniform_list, App, ClipboardItem, Context, Div, FocusHandle,
    Focusable, KeyDownEvent, KeyUpEvent, Keystroke, MouseButton, MouseDownEvent, MouseUpEvent,
    Pixels, ScrollStrategy, SharedString, Task, UniformListScrollHandle, Window,
};
use crate::config::{Config, LineNumbers, Search as SearchConfig};
use crate::document::{Document, Motion};
use crate::jumps::{Jumps, Pos};
use crate::keymap::{Ctx, Resolver};
use buffers::{
    open_or_empty, rel_display, resolve, resolve_link, unique_dest, vault_relative, with_md_ext,
    Buffer,
};
use command::{parse_ex, CmdArgs, COMMANDS, DEFAULT_BINDINGS};
use line_element::{
    caret_bytes, fence_block, heading_metrics, row_decor, run, segments_to_runs, CaretPaint,
    Gutter, Highlight, LineCaret, LineElement, RowDecor, CODE_MARGIN, CODE_PAD,
};
use picker::Picker;
use row_list::row_list;
use rows::{
    caret_only_change, content_change_is_local, row_offsets, LineForms, RowsCache, RowsKey,
    ShapeWrapCache,
};
use search::{search_sensitive, MatchCache, SearchState};
use sidebar::{expand_ancestors, FilePrompt, PromptAction};
// `line_text` lives in the scanner: every caller wants a newline-stripped line
// in order to feed it a markdown function.
use crate::markdown::{self, line_text, SpanKind};
use crate::session;
use crate::theme::Theme;
use crate::vault::{Row, Vault};
use crate::vim::{Action, Mode, Scroll, Vim};
use crate::watcher::VaultWatcher;

const WELCOME: &str =
    "# Welcome to darknotes\n\nOpen a vault: darknotes <folder>\nOr a file: darknotes <path.md>\n";

/// Fixed sidebar width. The soft-wrap width is derived as viewport minus
/// this, so it must match the `.w(px(SIDEBAR_WIDTH))` on the sidebar list.
const SIDEBAR_WIDTH: f32 = 330.;

// ponytail: fixed poll cadence over an async channel wakeup, to avoid a
// second crate (futures) just for `mpsc::UnboundedReceiver::next()`. Drop if
// 200ms ever feels laggy.
const FS_POLL_INTERVAL_MS: u64 = 200;

/// Which pane keystrokes drive. One entity owns both panes and a single focus
/// handle, so switching is a routing flag, not a GPUI focus change.
#[derive(Clone, Copy, PartialEq)]
enum Pane {
    Editor,
    Sidebar,
}

/// The app's main view. Owns the open `Buffer`s, the `Vim` grammar, and the
/// `Vault` (folder of notes), and renders a file-tree sidebar beside the text.
/// One entity holds everything so clicks and file-switch keys need no
/// cross-entity plumbing. Splits (multiple editors under a workspace) are a
/// later refactor.
pub struct Editor {
    /// Open buffers, in tab order. Never empty; `active` indexes into it.
    buffers: Vec<Buffer>,
    active: usize,
    /// Previously active buffer — the `Ctrl-6` / `:b #` target. Re-pointed
    /// when a `:bd` shifts indices.
    alternate: Option<usize>,
    vim: Vim,
    /// The last completed change as its emitted actions — what `.` replays.
    /// A change that entered insert mode carries the whole insert session
    /// (recorded via `pending_change`, sealed when insert exits).
    last_change: Option<Vec<Action>>,
    /// Change being recorded while insert mode is open: the entering command's
    /// actions, growing with each insert keystroke until exit seals it into
    /// `last_change`.
    pending_change: Option<Vec<Action>>,
    focus: FocusHandle,
    vault: Vault,
    /// Recursive watch on the vault root for external changes (an agent, a
    /// script, `git checkout`); `None` when `config.watch_files` is off or the
    /// watch failed to start (missing inotify capacity, etc).
    watcher: Option<VaultWatcher>,
    /// Polls `watcher` on a timer. Held so it stays alive; dropping cancels it
    /// (gpui cancels a dropped `Task`), like `blink_timer`/`seq_timer`.
    fs_poll_timer: Option<Task<()>>,
    /// Transient status-line message (command result/error); cleared each key.
    message: Option<String>,
    /// Pane that receives keystrokes (`Ctrl-W h`/`l` switches).
    pane: Pane,
    /// Sidebar cursor as an index into the currently *visible* rows (folders +
    /// expanded contents); meaningful while in `Pane::Sidebar`.
    selected: usize,
    /// Folder paths the user has expanded in the sidebar tree. Keyed by path so
    /// it survives a re-scan; stale entries for vanished folders are harmless.
    expanded: HashSet<PathBuf>,
    /// Drives the sidebar list's scroll position (wheel + scroll-to-selected).
    sidebar_scroll: UniformListScrollHandle,
    /// Sidebar cursor at the last render; a change scrolls it into view.
    last_selected: usize,
    /// `Ctrl-W` was the previous key; the next key picks a pane.
    pending_window: bool,
    /// Armed first key of a doubled sidebar file op (`dd`/`cc`/`yy`); the
    /// next key completes or cancels it.
    pending_sidebar: Option<char>,
    /// Sidebar cut/copy register: the marked path plus whether `p` moves
    /// (`x`) or copies (`yy`) it.
    file_register: Option<(PathBuf, bool)>,
    /// Open file-op prompt (create/rename/delete-confirm), or `None`.
    /// Routes keys while `Some`, like the picker.
    prompt: Option<FilePrompt>,
    /// Drives the editor line list's scroll position (wheel + scroll-to-cursor).
    scroll: UniformListScrollHandle,
    /// Caret *visual row* at the last render (soft-wrap makes rows outnumber
    /// lines); a change requests a scroll-to-cursor. Also what `zz`/`zt`/`zb`
    /// reposition — fresh, since z-scrolls don't move the caret.
    last_row: usize,
    /// Center the caret's row on the next render. Set by buffer switches: the
    /// shared scroll handle still holds the old buffer's offset, and the new
    /// buffer's visual-row index isn't known until render builds its wrap map.
    center_on_render: bool,
    /// Soft-wrap long lines at the pane edge (`:set wrap`/`nowrap`, config
    /// `wrap`). Off = long lines overflow right behind `scroll_x`.
    wrap: bool,
    /// The last row build, reused while nothing it depends on changed
    /// (`RowsKey`). Scroll-only frames — the common case while reading —
    /// rebuild nothing; caret-only moves patch two lines.
    rows_cache: Option<RowsCache>,
    /// Cross-build cache of shaped wrap boundaries; see `ShapeWrapCache`.
    wrap_cache: ShapeWrapCache,
    /// Per-line concealed render forms; see `LineForms`. Makes a `Plan::Full`
    /// driven by anything but content (a visual sweep, a search keystroke, an
    /// undo) skip the per-line conceal pipeline.
    line_forms: LineForms,
    /// Markdown parse of the active buffer, memoized on its revision (the
    /// parse is document-wide and caret-independent).
    spans_cache: Option<(u64, Rc<markdown::Parsed>)>,
    /// Horizontal scroll offset in pixels (lines have no soft-wrap, so they
    /// overflow right). The caret line's element nudges this in prepaint to keep
    /// the caret on screen; every line reads it in paint. Shared because the line
    /// elements that write/read it are built outside this struct.
    scroll_x: Rc<Cell<Pixels>>,
    /// Editor font, from config.
    font_family: SharedString,
    /// Chrome font (sidebar, tabline, status bar, picker), from config's
    /// `ui_font_family`; resolved at startup to the editor font when unset.
    ui_font_family: SharedString,
    font_size: f32,
    /// Line-number gutter mode, from config.
    line_numbers: LineNumbers,
    /// Hide markdown syntax markers on non-cursor lines, from config.
    render_markdown: bool,
    /// Keymap bindings (defaults + `[keymap.*]` config), resolved per keystroke
    /// before the vim grammar.
    keymap: Resolver,
    /// `timeoutlen` (ms) for partially-typed multi-key bindings, from config.
    timeoutlen: u64,
    /// Pending binding-sequence timeout. Held so it stays alive; dropping or
    /// replacing it cancels the timer (gpui cancels a dropped `Task`). On fire
    /// the buffered keys replay through the grammar as ordinary input.
    seq_timer: Option<Task<()>>,
    /// `key_repeat_delay`/`key_repeat_interval` (ms), from config — see
    /// `on_key`. `key_repeat_interval == 0` disables self-driven repeat and
    /// passes the backend's native repeat events straight through.
    key_repeat_delay: u64,
    key_repeat_interval: u64,
    /// The keystroke we're currently auto-repeating, or `None`. While set,
    /// `on_key` swallows the backend's echoes of it (see there for how an
    /// echo is recognized). Cleared by any `KeyUp` and on window
    /// deactivation.
    repeat_stroke: Option<Keystroke>,
    /// Pending repeat cadence. Replacing/clearing cancels the timer (gpui
    /// cancels a dropped `Task`), like `blink_timer`/`seq_timer`.
    repeat_timer: Option<Task<()>>,
    /// Open fuzzy picker, or `None`. Routes keys when `Some`.
    picker: Option<Picker>,
    /// Drives the picker results list scroll (scroll-to-selected).
    picker_scroll: UniformListScrollHandle,
    /// Cross-note jump history (`Ctrl-O`/`Ctrl-I`). On the editor, not the
    /// document — like the search register, it spans buffer switches.
    jumps: Jumps,
    /// `mA`–`mZ`: global marks, each carrying its file. Lowercase marks are
    /// per-buffer and live on the `Document`.
    marks: HashMap<char, Pos>,
    /// `/`-search state (`/`, `?`, `n`, `N`, hlsearch).
    search: SearchState,
    /// Search options from config (ignorecase, hlsearch, …).
    search_cfg: SearchConfig,
    /// Memoized match scan; see `MatchCache`. Shared by the incsearch jump and
    /// the render path, which otherwise scanned the buffer twice per keystroke.
    match_cache: Option<MatchCache>,
    /// Blink the caret while the editor pane is focused, from config.
    cursor_blink: bool,
    /// Length (ms) of each blink phase, from config. 0 disables blinking.
    blink_interval: u64,
    /// Caret visible in the current blink phase. Every keystroke resets it to
    /// true, so the caret reads solid while typing.
    blink_show: bool,
    /// Running blink toggler. Held so it stays alive; replacing it cancels the
    /// old cycle (gpui cancels a dropped `Task`), which restarts the phase.
    blink_timer: Option<Task<()>>,
    /// How the caret paints this frame (blink phase + pane focus). Shared with
    /// the line elements like `scroll_x`, so a blink tick or pane switch only
    /// repaints — the cached rows never rebuild for it.
    caret_paint: Rc<Cell<CaretPaint>>,
    /// The caret's logical line, shared with the line elements like
    /// `caret_paint`. Relative line numbers are relative to *this*, resolved
    /// when a row paints, so moving the caret re-labels the gutter without
    /// invalidating a single cached row.
    cur_line: Rc<Cell<usize>>,
}

impl Editor {
    pub fn new(
        vault_root: PathBuf,
        initial: Option<PathBuf>,
        config: Config,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // A resize only repaints by default. The wrap width reads the live
        // viewport in render, so force a render after every bounds change —
        // the async maximize at startup otherwise lands after the last
        // render and leaves stale narrow wrapping until the next keystroke.
        cx.observe_window_bounds(window, |_, _, cx| cx.notify()).detach();
        // OS focus dims the caret (render reads `is_window_active`); repaint on
        // the change, and restart the blink phase so re-focusing shows it solid.
        // Also drop any in-flight key-repeat: alt-tabbing away mid-hold loses
        // the `KeyUp`, and a stale `repeat_stroke` would swallow that key's
        // next real press.
        cx.observe_window_activation(window, |this, _, cx| {
            this.arm_blink(cx);
            this.repeat_stroke = None;
            this.repeat_timer = None;
            cx.notify();
        })
        .detach();
        let vault = Vault::scan(vault_root);
        // An explicit CLI file means a fresh session; otherwise rebuild last
        // session's tabs. Files deleted since then are skipped (Document::open,
        // not open_or_empty — a missing file must not resurrect as an empty
        // buffer).
        let mut buffers = Vec::new();
        let mut active = 0;
        if initial.is_none() {
            if let Some(s) = session::restore(&vault.root) {
                let mut seen_preview = false;
                for e in s.files {
                    let Ok(mut doc) = Document::open(&e.path) else { continue };
                    doc.jump_to(e.caret);
                    // At most one preview buffer exists; against a hand-edited
                    // or stale file, the first preview wins and the rest pin.
                    let preview = e.preview && !seen_preview;
                    seen_preview |= preview;
                    buffers.push(Buffer { doc, preview });
                }
                active = s.active.min(buffers.len().saturating_sub(1));
            }
        }
        let restored = !buffers.is_empty();
        if !restored {
            let doc = match initial {
                Some(path) => open_or_empty(&path),
                None => match vault.files.first() {
                    Some(first) => open_or_empty(first),
                    None => Document::new(WELCOME),
                },
            };
            // The startup buffer is a preview like any other open: the first
            // navigation replaces it, the first edit commits it.
            buffers.push(Buffer { doc, preview: true });
        }
        let vim = Vim::new(config.tab_width);
        let names: Vec<&str> = COMMANDS.iter().map(|c| c.name).collect();
        let keymap = Resolver::new(&config.keymap, DEFAULT_BINDINGS, &names);
        // Open the tree to the active file and park the sidebar cursor on it.
        let mut expanded = HashSet::new();
        let selected = buffers[active]
            .doc
            .path()
            .map(|p| {
                expand_ancestors(&vault.root, p, &mut expanded);
                vault
                    .visible_rows(&expanded)
                    .iter()
                    .position(|r| r.path == p)
                    .unwrap_or(0)
            })
            .unwrap_or(0);
        let mut this = Self {
            buffers,
            active,
            alternate: None,
            vim,
            last_change: None,
            pending_change: None,
            focus: cx.focus_handle(),
            vault,
            watcher: None,
            fs_poll_timer: None,
            selected,
            expanded,
            sidebar_scroll: UniformListScrollHandle::new(),
            last_selected: selected,
            message: None,
            pane: Pane::Editor,
            pending_window: false,
            pending_sidebar: None,
            file_register: None,
            prompt: None,
            scroll: UniformListScrollHandle::new(),
            last_row: 0,
            // A restored mid-file caret must start centered; the render pass
            // owns the scroll because the caret's visual row needs this
            // buffer's wrap map (same as a buffer switch).
            center_on_render: restored,
            wrap: config.wrap,
            rows_cache: None,
            wrap_cache: ShapeWrapCache::default(),
            line_forms: LineForms::default(),
            spans_cache: None,
            scroll_x: Rc::new(Cell::new(Pixels::ZERO)),
            font_family: config.font_family.clone().into(),
            ui_font_family: if config.ui_font_family.is_empty() {
                config.font_family.into()
            } else {
                config.ui_font_family.into()
            },
            font_size: config.font_size,
            line_numbers: config.line_numbers,
            render_markdown: config.render_markdown,
            keymap,
            timeoutlen: config.keymap.timeoutlen,
            seq_timer: None,
            key_repeat_delay: config.key_repeat_delay,
            key_repeat_interval: config.key_repeat_interval,
            repeat_stroke: None,
            repeat_timer: None,
            picker: None,
            picker_scroll: UniformListScrollHandle::new(),
            jumps: Jumps::default(),
            marks: HashMap::new(),
            search: SearchState::default(),
            search_cfg: config.search,
            match_cache: None,
            cursor_blink: config.cursor_blink,
            blink_interval: config.cursor_blink_interval,
            blink_show: true,
            blink_timer: None,
            caret_paint: Rc::new(Cell::new(CaretPaint::Solid)),
            cur_line: Rc::new(Cell::new(0)),
        };
        this.arm_blink(cx);
        if config.watch_files {
            match VaultWatcher::new(&this.vault.root) {
                Ok(w) => {
                    this.watcher = Some(w);
                    this.arm_fs_watch(cx);
                }
                Err(e) => eprintln!("darknotes: could not watch vault for changes: {e}"),
            }
        }
        this
    }

    /// Show the caret solid and (re)start the blink cycle — every keystroke
    /// lands here, so the caret never blinks away mid-typing.
    fn arm_blink(&mut self, cx: &mut Context<Self>) {
        self.blink_show = true;
        if !self.cursor_blink || self.blink_interval == 0 {
            self.blink_timer = None;
            return;
        }
        let dur = Duration::from_millis(self.blink_interval);
        self.blink_timer = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(dur).await;
                let alive = this.update(cx, |this, cx| {
                    // A dim caret (unfocused window / sidebar pane) is static:
                    // skip the toggle and, critically, the notify — repainting
                    // would redraw pixel-identical frames twice a second for
                    // as long as the app is unfocused. Phase parks on "shown"
                    // so the caret is solid the instant visibility returns.
                    // `caret_paint` is render-maintained, so it covers every
                    // pane/focus transition without hooking each one.
                    if this.caret_paint.get() == CaretPaint::Dim {
                        this.blink_show = true;
                        return;
                    }
                    this.blink_show = !this.blink_show;
                    cx.notify();
                });
                if alive.is_err() {
                    return;
                }
            }
        }));
    }

    /// Poll `self.watcher` on a timer, mirroring `arm_blink`'s bridge from
    /// gpui's background executor back onto the entity.
    fn arm_fs_watch(&mut self, cx: &mut Context<Self>) {
        let dur = Duration::from_millis(FS_POLL_INTERVAL_MS);
        self.fs_poll_timer = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(dur).await;
                let alive = this.update(cx, |this, cx| this.poll_fs_events(cx));
                if alive.is_err() {
                    return;
                }
            }
        }));
    }

    /// React to filesystem events queued since the last tick: reload clean
    /// open buffers whose backing file changed (keeping the caret's offset,
    /// clamped, rather than snapping to the top), flag ones whose file was
    /// deleted, warn (never clobber) dirty ones, and rescan the vault tree
    /// once per batch for anything else (create/delete/rename anywhere under
    /// the vault root).
    ///
    /// For a path backing an open buffer, the file is read once and that read
    /// answers everything — existence, and (by hashing against the document's
    /// `disk_hash`) whether the content actually differs from what this
    /// buffer last loaded or saved. Event kinds are never trusted: atomic
    /// saves and backend quirks make them unreliable, and our own `save`
    /// echoes back through the watcher looking just like an external edit.
    fn poll_fs_events(&mut self, cx: &mut Context<Self>) {
        let Some(watcher) = self.watcher.as_ref() else { return };
        let events = watcher.drain();
        if events.is_empty() {
            return;
        }
        let mut tree_changed = false;
        let mut repaint = false;
        for event in &events {
            // Pure-read events (inotify OPEN/CLOSE_NOWRITE) can't change
            // content or the tree, and our own directory reads emit them —
            // without this guard every `Vault::scan` triggers the next one,
            // a self-sustaining rescan loop at the poll interval. Real
            // changes still arrive as Modify/Create/Remove/Rename.
            if matches!(event.kind, notify::EventKind::Access(_)) {
                continue;
            }
            for path in &event.paths {
                let Some(i) =
                    self.buffers.iter().position(|b| b.doc.path() == Some(path.as_path()))
                else {
                    tree_changed = true;
                    continue;
                };
                let disk = match std::fs::read_to_string(path) {
                    Ok(s) => s,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        // Already flagged: don't re-warn on every duplicate
                        // remove event some backends fire for one deletion.
                        if !self.buffers[i].doc.is_missing() {
                            self.buffers[i].doc.set_missing(true);
                            let name = self.buffer_display(&self.buffers[i]);
                            self.message = Some(format!("file deleted: {name}"));
                            repaint = true;
                        }
                        continue;
                    }
                    // Transient (permissions, mid-rename); a follow-up event
                    // will re-check.
                    Err(_) => continue,
                };
                let hash = Document::hash_text(&disk);
                if hash == self.buffers[i].doc.disk_hash() {
                    // Disk holds exactly what this buffer last loaded or
                    // saved — the event is an echo of our own write (or a
                    // content-identical external one). Nothing to report.
                    if self.buffers[i].doc.is_missing() {
                        self.buffers[i].doc.set_missing(false);
                        repaint = true;
                    }
                    continue;
                }
                if self.buffers[i].doc.is_dirty() {
                    self.buffers[i].doc.set_missing(false); // back, even if we won't reload it
                    self.buffers[i].doc.set_disk_hash(hash); // dedupe repeat events
                    let name = self.buffer_display(&self.buffers[i]);
                    self.message =
                        Some(format!("W12: {name} changed on disk (unsaved changes kept)"));
                    repaint = true;
                } else {
                    let caret = self.buffers[i].doc.caret_offset();
                    self.buffers[i].doc = open_or_empty(path);
                    self.buffers[i].doc.jump_to(caret); // clamped; best-effort vs. shifted content
                    repaint |= i == self.active;
                }
            }
        }
        if tree_changed {
            self.rescan_vault();
            repaint = true;
        }
        if repaint {
            cx.notify();
        }
    }

    /// `:set {option}` — vim option toggles. Only 'wrap' exists so far; grow
    /// this into an option table when the second option arrives.
    fn set_option(&mut self, arg: Option<&str>) {
        match arg {
            Some("wrap") => self.wrap = true,
            Some("nowrap") => self.wrap = false,
            Some("wrap!") | Some("invwrap") => self.wrap = !self.wrap,
            // hlsearch is otherwise only reachable through config + restart,
            // which makes its render path awkward to exercise. `:noh` still
            // clears the current highlight without changing the option.
            Some("hlsearch") | Some("hls") => self.search_cfg.hlsearch = true,
            Some("nohlsearch") | Some("nohls") => self.search_cfg.hlsearch = false,
            Some("hlsearch!") | Some("invhlsearch") => {
                self.search_cfg.hlsearch = !self.search_cfg.hlsearch
            }
            Some("hlsearch?") | Some("hls?") => {
                self.message =
                    Some(if self.search_cfg.hlsearch { "  hlsearch" } else { "nohlsearch" }.into())
            }
            // Bare `:set` / `:set wrap?` report the current value, vim-style.
            None | Some("wrap?") => {
                self.message = Some(if self.wrap { "  wrap" } else { "nowrap" }.into())
            }
            Some(other) => self.message = Some(format!("E518: Unknown option: {other}")),
        }
    }

    /// `:theme {name}` — swap the palette live. Bare `:theme` lists the
    /// built-ins. Doesn't touch config.toml; the configured theme still wins
    /// on next launch.
    fn set_theme(&mut self, arg: Option<&str>, cx: &mut Context<Self>) {
        let Some(name) = arg else {
            let names: Vec<_> = crate::theme::Theme::names().collect();
            self.message = Some(format!("themes: {}", names.join(", ")));
            return;
        };
        match crate::theme::Theme::by_name(name) {
            Some(t) => cx.set_global(t),
            None => self.message = Some(format!("E185: Cannot find color scheme '{name}'")),
        }
    }

    /// The active buffer's markdown spans, memoized on its content revision,
    /// plus the one line a reparse was confined to when it was.
    ///
    /// A whole-document parse is the fallback, not the norm: an insert-mode
    /// keystroke changes one line, so `markdown::reparse_line` re-scans that line
    /// and keeps the rest — mutating the cached parse in place, since it is
    /// uniquely held between renders and `Rc::make_mut` then copies nothing. The
    /// returned line is what lets render patch rows for an *edit* rather than
    /// rebuilding the document (see the `Plan` match).
    ///
    /// `Some(line)` is a promise the caller acts on, so it is only ever returned
    /// when `Document::single_line_edit` confirms the cached parse is this
    /// buffer's state immediately before the edit, and the scanner state leaving
    /// that line is unchanged.
    fn spans_for_render(&mut self) -> (Rc<markdown::Parsed>, Option<usize>) {
        let rev = self.doc().revision();
        if let Some((r, s)) = &self.spans_cache {
            if *r == rev {
                return (s.clone(), None); // content unchanged: nothing reparsed
            }
        }
        let cached_rev = self.spans_cache.as_ref().map(|(r, _)| *r);
        let edited = cached_rev.and_then(|r| self.doc().single_line_edit(r)).map(|e| e.line);
        let is_md = self.doc().is_markdown();
        let rope = self.doc().rope.clone(); // ropey clone shares its backing

        if is_md {
            if let (Some(line), Some((r, parsed))) = (edited, self.spans_cache.as_mut()) {
                if markdown::reparse_line(&rope, Rc::make_mut(parsed), line) {
                    *r = rev;
                    return (parsed.clone(), Some(line));
                }
                // The edit moved a fence/frontmatter boundary, restyling every
                // line below it — reparse and rebuild in full.
            }
        }
        let parsed = Rc::new(if is_md {
            markdown::parse(&rope)
        } else {
            markdown::Parsed::blank(rope.len_lines())
        });
        self.spans_cache = Some((rev, parsed.clone()));
        (parsed, None)
    }

    /// The spans alone, for the paths outside render (click hit-testing,
    /// `gj`/`gk`) that don't care what changed.
    fn spans(&mut self) -> Rc<markdown::Parsed> {
        self.spans_for_render().0
    }

    /// Left click in the text area: place the caret at the clicked spot.
    /// Hit-testing reads the rendered rows cache — the exact geometry on
    /// screen — and re-shapes only the clicked row to turn x into a column.
    /// Insert mode stays in insert (click to focus, keep typing); any other
    /// mode resets to normal, dropping a visual selection or pending
    /// operator like a motion-aborting Esc.
    // ponytail: single click only; drag-select and double-click word
    // select when they're missed.
    fn click_to_caret(&mut self, pos: gpui::Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        // Modal UI owns the mouse: the picker overlays the list, a file-op
        // prompt is mid-edit, command mode is mid-`:`/search.
        if self.picker.is_some() || self.prompt.is_some() || self.vim.mode == Mode::Command {
            return;
        }
        let (rows, line_rows, offsets) = match &self.rows_cache {
            Some(c) => (c.rows.clone(), c.line_rows.clone(), c.offsets.clone()),
            None => return,
        };
        let (bounds, offset) = {
            let s = self.scroll.0.borrow();
            (s.base_handle.bounds(), s.base_handle.offset())
        };
        // offset.y is ≤ 0 once scrolled; a click past EOF (the overscroll
        // room) clamps to the last real row, vim-style.
        let y = pos.y - bounds.origin.y - offset.y;
        let row = offsets
            .partition_point(|&o| o <= y)
            .saturating_sub(1)
            .min(rows.len().saturating_sub(1));
        let Some(el) = rows.get(row) else { return };

        // Visual row → logical line + the line's first visual row.
        let (mut line, mut first) = (0usize, 0usize);
        while line + 1 < line_rows.len() && first + line_rows[line] as usize <= row {
            first += line_rows[line] as usize;
            line += 1;
        }

        // x → byte within this row's display text, mirroring paint's origin:
        // past the gutter, inset when the row sits in a code band, shifted by
        // the horizontal scroll.
        let font = gpui::font(self.font_family.clone());
        let font_size = px(self.font_size);
        let theme = *cx.global::<Theme>();
        let gutter_w = el.gutter.as_ref().map_or(Pixels::ZERO, |g| {
            let (text, _) = g.resolve(&theme);
            let runs = [run(&font, text.len(), theme.foreground)];
            window.text_system().shape_line(text, font_size, &runs, None).width
        });
        let pad = match el.decor {
            Some(RowDecor::CodeBand { .. }) => CODE_MARGIN + CODE_PAD,
            _ => Pixels::ZERO,
        };
        let runs = segments_to_runs(&el.text, &el.segments, &font, theme.foreground, &theme);
        let shaped =
            window.text_system().shape_line(el.text.clone(), font_size * el.scale, &runs, None);
        let x = pos.x - bounds.origin.x - gutter_w - pad + self.scroll_x.get();
        let byte_in_row = shaped.closest_index_for_x(x.max(Pixels::ZERO));

        // A click on the painted task box toggles it, caret untouched. The
        // row's cached segments are exactly what paint saw: a Task segment
        // means a box was drawn over those bytes. A revealed line (caret
        // line, fence reveal, markdown off) remaps Task→Marker at row build,
        // so its raw `[ ]` takes the caret like any other text.
        let mut seg_start = 0;
        for seg in el.segments.iter() {
            if byte_in_row < seg_start + seg.len {
                if matches!(seg.kind, Some(SpanKind::Task(_))) {
                    self.toggle_task(line);
                    self.pane = Pane::Editor;
                    window.focus(&self.focus);
                    self.arm_blink(cx);
                    cx.notify();
                    return;
                }
                break;
            }
            seg_start += seg.len;
        }

        // Byte within the line's display text: earlier rows are earlier
        // slices of it, so their lengths accumulate.
        let n = line_rows[line] as usize;
        let display_byte =
            rows[first..row].iter().map(|r| r.text.len()).sum::<usize>() + byte_in_row;

        let col = self.display_source_col(line, &rows[first..first + n], display_byte);
        let at = self.doc().rope.line_to_char(line) + col;
        if self.vim.mode != Mode::Insert {
            self.vim.reset();
        }
        self.doc_mut().jump_to(at);
        if self.vim.mode != Mode::Insert {
            self.doc_mut().clamp_caret_to_line();
        }
        self.pane = Pane::Editor;
        window.focus(&self.focus);
        self.arm_blink(cx);
        cx.notify();
    }

    /// Byte offset within `line`'s display text (`line_rows` = its rendered
    /// rows, in order) → source char column. A revealed line (cursor line,
    /// fence reveal, markdown off) displays its source verbatim — compare
    /// instead of re-deriving reveal state. A concealed line re-runs the
    /// conceal for its source→display map and inverts it: the last source
    /// byte mapping at or before the display byte is the kept char there
    /// (dropped marker bytes collapse onto the *next* kept byte, so they
    /// sort before it and lose).
    fn display_source_col(
        &mut self,
        line: usize,
        line_rows: &[LineElement],
        display_byte: usize,
    ) -> usize {
        let src = line_text(&self.doc().rope, line);
        let display: String = line_rows.iter().map(|r| r.text.as_ref()).collect();
        let source_byte = if display == src {
            display_byte
        } else {
            let spans = self.spans();
            let segs = markdown::flatten(src.len(), spans.get(line).map_or(&[][..], Vec::as_slice));
            display_to_source(&markdown::conceal(&src, &segs).map, display_byte)
        };
        src[..source_byte.min(src.len())].chars().count()
    }

    /// `gj`/`gk`: move the caret one *visual* row, keeping its column within
    /// the row. Lives here rather than in `Document` because only the rows
    /// cache knows wrap boundaries; the cache is current because the two
    /// grammar keys (`g`, then `j`/`k`) moved nothing since the last render.
    /// The column maps through the conceal machinery like a click, so landing
    /// on a concealed line puts the caret on the source char displayed there.
    // ponytail: no display goal column — each step re-derives the column
    // from the caret, so a run of gj across a short row drifts left (vim
    // would return to the goal column). Track one if it grates.
    fn move_display(&mut self, down: bool) {
        let Some(c) = &self.rows_cache else { return };
        let target = if down { c.cur_row + 1 } else { c.cur_row.wrapping_sub(1) };
        if target >= c.rows.len() {
            return; // first/last row (wrapping_sub underflows past the top)
        }
        self.move_to_row(target);
    }

    /// Land the caret on visual row `target` (clamped to the buffer), keeping
    /// its display column — the shared engine under `gj`/`gk` and the
    /// viewport scrolls.
    fn move_to_row(&mut self, target: usize) {
        let Some(c) = &self.rows_cache else { return };
        let (rows, line_rows, cur_row) = (c.rows.clone(), c.line_rows.clone(), c.cur_row);
        let target = target.min(rows.len() - 1);
        if target == cur_row {
            return;
        }

        // Display char column within the caret's row, read from the built row
        // itself — correct whether the line rendered revealed (source text)
        // or concealed (view mode remaps the caret at row build).
        let col_in_row = rows[cur_row].caret.as_ref().map_or(0, |lc| lc.col);

        // Target row → its logical line + that line's first row.
        let (mut tline, mut tfirst) = (0usize, 0usize);
        while tline + 1 < line_rows.len() && tfirst + line_rows[tline] as usize <= target {
            tfirst += line_rows[tline] as usize;
            tline += 1;
        }
        // Same char column in the target row (clamped to its end), as a byte
        // offset into the target line's display text.
        let row = &rows[target];
        let byte_in_row =
            row.text.char_indices().nth(col_in_row).map_or(row.text.len(), |(b, _)| b);
        let display_byte =
            rows[tfirst..target].iter().map(|r| r.text.len()).sum::<usize>() + byte_in_row;
        let n = line_rows[tline] as usize;
        let tcol = self.display_source_col(tline, &rows[tfirst..tfirst + n], display_byte);
        let at = self.doc().rope.line_to_char(tline) + tcol;
        self.doc_mut().jump_to(at); // feed_vim clamps to the line after us
    }

    /// The editor's base row height — a scale-1 body row (tracks font size,
    /// same ratio the render styles the list with). Heading rows are taller:
    /// `LineElement::height`, positioned by the rows cache's offset table.
    fn line_h(&self) -> Pixels {
        px(self.font_size * 22.0 / 15.0)
    }

    /// The editor viewport height (one base row before first layout).
    fn viewport_h(&self) -> Pixels {
        self.scroll
            .0
            .borrow()
            .last_item_size
            .map_or(self.line_h(), |s| s.item.height)
    }

    /// *Base* rows that fit fully in the viewport (1 before first layout) —
    /// the vim half-page distance. Heading rows are taller, so this
    /// overcounts slightly across them; motions count lines anyway.
    fn viewport_rows(&self, line_h: Pixels) -> usize {
        ((self.viewport_h() / line_h).floor() as usize).max(1)
    }

    /// `Ctrl-E/Y/D/U`: slide the viewport `n` visual rows (negative = up).
    /// `with_caret` (half-page) moves the caret the same distance so it keeps
    /// its screen position; otherwise the caret stays put until the view
    /// would drop it, then it's pulled to the nearest edge (vim's line-scroll
    /// rule). The offset is written directly; the render's caret auto-scroll
    /// only fires when the caret row moved and no-ops while it's visible, so
    /// the two don't fight.
    fn scroll_rows(&mut self, n: isize, with_caret: bool) {
        let Some(c) = &self.rows_cache else { return };
        let cur_row = c.cur_row;
        let offsets = c.offsets.clone();
        let line_h = self.line_h();
        let handle = self.scroll.0.borrow().base_handle.clone();
        let mut off = handle.offset();
        let floor = -handle.max_offset().height;
        // The viewport slides in base-row steps; a taller heading row just
        // takes two of them to clear, like a wrapped line takes two rows.
        off.y = off.y - line_h * n as f32;
        if off.y < floor {
            off.y = floor;
        }
        if off.y > Pixels::ZERO {
            off.y = Pixels::ZERO;
        }
        handle.set_offset(off);
        if with_caret {
            self.move_to_row(cur_row.saturating_add_signed(n));
        } else {
            // First and last fully visible rows at the new offset (a
            // fraction of a row at either edge counts as hidden).
            let (top_y, viewport) = (-off.y, self.viewport_h());
            let top = offsets.partition_point(|&o| o < top_y);
            let bottom = offsets.partition_point(|&o| o <= top_y + viewport).saturating_sub(2);
            let bottom = bottom.max(top);
            if cur_row < top {
                self.move_to_row(top);
            } else if cur_row > bottom {
                self.move_to_row(bottom);
            }
        }
    }

    /// Flip the task box on `line` (normal-mode Enter, a click on the box,
    /// the `toggle-task` command). Checkpoint policy lives here: undo records
    /// only when the line actually has a box, so a stray Enter on a plain
    /// line never pushes a no-op undo step.
    fn toggle_task(&mut self, line: usize) {
        let on_box = self.doc().is_markdown()
            && markdown::task_box(&line_text(&self.doc().rope, line)).is_some();
        if on_box {
            self.doc_mut().checkpoint();
            self.doc_mut().toggle_task(line);
        }
    }

    /// The tab row above the editor: one tab per buffer, `{n}: {basename}`,
    /// `●` when dirty, italic while a preview, muted + struck through when the
    /// backing file has been deleted out from under it. Click switches;
    /// middle-click closes (`:bd` semantics, no force).
    /// The unsaved-changes dialog raised by the titlebar X (`confirm_close`).
    /// Deliberately not a file list: the label names the note when there is one
    /// and counts them otherwise, which keeps the box a fixed size and puts the
    /// three answers — the point of the dialog — where the eye lands.
    fn render_confirm_quit(&self, theme: &Theme) -> Option<impl IntoElement> {
        let p = self.prompt.as_ref()?;
        if !matches!(p.action, PromptAction::ConfirmQuit) {
            return None;
        }
        let key = |k: &str, what: &str| {
            div()
                .flex()
                .gap_2()
                .child(div().text_color(theme.accent).child(k.to_string()))
                .child(div().text_color(theme.foreground).child(what.to_string()))
        };
        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .flex_col()
                .items_center()
                .pt(px(160.))
                .bg(hsla(0., 0., 0., 0.4)) // scrim, as the picker's
                .child(
                    div()
                        .w(px(440.))
                        .flex()
                        .flex_col()
                        .gap_3()
                        .p_4()
                        .font_family(self.ui_font_family.clone())
                        .bg(theme.background)
                        .border_1()
                        .border_color(theme.border)
                        .rounded_lg()
                        .shadow_lg()
                        .child(div().text_color(theme.foreground).child(p.label.clone()))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(key("s", "save all and quit"))
                                .child(key("d", "discard and quit"))
                                .child(key("Esc", "keep editing")),
                        ),
                ),
        )
    }

    fn render_tabline(&self, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let theme = *theme;
        let entity = cx.entity();
        // Tab separator: a translucent slice of `muted` (the inactive-tab text
        // color, legible on the strip in every theme) rather than `border`,
        // which several dark palettes set too close to the strip color to
        // survive at 1px.
        let mut sep = theme.muted;
        sep.a *= 0.4;
        div()
            .flex()
            .flex_row()
            .w_full()
            .font_family(self.ui_font_family.clone())
            .bg(theme.status_background)
            .border_b_1()
            .border_color(theme.border)
            .children(self.buffers.iter().enumerate().flat_map(|(i, b)| {
                let name = b
                    .doc
                    .path()
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "[No Name]".into());
                let dirty = b.doc.is_dirty();
                let missing = b.doc.is_missing();
                let active = i == self.active;
                // Active tab joins the buffer area's background; inactive tabs
                // recede into the (status-colored) strip. A deleted backing
                // file always reads as muted, active or not.
                let (bg, fg) = if active {
                    (theme.background, if missing { theme.muted } else { theme.foreground })
                } else {
                    (theme.status_background, theme.muted)
                };
                let switch = entity.clone();
                let close = entity.clone();
                let tab = div()
                    .px_2()
                    .py_1()
                    .min_w_0()
                    .flex()
                    .flex_row()
                    .items_center()
                    .bg(bg)
                    .text_color(fg)
                    // Inactive tabs carry a transparent border of the same
                    // height so every tab lays out identically.
                    .border_b_2()
                    .border_color(if active { theme.accent } else { hsla(0., 0., 0., 0.) })
                    .when(b.preview, |d| d.italic())
                    .when(missing, |d| d.line_through())
                    .when(!active, |d| d.hover(move |s| s.bg(theme.hover)))
                    .child(
                        // a crowded tab row shrinks tabs, never the layout
                        div().min_w_0().truncate().child(format!("{}: {name}", i + 1)),
                    )
                    // The dirty dot is always laid out — transparent when
                    // clean — so saving never shifts the tab's width.
                    .child(
                        div()
                            .flex_shrink_0()
                            .ml_1()
                            .text_color(if dirty { fg } else { hsla(0., 0., 0., 0.) })
                            .child("●"),
                    )
                    .on_mouse_up(MouseButton::Left, move |_ev: &MouseUpEvent, window, cx| {
                        switch.update(cx, |this, cx| {
                            this.activate(i, window);
                            cx.notify();
                        });
                    })
                    .on_mouse_up(MouseButton::Middle, move |_ev: &MouseUpEvent, window, cx| {
                        close.update(cx, |this, cx| {
                            this.close_buffer(i, false, window);
                            cx.notify();
                        });
                    });
                // A 1px separator after every tab: crowded (truncated) inactive
                // tabs blur together without one, and the trailing separator
                // marks where the last tab ends against the empty strip. A
                // sibling element, not border_r, because a tab's single
                // border_color is taken by the active accent underline.
                [
                    tab.into_any_element(),
                    div().w(px(1.)).flex_shrink_0().bg(sep).into_any_element(),
                ]
            }))
    }

    /// Entry point for every raw `KeyDown`. darknotes drives its own repeat
    /// cadence instead of trusting the OS/platform backend: macOS's native
    /// auto-repeat stays slow even at its fastest setting, and gpui's X11
    /// backend (WSLg included) delivers every X-server repeat pulse looking
    /// like a fresh press. So while `repeat_stroke` is set, a `KeyDown`
    /// recognized as a backend echo is swallowed — `arm_key_repeat`'s timer
    /// drives the cadence instead.
    ///
    /// An echo is recognized by `is_held` first, falling back to a `key`
    /// compare. The fallback exists only for X11, which hardcodes `is_held`
    /// to `false`; it can't be the primary test because macOS folds Shift
    /// into `key` for symbol keys, so releasing Shift mid-hold renames the
    /// still-held key (`:` → `;`) and a `key` match would misread its next
    /// pulse as a fresh press, inserting a stray character.
    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let same_key = self.repeat_stroke.as_ref().is_some_and(|s| s.key == ev.keystroke.key);
        let is_echo = self.repeat_stroke.is_some() && (ev.is_held || same_key);
        if self.key_repeat_interval > 0 && is_echo {
            return;
        }
        self.handle_key(ev, window, cx);
        if self.key_repeat_interval > 0 {
            self.arm_key_repeat(ev.keystroke.clone(), window, cx);
        }
    }

    /// A physical key released: stop repeating. Deliberately doesn't check
    /// *which* key came up — the Shift fold (see `on_key`) means a `KeyUp`
    /// can report a different `key` than the `KeyDown` we stored. Only one
    /// key repeats at a time, so any `KeyUp` is almost certainly its
    /// release; the rare false positive (another key tapped mid-hold) costs
    /// one extra "fresh press" before the next native pulse re-arms it —
    /// far cheaper than a repeat that never stops.
    fn on_key_up(&mut self, _ev: &KeyUpEvent, _window: &mut Window, _cx: &mut Context<Self>) {
        self.repeat_stroke = None;
        self.repeat_timer = None;
    }

    /// (Re)arm `stroke`'s auto-repeat: after `key_repeat_delay`, replay it
    /// through `handle_key` every `key_repeat_interval` until `on_key_up` (or
    /// a window-activation change) clears `repeat_stroke`. Mirrors
    /// `arm_blink`/`arm_fs_watch`'s bridge from gpui's background executor
    /// back onto the entity; replacing `repeat_timer` cancels whatever was
    /// running before (a different key was already repeating).
    fn arm_key_repeat(&mut self, stroke: Keystroke, window: &Window, cx: &mut Context<Self>) {
        self.repeat_stroke = Some(stroke.clone());
        let delay = Duration::from_millis(self.key_repeat_delay);
        let interval = Duration::from_millis(self.key_repeat_interval);
        self.repeat_timer = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(delay).await;
            loop {
                let ev = KeyDownEvent { keystroke: stroke.clone(), is_held: true };
                let alive =
                    this.update_in(cx, |this, window, cx| this.handle_key(&ev, window, cx));
                if alive.is_err() {
                    return;
                }
                cx.background_executor().timer(interval).await;
            }
        }));
    }

    fn handle_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        crate::perf::key(&ev.keystroke.key, window);
        self.message = None; // a fresh keystroke clears the previous result
        self.arm_blink(cx); // caret solid while typing; the phase restarts after

        // The fuzzy picker is modal: while open it swallows every key (Ctrl-W
        // included), so route before any other handling.
        if self.picker.is_some() {
            self.picker_key(ev, window, cx);
            cx.notify();
            return;
        }

        // File-op prompts (create/rename/delete-confirm) are modal the same way.
        if self.prompt.is_some() {
            self.prompt_key(ev, window, cx);
            cx.notify();
            return;
        }

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
                    self.reveal_current();
                }
                "l" => self.pane = Pane::Editor,
                _ => {}
            }
            cx.notify();
            return;
        }
        if m.control && !m.alt && !m.platform && key == "w" {
            self.pending_window = true;
            // A half-typed sidebar op must not survive a pane switch and
            // complete on an unrelated later keystroke.
            self.pending_sidebar = None;
            return;
        }
        if self.pane == Pane::Sidebar {
            self.sidebar_key(key, m.shift, window);
            cx.notify();
            return;
        }

        // Mid-sequence grammar keys (the `f` of `gf`, the target of `dt`)
        // bypass the keymap: bindings match at command start only (vim
        // semantics), so they never wait out `timeoutlen` as a possible chord
        // lead. Skipped while the resolver itself holds buffered keys, so
        // replay order stays first-typed-first.
        if self.vim.in_sequence() && !self.keymap.pending() {
            self.feed_vim(&ev.keystroke, window, cx);
            cx.notify();
            return;
        }

        // The keymap layer: user/default bindings resolved before the vim
        // grammar. Global bindings (Ctrl-chords) are live in every mode;
        // normal/insert bindings gate on the vim mode. Keys no binding claims
        // replay through the grammar unchanged.
        let ctxs: &[Ctx] = match self.vim.mode {
            Mode::Normal => &[Ctx::Global, Ctx::Normal],
            Mode::Insert => &[Ctx::Global, Ctx::Insert],
            _ => &[Ctx::Global],
        };
        let res = self.keymap.feed(ctxs, &ev.keystroke);
        for ks in &res.replay {
            self.feed_vim(ks, window, cx);
        }
        if let Some(name) = res.command {
            self.run_command(&name, window, cx);
        }
        self.arm_seq_timer(window, cx);
        // Mode (hence caret style) can change with no action, so always notify.
        cx.notify();
    }

    /// One keystroke through the vim grammar: checkpoint policy, action
    /// application, search-prompt bookkeeping.
    fn feed_vim(&mut self, ks: &Keystroke, window: &mut Window, cx: &mut Context<Self>) {
        // Checkpoint undo once per undoable unit: before a mutating normal-mode
        // command, or on entering insert (the whole insert session coalesces
        // into that one checkpoint).
        let mode_before = self.vim.mode;
        let mut actions = self.vim.on_key(ks);
        // `.`: splice in the recorded last change. Everything below —
        // checkpoint, register mirror, renumber, re-recording — then treats
        // it exactly like freshly typed input.
        if let [Action::Repeat] = actions[..] {
            actions = self.last_change.clone().unwrap_or_default();
        }
        let entering_insert = mode_before == Mode::Normal && self.vim.mode == Mode::Insert;
        // Checkpoint a single undoable unit. Insert-mode edits are excluded so the
        // whole session coalesces into the entering-insert checkpoint; everything
        // else (normal- and visual-mode mutations) gets its own.
        let mutates = mode_before != Mode::Insert && actions.iter().any(Action::mutates);
        if entering_insert || mutates {
            self.doc_mut().checkpoint();
        }
        // Record for `.`: a completed normal-mode change is repeatable as-is;
        // one that enters insert keeps recording the session (typed text,
        // exit nudge included) until insert exits and seals it.
        // ponytail: visual-mode changes aren't recorded — vim's `.`-on-a-
        // same-sized-region semantics need selection synthesis; add if missed.
        match (mode_before, self.vim.mode) {
            (Mode::Normal, Mode::Insert) => self.pending_change = Some(actions.clone()),
            (Mode::Normal, _) if mutates => self.last_change = Some(actions.clone()),
            (Mode::Insert, mode) => {
                if let Some(rec) = &mut self.pending_change {
                    rec.extend(actions.iter().cloned());
                    if mode == Mode::Normal {
                        self.last_change = self.pending_change.take();
                    }
                }
            }
            _ => {}
        }
        let wrote_register = actions.iter().any(Action::writes_register);
        for action in actions {
            self.apply(action, window, cx);
        }
        // clipboard=unnamed: mirror every register write (yank/delete) out to
        // the system clipboard.
        if wrote_register && !self.doc().register_text().is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(self.doc().register_text().to_owned()));
        }
        // Normal mode disallows the caret one past the line's last char; the
        // shared motions/edits allow it (insert mode appends there), so snap
        // back at this choke point whenever a keystroke lands in normal mode.
        if self.vim.mode == Mode::Normal {
            self.doc_mut().clamp_caret_to_line();
        }
        // After the actions: a submitted search has consumed `origin` in apply,
        // so a leftover origin on prompt close means the prompt was cancelled.
        self.sync_search_prompt(mode_before);
    }

    /// (Re)arm the sequence timeout while binding lead keys are buffered, or
    /// cancel it once the buffer resolves. Each key restarts the clock (vim's
    /// per-key `timeoutlen`); on fire, the buffered keys replay through the
    /// grammar as ordinary input (in insert mode: typed as literal text).
    /// Replacing/clearing the stored `Task` cancels the prior one.
    fn arm_seq_timer(&mut self, window: &Window, cx: &mut Context<Self>) {
        if !self.keymap.pending() {
            self.seq_timer = None;
            return;
        }
        let dur = Duration::from_millis(self.timeoutlen);
        self.seq_timer = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(dur).await;
            this.update_in(cx, |this, window, cx| {
                for ks in this.keymap.flush() {
                    this.feed_vim(&ks, window, cx);
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// The caret's spot, or `None` in an unnamed buffer (nothing to reopen by).
    fn here(&self) -> Option<Pos> {
        Some(Pos {
            path: self.doc().path()?.to_path_buf(),
            line: self.doc().caret_line_col().0,
            offset: self.doc().caret_offset(),
        })
    }

    /// Record the caret's spot as a jump origin. Call *before* the move.
    fn push_jump(&mut self) {
        if let Some(pos) = self.here() {
            self.jumps.push(pos);
        }
    }

    /// `Ctrl-O`/`Ctrl-I`. Silent at either end of the list, like vim.
    fn jump_history(&mut self, back: bool, window: &mut Window) {
        let here = self.here();
        let Some(target) = (if back { self.jumps.back(here) } else { self.jumps.forward() })
        else {
            return;
        };
        self.goto(&target, false, window);
    }

    /// Put the caret on `target`, opening its file if it isn't the active
    /// buffer. `record` pushes the position we leave — off for a jumplist walk,
    /// which must not rewrite the history it is walking.
    ///
    /// A stored position is a snapshot and doesn't track edits above it, so the
    /// offset can be stale: `jump_to` clamps it into the rope, and `feed_vim`'s
    /// normal-mode snap fixes the column.
    fn goto(&mut self, target: &Pos, record: bool, window: &mut Window) {
        if self.doc().path() != Some(target.path.as_path()) {
            match record {
                true => self.open_path(target.path.clone(), window),
                false => self.open_path_quiet(target.path.clone(), window),
            }
        } else if record {
            self.push_jump();
        }
        self.doc_mut().jump_to(target.offset);
    }

    /// `m{a}`. Lowercase is per-buffer (vim: `ma` in two files, two marks);
    /// uppercase carries its file. Anything else is not a mark name.
    fn set_mark(&mut self, name: char) {
        match name {
            'a'..='z' => {
                let at = self.doc().caret_offset();
                self.doc_mut().set_mark(name, at);
            }
            'A'..='Z' => {
                if let Some(pos) = self.here() {
                    self.marks.insert(name, pos);
                }
            }
            _ => self.message = Some(format!("E191: Argument must be a letter: {name}")),
        }
    }

    /// `` `{a} `` / `'{a}`, and `` `` ``/`''` (the position before the latest
    /// jump — the jumplist's newest entry). `line` lands on the target line's
    /// first non-blank instead of its exact column.
    fn jump_to_mark(&mut self, name: char, line: bool, window: &mut Window) {
        let target = match name {
            '\'' | '`' => self.jumps.last().cloned(),
            // A lowercase mark is this buffer's, so it has no path of its own.
            'a'..='z' => self.doc().mark(name).and_then(|offset| {
                Some(Pos { path: self.doc().path()?.to_path_buf(), line: 0, offset })
            }),
            'A'..='Z' => self.marks.get(&name).cloned(),
            _ => {
                self.message = Some(format!("E191: Argument must be a letter: {name}"));
                return;
            }
        };
        let Some(target) = target else {
            self.message = Some(format!("E20: Mark not set: {name}"));
            return;
        };
        self.goto(&target, true, window);
        if line {
            self.doc_mut().move_motion(Motion::FirstNonBlank, 1);
        }
    }

    /// The single execution seam every input grammar funnels through.
    fn apply(&mut self, action: Action, window: &mut Window, cx: &mut Context<Self>) {
        let renumbers = action.renumbers();
        // Record where a jump leaves from, before it moves the caret. Opens by
        // path record in `open_path` instead — see `Action::is_jump`.
        if action.is_jump() {
            self.push_jump();
        }
        match action {
            // In visual mode a motion drags the selection's head; otherwise it
            // just moves the caret.
            Action::Move(m, n) => {
                if self.vim.mode.is_visual() {
                    self.doc_mut().extend_motion(m, n);
                } else {
                    self.doc_mut().move_motion(m, n);
                }
            }
            Action::DeleteSelection { linewise, change } => {
                self.doc_mut().delete_selection(linewise, change)
            }
            Action::YankSelection { linewise } => self.doc_mut().yank_selection(linewise),
            Action::IndentSelection { width, dedent } => {
                self.doc_mut().indent_selection(width, dedent)
            }
            Action::IndentLines { width, dedent, count } => {
                let line = self.doc().caret_line_col().0;
                self.doc_mut().indent_lines(line, line + count - 1, width, dedent);
            }
            Action::MoveDisplay { down } => self.move_display(down),
            Action::CollapseSelection => self.doc_mut().collapse_selection(),
            Action::DeleteMotion(m, n) => self.doc_mut().delete_motion(m, n),
            Action::DeleteLines(n) => self.doc_mut().delete_lines(n),
            Action::DeleteLinesVertical { count, up } => self.doc_mut().delete_lines_dir(count, up),
            Action::DeleteCharUnder(n) => self.doc_mut().delete_char_under(n),
            Action::YankMotion(m, n) => self.doc_mut().yank_motion(m, n),
            Action::YankLines(n) => self.doc_mut().yank_lines(n),
            Action::DeleteObject { obj, change } => self.doc_mut().delete_object(obj, change),
            Action::YankObject(obj) => self.doc_mut().yank_object(obj),
            Action::SelectObject(obj) => self.doc_mut().select_object(obj),
            Action::JoinLines { count, space } => self.doc_mut().join_lines(count, space),
            Action::ReplaceChar(ch, n) => self.doc_mut().replace_char(ch, n),
            Action::ToggleCase(n) => self.doc_mut().toggle_case(n),
            // clipboard=unnamed: an external copy supersedes the internal
            // register. Same content means the register was ours (we mirrored
            // it out), so keep its linewise flag; foreign text guesses linewise
            // from a trailing newline.
            Action::Paste { after } => {
                if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    if text != self.doc().register_text() {
                        let linewise = text.ends_with('\n');
                        self.doc_mut().set_register(text, linewise);
                    }
                }
                self.doc_mut().paste(after)
            }
            Action::InsertText(s) => self.doc_mut().insert(&s),
            Action::Newline { clear_empty } => {
                let width = self.vim.tab_width;
                self.doc_mut().insert_newline(clear_empty, width);
            }
            Action::Tab { width, dedent } => self.doc_mut().indent(width, dedent),
            Action::DeleteBackward => self.doc_mut().delete_backward(),
            Action::DeleteForward => self.doc_mut().delete_forward(),
            Action::Undo => self.doc_mut().undo(),
            // `z` scroll commands don't move the caret, so the render auto-scroll
            // won't override this; `_strict` repositions the already-visible
            // cursor row (plain `scroll_to_item` no-ops when it's on screen).
            // `last_row` is the caret's visual row from the last render — current,
            // because a z-scroll follows a rendered keystroke and moves nothing.
            Action::Scroll(s) => {
                let strategy = match s {
                    Scroll::Center => ScrollStrategy::Center,
                    Scroll::Top => ScrollStrategy::Top,
                    Scroll::Bottom => ScrollStrategy::Bottom,
                };
                self.scroll.scroll_to_item_strict(self.last_row, strategy);
            }
            Action::ScrollLines { down, count } => {
                let n = count as isize;
                self.scroll_rows(if down { n } else { -n }, false);
            }
            Action::ScrollHalf { down } => {
                let n = (self.viewport_rows(self.line_h()) / 2).max(1) as isize;
                self.scroll_rows(if down { n } else { -n }, true);
            }
            Action::ExecuteCommand(cmd) => self.exec_command(&cmd, window, cx),
            Action::Search { query, backward } => self.do_search(query, backward),
            Action::SearchNext { reverse, count } => self.search_next(reverse, count),
            Action::BufferNext => self.buffer_next(window),
            Action::BufferPrev => self.buffer_prev(window),
            Action::FollowLink => self.follow_link(window, cx),
            Action::ToggleTask => {
                let line = self.doc().caret_line_col().0;
                self.toggle_task(line);
            }
            Action::JumpBack => self.jump_history(true, window),
            Action::JumpForward => self.jump_history(false, window),
            Action::SetMark(name) => self.set_mark(name),
            Action::JumpToMark { name, line } => self.jump_to_mark(name, line, window),
            // Expanded into the recorded change in `feed_vim`, before dispatch;
            // never reaches here.
            Action::Repeat => {}
        }
        // A delete can remove or merge list items; the surviving block renumbers.
        if renumbers && self.doc().is_markdown() {
            let (line, _) = self.doc().caret_line_col();
            let width = self.vim.tab_width;
            self.doc_mut().renumber_block(line, width);
        }
    }

    /// `gd`/`gf`/`gx`: follow the link under the caret. A wikilink opens its
    /// note, or a blank named buffer if it doesn't exist yet (created on `:w`,
    /// like `:e`); a `#Heading` fragment then lands on that heading, and a
    /// note-less `[[#Heading]]` stays in the current document. An external URL
    /// opens in the browser. Off a link it's a silent no-op.
    fn follow_link(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (line, col) = self.doc().caret_line_col();
        let text = line_text(&self.doc().rope, line);
        if let Some((note, heading)) = markdown::wikilink_at(&text, col) {
            if note.is_empty() {
                // Same document: no buffer switch to record the origin for us.
                self.push_jump();
            } else {
                match resolve_link(&self.vault.root, &self.vault.files, &note) {
                    Some(path) => self.open_path(path, window),
                    // Nothing to search in a buffer that doesn't exist yet.
                    // `edit` roots a relative name under the vault, so the
                    // target is gated on staying inside it — unlike a typed
                    // `:e`, this name comes from the note.
                    None if vault_relative(&note) => return self.edit(&note, false, window),
                    None => {
                        self.message = Some(format!("link outside vault: {note}"));
                        return;
                    }
                }
            }
            if let Some(heading) = heading {
                match heading_line(&self.doc().rope, &heading) {
                    Some(l) => self.jump_to_line_col(l, 0),
                    None => self.message = Some(format!("no heading: {heading}")),
                }
            }
        } else if let Some(url) = markdown::url_at(&text, col) {
            cx.open_url(&url);
        }
    }

    /// Re-read the vault from disk (a save may have created a new file).
    fn rescan_vault(&mut self) {
        self.vault = Vault::scan(self.vault.root.clone());
        // The tree may have shrunk; keep the sidebar cursor in range.
        let n = self.vault.visible_rows(&self.expanded).len();
        self.selected = self.selected.min(n.saturating_sub(1));
    }

    /// `:w` with no arg writes the backing file; `:w <name>` saves as `<name>`,
    /// defaulting a bare name to `.md`.
    /// Snapshot open buffers to session.toml. Pathless buffers (scratch,
    /// `:enew`) aren't on disk and are skipped; `active` is re-pointed into
    /// the kept list (0 if the active buffer itself was pathless).
    fn save_session(&self) {
        let mut files = Vec::new();
        let mut active = 0;
        for (i, b) in self.buffers.iter().enumerate() {
            let Some(path) = b.doc.path() else { continue };
            if i == self.active {
                active = files.len();
            }
            files.push(session::FileEntry {
                // Canonicalized: the next launch's cwd can differ.
                path: std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()),
                preview: b.preview,
                caret: b.doc.caret_offset(),
            });
        }
        session::record(&self.vault.root, session::VaultSession { active, files });
    }

    fn save(&mut self, arg: Option<&str>) {
        let result = match arg {
            Some(name) => {
                let path = resolve(&self.vault.root, name);
                let display = path.display().to_string();
                let r = self.doc_mut().save_as(path).map(|()| display);
                if r.is_ok() {
                    // A new file may now exist under the vault root — re-scan
                    // so the sidebar shows it.
                    self.rescan_vault();
                }
                r
            }
            None => match self.doc().path().map(|p| p.display().to_string()) {
                Some(display) => {
                    // A buffer created by `:e {new}` has no file on disk yet, so
                    // the vault listing doesn't know it; once the write lands,
                    // re-scan so the sidebar picks it up.
                    let known = self
                        .doc()
                        .path()
                        .is_some_and(|p| self.vault.files.iter().any(|f| f == p));
                    let r = self.doc_mut().save().map(|()| display);
                    if r.is_ok() && !known {
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
        if result.is_ok() {
            // Saving pins a preview tab, edited or not (VS Code behavior).
            self.buffers[self.active].preview = false;
            // `:w` is a natural caret snapshot, and the preview flag changed.
            self.save_session();
        }
        self.message = Some(match result {
            Ok(name) => format!("\"{name}\" written"),
            Err(e) => format!("save failed: {e}"),
        });
    }

    /// Run a submitted `:` command: parse `(name, bang, arg)`, look up the
    /// registry entry by ex alias, dispatch. An arg to a command that takes
    /// none (`:q x`) is not that command → E492, like the unknown case.
    fn exec_command(&mut self, cmd: &str, window: &mut Window, cx: &mut Context<Self>) {
        let cmd = cmd.trim();
        if cmd.is_empty() {
            return;
        }
        let (name, bang, arg) = parse_ex(cmd);
        match COMMANDS.iter().find(|c| c.ex.contains(&name)) {
            Some(c) if c.takes_arg || arg.is_none() => {
                let args = CmdArgs { bang, arg: arg.map(str::to_string) };
                (c.run)(self, &args, window, cx);
            }
            _ => self.message = Some(format!("E492: Not an editor command: {cmd}")),
        }
    }

    /// Dispatch a registry command by `name` (Ctrl-chords), with no bang and
    /// no arg. Unknown names are a bug, not user input; ignored.
    fn run_command(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(c) = COMMANDS.iter().find(|c| c.name == name) {
            (c.run)(self, &CmdArgs { bang: false, arg: None }, window, cx);
        }
    }

    /// A palette pick. `takes_arg` commands have no argument yet, so they
    /// pre-fill the ex prompt (`:e `) instead of running; the rest run directly.
    fn run_picked_command(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(c) = COMMANDS.iter().find(|c| c.name == name) else { return };
        match c.ex.first() {
            Some(ex) if c.takes_arg => self.vim.start_command(&format!("{ex} ")),
            _ => (c.run)(self, &CmdArgs { bang: false, arg: None }, window, cx),
        }
    }
}

impl Focusable for Editor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Editor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::perf::first_frame(window);
        // Commit the preview tab on its first edit. render runs after every
        // notify, so this catches every mutation path (keys, chords, palette
        // picks, timer-replayed sequences) without instrumenting each one.
        if self.doc().is_dirty() {
            self.buffers[self.active].preview = false;
        }

        let theme = *cx.global::<Theme>();

        // Blink phase + focus reach `LineElement::paint` through this shared
        // cell (like `scroll_x`): a blink tick or focus change repaints the
        // caret without touching the cached rows. Unfocused — sidebar has keys
        // or the window lost OS focus — shows a solid dim caret, no blinking.
        self.caret_paint.set(if self.pane != Pane::Editor || !window.is_window_active() {
            CaretPaint::Dim
        } else if self.blink_show {
            CaretPaint::Solid
        } else {
            CaretPaint::Hidden
        });
        // Relative line numbers read this when a row paints, so a caret move
        // re-labels the gutter without rebuilding any row (see `Gutter`).
        self.cur_line.set(self.doc().caret_line_col().0);

        // Soft-wrap width: the live viewport minus the fixed sidebar and the
        // line-number gutter. Live — not last frame's layout — so a resize
        // (including the async maximize at startup) re-wraps correctly within
        // its own frame. `None` turns wrapping off.
        let line_count = self.doc().rope.len_lines();
        // Gutter wide enough for the largest line number, monospace-aligned.
        let num_width = line_count.to_string().len().max(3);
        let wrap_width = if self.wrap {
            // Wrapped rows never overflow — clear any leftover nowrap offset.
            self.scroll_x.set(Pixels::ZERO);
            let gutter_w = if self.line_numbers == LineNumbers::Off {
                Pixels::ZERO
            } else {
                let sample: SharedString = format!(" {line_count:>num_width$}  ").into();
                let font = gpui::font(self.font_family.clone());
                let runs = [run(&font, sample.len(), theme.foreground)];
                window.text_system().shape_line(sample, px(self.font_size), &runs, None).width
            };
            let w = window.viewport_size().width - px(SIDEBAR_WIDTH) - gutter_w;
            (w > Pixels::ZERO).then_some(w)
        } else {
            None
        };
        // Reuse the previous rows when nothing they depend on changed — a
        // wheel scroll re-renders every frame and must not rebuild (much less
        // re-shape) the whole document each time. A caret-only change (j/k,
        // h/l, w/b — the hot path) patches just the affected lines.
        let key = RowsKey {
            revision: self.doc().revision(),
            caret: self.doc().caret_offset(),
            mode: self.vim.mode,
            view: self.vim.view,
            sel: self
                .vim
                .mode
                .is_visual()
                .then(|| self.doc().selection_span(self.vim.mode == Mode::VisualLine)),
            q: self.search_query(),
            wrap_width,
        };
        enum Plan {
            Hit,
            Patch,
            Full,
        }
        // Reparse first: whether a content change was confined to one line is
        // what separates a patchable edit from a whole-document rebuild, and
        // only the parse can establish it (a typed ``` moves a fence boundary
        // and restyles everything below).
        let (_, edited_line) = self.spans_for_render();
        let line_count = self.doc().rope.len_lines();
        let plan = match &self.rows_cache {
            Some(c) if c.key == key => Plan::Hit,
            Some(c) if caret_only_change(&c.key, &key) => Plan::Patch,
            // A one-line edit — an insert-mode keystroke — whose reparse stayed
            // local. The edited line is the only content that differs, so it
            // patches like a caret move. `line_rows` is indexed per logical
            // line below, so its length has to still match.
            Some(c)
                if edited_line.is_some()
                    && c.line_rows.len() == line_count
                    && content_change_is_local(&c.key, &key) =>
            {
                Plan::Patch
            }
            _ => Plan::Full,
        };
        let (lines, cur_row) = match plan {
            Plan::Hit => {
                let c = self.rows_cache.as_ref().unwrap();
                (c.rows.clone(), c.cur_row)
            }
            Plan::Patch => {
                // Same content, no highlights: only the old and new cursor
                // lines — plus the fence lines their enclosing code blocks
                // reveal — can render differently (conceal swap, caret).
                // Rebuild those lines and splice in place. Line numbers are
                // not in that set: the gutter resolves its label at paint time
                // against the shared cursor line, so relative numbering
                // re-labels every row here without touching one.
                let t0 = crate::perf::t0();
                let mut c = self.rows_cache.take().unwrap();
                let ctx = self.row_ctx(wrap_width, &theme, window);
                let old_line =
                    ctx.rope.char_to_line(c.key.caret.min(ctx.rope.len_chars()));
                let new_line = ctx.cur_line;
                let mut rows = Rc::try_unwrap(c.rows).unwrap_or_else(|rc| (*rc).clone());
                let mut caret_in_line = 0;
                // Higher line first, so the lower splice's length change
                // can't shift the row range the higher one was measured at.
                let mut redo = vec![new_line, old_line];
                // The edited line is usually the caret's, but not always — a
                // click on a checkbox toggles a line the caret never visits.
                redo.extend(edited_line);
                redo.extend(ctx.reveal_fences.into_iter().flatten());
                redo.extend(fence_block(&ctx.spans, old_line).into_iter().flatten());
                redo.sort_unstable_by(|a, b| b.cmp(a));
                redo.dedup();
                for &li in &redo {
                    let start: usize =
                        c.line_rows[..li].iter().map(|&n| n as usize).sum();
                    let end = start + c.line_rows[li] as usize;
                    let mut fresh = Vec::new();
                    if let Some(k) = self.append_line_rows(&ctx, li, window, &mut fresh) {
                        caret_in_line = k;
                    }
                    c.line_rows[li] = fresh.len() as u32;
                    rows.splice(start..end, fresh);
                }
                let cur_row = c.line_rows[..new_line].iter().map(|&n| n as usize).sum::<usize>()
                    + caret_in_line;
                let rows = Rc::new(rows);
                let offsets = row_offsets(&rows, self.line_h());
                crate::perf::rows_done(t0, "patch", redo.len(), rows.len());
                self.rows_cache = Some(RowsCache {
                    key,
                    rows: rows.clone(),
                    cur_row,
                    line_rows: c.line_rows,
                    offsets,
                });
                (rows, cur_row)
            }
            Plan::Full => {
                let t0 = crate::perf::t0();
                let ctx = self.row_ctx(wrap_width, &theme, window);
                let line_count = ctx.rope.len_lines();
                let (rows, cur_row, line_rows) = self.build_rows(&ctx, window);
                let rows = Rc::new(rows);
                let offsets = row_offsets(&rows, self.line_h());
                crate::perf::rows_done(t0, "FULL", line_count, rows.len());
                self.rows_cache =
                    Some(RowsCache { key, rows: rows.clone(), cur_row, line_rows, offsets });
                (rows, cur_row)
            }
        };

        // Keep the caret on screen, but only when its row actually moved — so
        // the mouse wheel can scroll freely without snapping back every frame.
        // A buffer switch recenters instead (the shared scroll handle still
        // holds the old buffer's offset). Like vim, landing on a soft-wrapped
        // line pulls the whole line into view, not just the caret's row: the
        // scroll target is the line's last visual row moving down, its first
        // moving up, clamped (in pixels, rows vary in height) so the caret
        // itself stays visible when a single line runs taller than the
        // viewport. One scroll_to_item call only — gpui keeps a single
        // deferred scroll per frame, last call wins.
        let line_h = self.line_h();
        if self.center_on_render {
            self.scroll.scroll_to_item_strict(cur_row, ScrollStrategy::Center);
            self.center_on_render = false;
        } else if cur_row != self.last_row {
            let viewport = self.viewport_h();
            let c = self.rows_cache.as_ref().unwrap();
            let line = self.doc().rope.char_to_line(c.key.caret);
            let first: usize = c.line_rows[..line].iter().map(|&n| n as usize).sum();
            let last = first + c.line_rows[line] as usize - 1;
            let offs = &c.offsets;
            if cur_row > self.last_row {
                // Deepest row whose bottom edge keeps the caret row's top
                // within one viewport when scrolled to the bottom.
                let deep = offs
                    .partition_point(|&o| o <= offs[cur_row] + viewport)
                    .saturating_sub(2);
                let target = last.min(deep.max(cur_row));
                self.scroll.scroll_to_item(target, ScrollStrategy::Bottom);
            } else {
                // Shallowest row whose top edge keeps the caret row's bottom
                // within one viewport when scrolled to the top.
                let shallow =
                    offs.partition_point(|&o| o < offs[cur_row + 1] - viewport);
                let target = first.max(shallow.min(cur_row));
                self.scroll.scroll_to_item(target, ScrollStrategy::Top);
            }
        }
        self.last_row = cur_row;

        // Keep the sidebar cursor on screen as `j`/`k` move it past the viewport.
        // Only while the sidebar drives keys, so a click (which switches to the
        // editor) and the mouse wheel don't snap it back. Direction picks the edge
        // like the editor above: down lands at the bottom, up at the top.
        if self.selected != self.last_selected {
            if self.pane == Pane::Sidebar {
                let strategy = if self.selected > self.last_selected {
                    ScrollStrategy::Bottom
                } else {
                    ScrollStrategy::Top
                };
                self.sidebar_scroll.scroll_to_item(self.selected, strategy);
            }
            self.last_selected = self.selected;
        }

        // Keep the picker highlight on screen. scroll_to_item no-ops while the
        // item is visible, so this only fires when arrowing past the viewport.
        if let Some(p) = &self.picker {
            self.picker_scroll.scroll_to_item(p.selected, ScrollStrategy::Center);
        }

        let mode = self.vim.mode;

        // Mode reads as a colored pill; command mode keeps the raw `:` prompt
        // and a file-op prompt shows its hint instead. The filename (and dirty
        // flag) live in the tabline.
        let (pill, bar) = if let Some(p) =
            self.prompt.as_ref().filter(|p| !matches!(p.action, PromptAction::ConfirmQuit))
        {
            // Create/rename input renders inline in the tree; this is a hint.
            // The quit confirmation is excluded — its dialog asks in full, and
            // repeating the question down here would just read as an echo.
            (None, p.label.clone())
        } else if mode == Mode::Command {
            (None, format!("{}{}", self.vim.prompt(), self.vim.command_line()))
        } else {
            let pill = match mode {
                Mode::Insert => ("INSERT", theme.mode_insert),
                Mode::Visual => ("VISUAL", theme.mode_visual),
                Mode::VisualLine => ("VISUAL LINE", theme.mode_visual),
                // The read-only posture reads as its own mode, whatever the
                // grammar mode underneath.
                _ if self.vim.view => ("VIEW", theme.mode_view),
                _ => ("NORMAL", theme.accent),
            };
            (Some(pill), self.message.clone().unwrap_or_default())
        };

        let open_path = self.doc().path().map(Path::to_path_buf);
        let tabline = self.render_tabline(&theme, cx);
        let trash_dir = self.vault.root.join(".trash");
        let mut rows = self.vault.visible_rows(&self.expanded);
        // Inline file-op editing: a create prompt renders as a phantom row at
        // its insertion point, a rename replaces its row's label with the
        // input. The edited row takes over the cursor highlight.
        let mut edit_row = None;
        match &self.prompt {
            Some(FilePrompt { action: PromptAction::Create { at, depth, .. }, input, .. }) => {
                let at = (*at).min(rows.len());
                rows.insert(
                    at,
                    Row {
                        depth: *depth,
                        name: input.clone(),
                        path: PathBuf::new(),
                        is_dir: false,
                        expanded: false,
                    },
                );
                edit_row = Some(at);
            }
            Some(FilePrompt { action: PromptAction::Rename { target }, input, .. }) => {
                if let Some(i) = rows.iter().position(|r| r.path == *target) {
                    rows[i].name = input.clone();
                    edit_row = Some(i);
                }
            }
            _ => {}
        }
        let cursor =
            (self.pane == Pane::Sidebar && edit_row.is_none()).then_some(self.selected);
        let caret_h = px(self.font_size);
        let row_count = rows.len();
        let entity = cx.entity();

        div()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .on_key_up(cx.listener(Self::on_key_up))
            .size_full()
            .relative() // positioning context for the switcher overlay
            .flex()
            .bg(theme.background)
            .text_color(theme.foreground)
            // Font from config. A real installed family matters: GPUI's default
            // triggers per-line fallback scanning when absent (~8ms/cold line).
            // ponytail: line_height tracks font_size at a fixed ~1.47 ratio (22px
            // at the 15px default); expose it as its own config key only if asked.
            .font_family(self.font_family.clone())
            .text_size(px(self.font_size))
            .line_height(line_h)
            .child(
                uniform_list("sidebar", row_count, move |range, _win, _cx| {
                    range
                        .map(|i| {
                            let row = &rows[i];
                            let is_open_file =
                                open_path.as_deref() == Some(row.path.as_path());
                            let editing = edit_row == Some(i);
                            // The edited row outranks the sidebar cursor
                            // (suppressed while editing) and open-file highlight.
                            let (bg, fg) = if editing || cursor == Some(i) {
                                (theme.sidebar_cursor_background, theme.sidebar_active_foreground)
                            } else if is_open_file {
                                (theme.sidebar_current_background, theme.sidebar_active_foreground)
                            } else if row.path.starts_with(&trash_dir) {
                                // Trash and everything in it read grayed-out:
                                // in limbo, not real notes.
                                (theme.sidebar_background, theme.muted)
                            } else {
                                (theme.sidebar_background, theme.sidebar_foreground)
                            };
                            // Icon by kind. `.md` is keyed on the path, not the
                            // display name (which strips the implied `.md`).
                            let icon = if row.is_dir || row.name.ends_with('/') {
                                // A trailing `/` in a create prompt commits as a
                                // folder, so it previews as one.
                                if row.path == trash_dir {
                                    "icons/trash.svg"
                                } else {
                                    "icons/folder.svg"
                                }
                            } else if row.path.as_os_str().is_empty() {
                                // Phantom create row: a bare name commits as a
                                // `.md` note; an explicit extension is kept.
                                if Path::new(&row.name).extension().is_some_and(|e| e != "md") {
                                    "icons/file-code.svg"
                                } else {
                                    "icons/file-text.svg"
                                }
                            } else if row.path.extension().is_some_and(|e| e == "md") {
                                "icons/file-text.svg"
                            } else {
                                "icons/file-code.svg"
                            };
                            // Indent one step per tree level; px_2 (8px) is the base.
                            let indent = 8. + row.depth as f32 * 14.;
                            let path = row.path.clone();
                            let is_dir = row.is_dir;
                            let entity = entity.clone();
                            // Bg goes on every row (on plain rows it matches the
                            // pane so it's invisible) — highlights span the full
                            // sidebar width, text never shifts, and uniform_list
                            // keeps its one row height (py: ~26px).
                            //
                            // The row is block layout, not flex: gpui only
                            // ellipsizes text whose div gets a definite width at
                            // measure time, and flex items are measured
                            // content-first, painting that untruncated layout.
                            // Chevron and file icon are absolutely positioned
                            // over the row's left padding instead of being flex
                            // siblings.
                            let icon_top = px(2.) + (line_h - px(14.)) * 0.5;
                            let base = div()
                                .w_full()
                                .relative()
                                // indent + chevron (14) + gap (4) + icon (14) + gap (4)
                                .pl(px(indent + 36.))
                                .pr_2()
                                .py(px(2.))
                                .bg(bg)
                                .text_color(fg)
                                // Chevron: disclosure for folders, absent for
                                // files (the fixed padding keeps names aligned
                                // under sibling folder names). svg() paints only
                                // with its own text color set; it doesn't
                                // inherit the row's.
                                .children(row.is_dir.then(|| {
                                    let p = if row.expanded {
                                        "icons/chevron-down.svg"
                                    } else {
                                        "icons/chevron-right.svg"
                                    };
                                    svg()
                                        .path(p)
                                        .absolute()
                                        .left(px(indent))
                                        .top(icon_top)
                                        .size(px(14.))
                                        .text_color(fg)
                                }))
                                .child(
                                    svg()
                                        .path(icon)
                                        .absolute()
                                        .left(px(indent + 18.))
                                        .top(icon_top)
                                        .size(px(14.))
                                        .text_color(fg),
                                )
                                .child(if editing {
                                    // Renaming: the caret must stay visible, so
                                    // overflow clips on the left — the text
                                    // slides toward the icons as it grows, like
                                    // Zed's rename input. justify_end pins the
                                    // content's right edge (with the caret) to
                                    // the row's; min_w_full makes short names
                                    // span the full row so justify_end has
                                    // nothing to shift and they stay
                                    // left-aligned.
                                    div().overflow_hidden().flex().justify_end().child(
                                        div()
                                            .min_w_full()
                                            .flex_shrink_0()
                                            .flex()
                                            .items_center()
                                            .child(row.name.clone())
                                            .child(div().w(px(2.)).h(caret_h).bg(fg)),
                                    )
                                } else {
                                    // Ellipsize long names. A block child fills
                                    // the row's content box, so the text's first
                                    // measure sees the real width.
                                    div().truncate().child(row.name.clone())
                                });
                            // Hover wash only where no highlight would be hidden
                            // by it.
                            let hoverable = !editing && cursor != Some(i) && !is_open_file;
                            let base =
                                base.when(hoverable, |d| d.hover(move |s| s.bg(theme.hover)));
                            base.on_mouse_up(
                                MouseButton::Left,
                                move |_ev: &MouseUpEvent, window, cx| {
                                    let path = path.clone();
                                    entity.update(cx, |this, cx| {
                                        // Click-away cancels an open inline edit;
                                        // a click on the edited row does nothing.
                                        if this.prompt.is_some() {
                                            if !editing {
                                                this.prompt = None;
                                            }
                                            cx.notify();
                                            return;
                                        }
                                        this.selected = i;
                                        if is_dir {
                                            this.toggle(path);
                                        } else {
                                            this.open_path(path, window);
                                            this.pane = Pane::Editor;
                                        }
                                        cx.notify();
                                    });
                                },
                            )
                        })
                        .collect::<Vec<_>>()
                })
                .track_scroll(self.sidebar_scroll.clone())
                .w(px(330.))
                .h_full()
                .flex_shrink_0() // never let a wide editor pane squeeze the sidebar
                .font_family(self.ui_font_family.clone())
                .bg(theme.sidebar_background)
                .border_r_1()
                .border_color(theme.border),
            )
            .child(
                div()
                    .flex_1()
                    // A flex item's min width is its content's min-content width, so
                    // an unwrapped status line (long name/message) would push the row
                    // wider than the window and shove the sidebar off. Allow shrink to
                    // 0 and clip the status line instead.
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(tabline)
                    .child(
                        row_list(
                            "lines",
                            lines,
                            self.rows_cache.as_ref().unwrap().offsets.clone(),
                            line_h,
                        )
                        .track_scroll(self.scroll.clone())
                        .flex_1()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                                this.click_to_caret(ev.position, window, cx);
                            }),
                        ),
                    )
                    .child(
                        div()
                            .w_full()
                            .px_2()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .font_family(self.ui_font_family.clone())
                            .bg(theme.status_background)
                            .text_color(theme.status_foreground)
                            .border_t_1()
                            .border_color(theme.border)
                            .children(pill.map(|(label, color)| {
                                // All-caps label: the font reserves descent space
                                // below the baseline but caps have no descenders,
                                // so glyphs sit high in the inherited 22px line
                                // box. A tight line box (chip inset in the bar,
                                // centered by items_center) plus 1px top pad
                                // rebalances the gaps.
                                div()
                                    .px_2()
                                    .line_height(px(self.font_size + 2.))
                                    .pt(px(1.))
                                    .rounded_sm()
                                    .bg(color)
                                    .text_color(theme.background)
                                    .child(label)
                            }))
                            .child(
                                // Clip an over-long message, don't grow the layout.
                                div().min_w_0().truncate().child(bar),
                            ),
                    ),
            )
            .children(self.render_picker(&theme))
            .children(self.render_confirm_quit(&theme))
    }
}

/// Line of the first heading titled `name`, case-insensitively and ignoring the
/// `#` markers — the target of a `[[note#Heading]]` link. Duplicate titles
/// resolve to the first, matching how a duplicate note stem resolves.
// ponytail: ASCII-only case folding, like wikilink note resolution. Whole-line
// scan, fine at note scale; index headings per buffer if it ever isn't.
fn heading_line(rope: &ropey::Rope, name: &str) -> Option<usize> {
    (0..rope.len_lines()).find(|&i| {
        markdown::heading_text(&line_text(rope, i)).is_some_and(|t| t.eq_ignore_ascii_case(name))
    })
}

/// Invert a conceal source→display byte map: the source byte displayed at
/// `display_byte`. Dropped marker bytes collapse onto the display position of
/// the next kept byte, so among equal map entries the last one is the kept
/// char — the one a click on that display position means.
fn display_to_source(map: &[usize], display_byte: usize) -> usize {
    map.partition_point(|&m| m <= display_byte).saturating_sub(1)
}

#[cfg(test)]
mod tests {
    use super::display_to_source;
    use crate::markdown;
    use ropey::Rope;

    #[test]
    fn display_to_source_lands_on_kept_bytes() {
        // "# Title" conceals to "Title": clicking display 0 must land on 'T'
        // (source 2), not the dropped '#'; each later display byte maps 1:1;
        // one past display end maps one past source end.
        let rope = Rope::from_str("# Title\n");
        let spans = markdown::parse(&rope);
        let segs = markdown::flatten("# Title".len(), &spans[0]);
        let c = markdown::conceal("# Title", &segs);
        assert_eq!(c.text, "Title");
        assert_eq!(display_to_source(&c.map, 0), 2);
        assert_eq!(display_to_source(&c.map, 4), 6);
        assert_eq!(display_to_source(&c.map, 5), 7);
    }
}
