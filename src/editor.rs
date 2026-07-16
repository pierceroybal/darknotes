mod command;

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    div, fill, hsla, outline, point, prelude::*, px, relative, size, svg, uniform_list, App,
    BorderStyle, Bounds,
    ClipboardItem, ContentMask, Context, Corners, Div, Edges, FocusHandle, Focusable, Font,
    FontId, FontStyle,
    FontWeight,
    GlobalElementId, GlyphId, Hsla, InspectorElementId, KeyDownEvent, KeyUpEvent, Keystroke,
    LayoutId,
    MouseButton, MouseUpEvent, Pixels, ScrollStrategy, ShapedLine, SharedString, Style, Task,
    TextRun, TransformationMatrix, UniformListScrollHandle, Window,
};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config as NucleoConfig, Matcher, Utf32Str};

use crate::config::{Config, LineNumbers, Search as SearchConfig};
use crate::document::Document;
use crate::keymap::{Ctx, Resolver};
use command::{parse_ex, CmdArgs, COMMANDS, DEFAULT_BINDINGS};
use crate::markdown::{self, Segment, Span, SpanKind};
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

/// One open buffer: a `Document` plus tab metadata.
struct Buffer {
    doc: Document,
    /// VS Code-style preview tab: the next file open replaces this buffer
    /// instead of adding a tab; the first edit commits it (clears the flag).
    /// At most one preview buffer exists at a time.
    preview: bool,
}

/// What a picker row resolves to on Enter. New picker kinds add a variant
/// (jump-to-heading → `Line(usize)`).
enum PickItem {
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
struct Picker {
    /// Query-line prefix: `"> "` for files, `": "` for commands.
    title: &'static str,
    /// `(display, payload)` for every candidate, built once on open.
    items: Vec<(String, PickItem)>,
    query: String,
    /// Matches as indices into `items`, best-first; every index, in order,
    /// when the query is empty.
    results: Vec<usize>,
    selected: usize,
}

/// A sidebar file-op prompt — modal like the picker. Create and rename edit
/// inline in the tree: the input renders as the row being created/renamed
/// (vim's open-line, for files) with `label` as a status-line hint. The
/// delete confirmation lives in the status line alone. Printable keys extend
/// `input`, Backspace trims, Enter applies via `action`, Esc — or clicking
/// another row — cancels.
struct FilePrompt {
    /// Status-line text: a hint (`"New in notes/"`) while editing inline,
    /// the whole question for the delete confirmation.
    label: String,
    input: String,
    action: PromptAction,
}

/// What Enter does with a finished file prompt.
enum PromptAction {
    /// Create `input` inside `dir`: a folder on a trailing `/`, else a file
    /// (bare names default to `.md`). `at`/`depth` place the phantom input
    /// row in the sidebar list while typing.
    Create { dir: PathBuf, at: usize, depth: usize },
    /// Rename `target` (whose row renders the input in place) to `input`
    /// within its folder.
    Rename { target: PathBuf },
    /// Permanently delete `target` (already inside `.trash`) on `y`.
    ConfirmDelete { target: PathBuf },
}

/// Everything the editor's row list is built from, beyond session constants
/// (font, theme, gutter mode). Equal keys ⇒ identical rows, so render reuses
/// the cached build. `revision` values are process-unique per content state,
/// so buffer switches and `:e` reloads can't collide.
#[derive(PartialEq)]
struct RowsKey {
    revision: u64,
    caret: usize,
    mode: Mode,
    /// Visual-mode selection span (`None` outside visual mode).
    sel: Option<(usize, usize)>,
    /// The query whose matches are highlighted (incsearch preview or lit
    /// hlsearch); empty = none.
    q: String,
    wrap_width: Option<Pixels>,
}

/// The last row build: what it was built from, its rows, the caret's row,
/// and how many rows each logical line produced — the caret fast path uses
/// the per-line counts to splice single lines instead of rebuilding.
struct RowsCache {
    key: RowsKey,
    rows: Rc<Vec<LineElement>>,
    cur_row: usize,
    line_rows: Vec<u32>,
}

/// Inputs to `append_line_rows` that are uniform across lines within one
/// build, bundled so the caret fast path can rebuild single lines without
/// rerunning a whole `build_rows` pass.
struct RowCtx {
    rope: ropey::Rope, // ropey clone is cheap (shared, CoW)
    spans: Rc<Vec<Vec<markdown::Span>>>,
    theme: Theme,
    font: Font,
    font_size: Pixels,
    wrap_width: Option<Pixels>,
    /// Columns per row when the font probed monospace; the plain-ASCII
    /// column-walk wrap path.
    mono_cols: Option<usize>,
    /// `mono_cols` shrunk by the `CODE_MARGIN + CODE_PAD` text inset — the
    /// column budget for code-band lines, which wrap inside the band's border.
    mono_band_cols: Option<usize>,
    mode: Mode,
    cur_line: usize,
    cur_col: usize,
    /// Fence lines of the block the caret sits in, revealed along with the
    /// cursor line so the whole block reads as one unit while edited inside.
    reveal_fences: [Option<usize>; 2],
    sel_span: Option<(usize, usize)>,
    search_ranges: Vec<(usize, usize)>,
    num_width: usize,
}

/// Wrap boundaries for lines that need real shaping (non-ASCII, tabs, bold
/// spans), cached across row rebuilds. gpui's own layout cache only survives
/// frame to frame — and memoized frames don't shape — so leaning on it meant
/// re-platform-shaping every such line on every caret move (visible j/k
/// stutter). Two generations, rotated once per *full* build: an entry unused
/// for one whole build is dropped. A width change clears everything, since
/// boundaries depend on it.
#[derive(Default)]
struct ShapeWrapCache {
    width: Pixels,
    cur: HashMap<(String, Vec<Segment>), Vec<usize>>,
    prev: HashMap<(String, Vec<Segment>), Vec<usize>>,
}

impl ShapeWrapCache {
    /// Start a full build: rotate generations, or clear on width change.
    fn begin(&mut self, width: Pixels) {
        if width != self.width {
            self.width = width;
            self.cur.clear();
            self.prev.clear();
        } else {
            self.prev = std::mem::take(&mut self.cur);
        }
    }

    /// Look up boundaries, promoting a previous-generation hit.
    fn get(&mut self, key: &(String, Vec<Segment>)) -> Option<Vec<usize>> {
        if let Some(v) = self.cur.get(key) {
            return Some(v.clone());
        }
        let v = self.prev.remove(key)?;
        self.cur.insert(key.clone(), v.clone());
        Some(v)
    }
}

/// The last search and the live prompt state. Lives on the editor, not the
/// document — like vim's search register it spans buffer switches.
#[derive(Default)]
struct SearchState {
    /// Last submitted query (the `n`/`N` target); empty = no search yet.
    query: String,
    /// Direction of the last search; `n` follows it, `N` reverses it.
    backward: bool,
    /// hlsearch is lit; `:noh` clears it until the next search or `n`/`N`.
    hl: bool,
    /// Caret when the `/`/`?` prompt opened — `Some` only while it's open.
    /// Incremental jumps preview from here; cancelling restores to it.
    origin: Option<usize>,
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
    /// Markdown parse of the active buffer, memoized on its revision (the
    /// parse is document-wide and caret-independent).
    spans_cache: Option<(u64, Rc<Vec<Vec<markdown::Span>>>)>,
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
    /// `key_repeat_delay`/`key_repeat_interval` (ms), from config. darknotes
    /// drives its own repeat cadence for held keys rather than trusting the
    /// OS/platform backend — see `on_key`. `key_repeat_interval == 0`
    /// disables this and falls back to raw passthrough of whatever repeat
    /// events the backend generates natively.
    key_repeat_delay: u64,
    key_repeat_interval: u64,
    /// The keystroke we're currently auto-repeating, or `None`. Cleared by
    /// `on_key_up` (or on window deactivation, so alt-tabbing away mid-hold
    /// can't strand it set). Comparing incoming `KeyDown`s against this is
    /// what lets `on_key` tell an OS repeat-echo from a deliberate second
    /// press — the latter is always preceded by a `KeyUp` — even on backends
    /// like X11 that never set `KeyDownEvent::is_held`.
    repeat_stroke: Option<Keystroke>,
    /// Pending repeat cadence. Replacing/clearing cancels the timer (gpui
    /// cancels a dropped `Task`), like `blink_timer`/`seq_timer`.
    repeat_timer: Option<Task<()>>,
    /// Open fuzzy picker, or `None`. Routes keys when `Some`.
    picker: Option<Picker>,
    /// Drives the picker results list scroll (scroll-to-selected).
    picker_scroll: UniformListScrollHandle,
    /// `/`-search state (`/`, `?`, `n`, `N`, hlsearch).
    search: SearchState,
    /// Search options from config (ignorecase, hlsearch, …).
    search_cfg: SearchConfig,
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
        // Also drop any in-flight key-repeat: alt-tabbing away mid-hold can
        // lose the matching `KeyUp`, which would otherwise strand
        // `repeat_stroke` set and silently swallow that key's next real press.
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
            search: SearchState::default(),
            search_cfg: config.search,
            cursor_blink: config.cursor_blink,
            blink_interval: config.cursor_blink_interval,
            blink_show: true,
            blink_timer: None,
            caret_paint: Rc::new(Cell::new(CaretPaint::Solid)),
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

    fn doc(&self) -> &Document {
        &self.buffers[self.active].doc
    }

    fn doc_mut(&mut self) -> &mut Document {
        &mut self.buffers[self.active].doc
    }

    /// Open a file by path (sidebar click / Enter / switcher / `:e`): switch
    /// to its buffer if one is already open, else show it in the preview slot.
    fn open_path(&mut self, path: PathBuf, window: &mut Window) {
        if let Some(i) = self.buffers.iter().position(|b| b.doc.path() == Some(path.as_path())) {
            self.activate(i, window);
            return;
        }
        self.show_preview(open_or_empty(&path), window);
    }

    /// Expand or collapse a folder, then clamp the cursor — collapsing drops the
    /// rows beneath it, so `selected` can land past the end.
    fn toggle(&mut self, dir: PathBuf) {
        if !self.expanded.remove(&dir) {
            self.expanded.insert(dir);
        }
        let n = self.vault.visible_rows(&self.expanded).len();
        self.selected = self.selected.min(n.saturating_sub(1));
    }

    /// Make buffer `i` the active one: reset vim/keymap state, restore its
    /// view, and refocus the editor so keys keep flowing after a click or
    /// command. Leaves `alternate` untouched — `activate` (a user-facing
    /// switch) records that.
    fn switch_to(&mut self, i: usize, window: &mut Window) {
        self.active = i;
        self.vim.reset(); // clears transient state, keeps config (tab width)
        self.keymap.clear(); // a pending binding sequence dies with the buffer
        self.seq_timer = None;
        // A mid-prompt buffer switch (Ctrl-P) must not restore a stale caret
        // into the new buffer. The query itself survives — vim search is global.
        self.search.origin = None;
        // The scroll handle is shared across buffers and still holds the old
        // offset; recenter on this buffer's own caret. Deferred to the render
        // pass — the caret's visual row needs this buffer's wrap map.
        self.center_on_render = true;
        self.reveal_current();
        window.focus(&self.focus);
        self.save_session();
    }

    /// Switch to buffer `i`, recording where we came from for `Ctrl-6`/`:b #`.
    fn activate(&mut self, i: usize, window: &mut Window) {
        if i != self.active {
            self.alternate = Some(self.active);
        }
        self.switch_to(i, window);
    }

    /// Show `doc` in the preview slot: replace the existing preview buffer, or
    /// append a new preview tab.
    fn show_preview(&mut self, doc: Document, window: &mut Window) {
        let buf = Buffer { doc, preview: true };
        match self.buffers.iter().position(|b| b.preview) {
            Some(i) => {
                self.buffers[i] = buf;
                self.activate(i, window);
            }
            None => {
                self.buffers.push(buf);
                self.activate(self.buffers.len() - 1, window);
            }
        }
    }

    /// Reveal the open buffer's file in the sidebar: expand collapsed ancestors,
    /// park the sidebar cursor on its row, and scroll it into view. A pathless
    /// buffer (or one outside the vault) parks the cursor at the top.
    fn reveal_current(&mut self) {
        self.selected = 0;
        if let Some(path) = self.doc().path().map(Path::to_path_buf) {
            expand_ancestors(&self.vault.root, &path, &mut self.expanded);
            if let Some(i) = self
                .vault
                .visible_rows(&self.expanded)
                .iter()
                .position(|r| r.path == path)
            {
                self.selected = i;
            }
        }
        // Scrolled here (centered, no-op if already visible), so the render
        // pass's edge-scroll must not fire again.
        self.last_selected = self.selected;
        self.sidebar_scroll.scroll_to_item(self.selected, ScrollStrategy::Center);
    }

    /// Guard before discarding the active buffer's changes (the in-place `:e`
    /// reload): `true` if it's safe, else sets the vim E37 message and returns
    /// `false`. `bang` (`:e!`) forces it through.
    fn may_discard(&mut self, bang: bool) -> bool {
        if self.doc().is_dirty() && !bang {
            self.message = Some("E37: No write since last change (add ! to override)".into());
            false
        } else {
            true
        }
    }

    /// `:set {option}` — vim option toggles. Only 'wrap' exists so far; grow
    /// this into an option table when the second option arrives.
    fn set_option(&mut self, arg: Option<&str>) {
        match arg {
            Some("wrap") => self.wrap = true,
            Some("nowrap") => self.wrap = false,
            Some("wrap!") | Some("invwrap") => self.wrap = !self.wrap,
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

    /// The query whose matches should be highlighted right now: the pending
    /// prompt text while a `/`/`?` search is being typed (incsearch preview),
    /// else the last submitted query while hlsearch is lit. Empty = none.
    fn search_query(&self) -> String {
        let prompt_open = self.vim.mode == Mode::Command && self.vim.prompt() != ':';
        if prompt_open && self.search_cfg.incsearch {
            self.vim.command_line().to_string()
        } else if !prompt_open && self.search_cfg.hlsearch && self.search.hl {
            self.search.query.clone()
        } else {
            String::new()
        }
    }

    /// Per-line markdown spans of the active buffer, memoized on its content
    /// revision (the parse is document-wide and caret-independent). Non-markdown
    /// buffers get empty spans per line — plain text, no conceal/styling.
    fn spans(&mut self) -> Rc<Vec<Vec<markdown::Span>>> {
        let rev = self.doc().revision();
        match &self.spans_cache {
            Some((r, s)) if *r == rev => s.clone(),
            _ => {
                let s = if self.doc().is_markdown() {
                    Rc::new(markdown::parse(&self.doc().rope))
                } else {
                    Rc::new(vec![Vec::new(); self.doc().rope.len_lines()])
                };
                self.spans_cache = Some((rev, s.clone()));
                s
            }
        }
    }

    /// Assemble the per-build inputs shared by every line. `window` shapes
    /// the two one-glyph monospace probes.
    fn row_ctx(&mut self, wrap_width: Option<Pixels>, theme: &Theme, window: &mut Window) -> RowCtx {
        let spans = self.spans();
        let rope = self.doc().rope.clone(); // ropey clone is cheap (shared, CoW)
        let mode = self.vim.mode;
        let (cur_line, cur_col) = self.doc().caret_line_col();
        // The selected char-range to highlight, `None` outside visual mode.
        let sel_span: Option<(usize, usize)> =
            mode.is_visual().then(|| self.doc().selection_span(mode == Mode::VisualLine));
        let q = self.search_query();
        let search_ranges = if q.is_empty() {
            Vec::new()
        } else {
            find_matches(&rope, &q, search_sensitive(&q, &self.search_cfg))
        };

        // Shaping font, matching what `LineElement` resolves from the window's
        // text-style cascade so wrap boundaries agree with the painted rows.
        let font = gpui::font(self.font_family.clone());
        let font_size = px(self.font_size);

        // Monospace fast path: when every glyph advances the same, wrap
        // boundaries are a pure column walk — no platform shaping, which is
        // what made opening/editing long-lined docs drag. Probe the font once
        // ('i' and 'M' advance alike ⇒ monospace); the per-line gate in
        // `append_line_rows` keeps the exact shaped path for anything the
        // walk can't promise.
        let mono: Option<(usize, usize)> = wrap_width.and_then(|w| {
            let advance = |s: &'static str| {
                let runs = [run(&font, 1, theme.foreground)];
                window.text_system().shape_line(s.into(), font_size, &runs, None).width
            };
            let (iw, mw) = (advance("i"), advance("M"));
            ((iw - mw).abs() < px(0.01) && iw > Pixels::ZERO).then(|| {
                let cols = |w: Pixels| ((w / iw) as usize).max(1);
                (cols(w), cols(w - (CODE_MARGIN + CODE_PAD) * 2.))
            })
        });

        let num_width = rope.len_lines().to_string().len().max(3);
        let reveal_fences = fence_block(&spans, cur_line);
        RowCtx {
            rope,
            spans,
            theme: *theme,
            font,
            font_size,
            wrap_width,
            mono_cols: mono.map(|(c, _)| c),
            mono_band_cols: mono.map(|(_, c)| c),
            mode,
            cur_line,
            cur_col,
            reveal_fences,
            sel_span,
            search_ranges,
            num_width,
        }
    }

    /// One `LineElement` per *visual row* of the buffer, the caret's row
    /// index, and each logical line's row count. With soft-wrap on, a logical
    /// line becomes one element per wrapped row, its display text, styling
    /// segments, highlights, and caret sliced to each row. `LineElement`
    /// stays a fixed-height single row, which is what keeps `uniform_list`'s
    /// virtualization valid.
    ///
    /// Runs only when a `RowsKey` input changed beyond a caret move (render
    /// memoizes and caret moves patch single lines via `append_line_rows`).
    // ponytail: a full rebuild walks the whole doc (conceal + slice, ≈ per
    // edit keystroke). If typing in a huge doc ever bites, cache rows per
    // line keyed on (text, segments) like `ShapeWrapCache`.
    fn build_rows(
        &mut self,
        ctx: &RowCtx,
        window: &mut Window,
    ) -> (Vec<LineElement>, usize, Vec<u32>) {
        // Full build = one cache generation for the shaped wrap boundaries.
        self.wrap_cache.begin(ctx.wrap_width.unwrap_or(Pixels::ZERO));
        let line_count = ctx.rope.len_lines();
        let mut rows = Vec::with_capacity(line_count);
        let mut line_rows = Vec::with_capacity(line_count);
        let mut cur_row = 0;
        for i in 0..line_count {
            let base = rows.len();
            if let Some(k) = self.append_line_rows(ctx, i, window, &mut rows) {
                cur_row = base + k;
            }
            line_rows.push((rows.len() - base) as u32);
        }
        (rows, cur_row, line_rows)
    }

    /// Build logical line `i`'s visual rows into `out`, returning the caret's
    /// index within the appended rows when `i` is the cursor line. The whole
    /// per-line pipeline lives here so the caret fast path can redo exactly
    /// the lines that changed.
    fn append_line_rows(
        &mut self,
        ctx: &RowCtx,
        i: usize,
        window: &mut Window,
        out: &mut Vec<LineElement>,
    ) -> Option<usize> {
        let base = out.len();
        let mut caret_at = None;
        // Conceal markers on every line but the cursor line, which keeps
        // full source so caret math stays on real document bytes.
        let text = line_text(&ctx.rope, i);
        let line_spans = ctx.spans.get(i).map_or(&[][..], Vec::as_slice);
        let segs = markdown::flatten(text.len(), line_spans);
        // Highlights land in source columns; a concealed line remaps them
        // through the conceal map so they track the display text.
        let selection = ctx.sel_span.and_then(|(lo, hi)| line_highlight(&ctx.rope, i, lo, hi));
        let search: Vec<Highlight> = ctx
            .search_ranges
            .iter()
            .filter_map(|&(lo, hi)| line_highlight(&ctx.rope, i, lo, hi))
            .collect();
        // Concealed heading lines shape larger inside the fixed line box.
        // Concealed only: the cursor line (revealed source) keeps body size,
        // so caret geometry and scroll_x math never see a non-body size.
        // Scale is a pure function of the concealed segments — the same value
        // the wrap cache keys on — so cached boundaries stay consistent.
        let revealed = i == ctx.cur_line || ctx.reveal_fences.contains(&Some(i));
        let (text, segments, selection, search, scale, decor) = if self.render_markdown
            && !revealed
        {
            let c = markdown::conceal(&text, &segs);
            let selection = selection.and_then(|h| remap_highlight(h, &text, &c));
            let search =
                search.into_iter().filter_map(|h| remap_highlight(h, &text, &c)).collect();
            let scale = heading_scale(&c.segments);
            (c.text, c.segments, selection, search, scale, row_decor(&ctx.spans, i))
        } else {
            // Source view (revealed line, or markdown rendering off): a task
            // box's `[ ]` shows its source bytes styled like any other marker
            // instead of the transparent box span. The code band is the one
            // decoration that survives reveal — code text isn't concealed
            // anyway, and the block should read as one unit while edited.
            let segs = segs
                .into_iter()
                .map(|s| match s.kind {
                    Some(SpanKind::Task(_)) => Segment { kind: Some(SpanKind::Marker), ..s },
                    _ => s,
                })
                .collect();
            let decor = self
                .render_markdown
                .then(|| row_decor(&ctx.spans, i))
                .flatten()
                .filter(|d| matches!(d, RowDecor::CodeBand { .. }));
            (text, segs, selection, search, 1.0, decor)
        };

        // Byte offset where each visual row starts: 0, plus one per wrap
        // boundary (the boundary glyph opens the next row). Plain ASCII
        // lines in a monospace font take the column walk; anything the
        // walk can't promise — non-ASCII (fallback fonts, wide glyphs),
        // tabs, bold spans (a family's bold could differ) — shapes for
        // exact boundaries, cached in `wrap_cache` across rebuilds.
        let row_starts: Vec<usize> = match ctx.wrap_width {
            None => vec![0],
            Some(w) => {
                // Code-band text is inset by CODE_MARGIN + CODE_PAD per side
                // (paint shifts it right); wrap inside the inset width — a
                // reduced column budget on the mono walk, a reduced pixel
                // width when shaping — so wrapped rows stay clear of the
                // band's right border.
                let band = matches!(decor, Some(RowDecor::CodeBand { .. }));
                let w = if band { w - (CODE_MARGIN + CODE_PAD) * 2. } else { w };
                let plain = text.is_ascii()
                    && !text.contains('\t')
                    && segments.iter().all(|s| {
                        !matches!(s.kind, Some(SpanKind::Heading(_)) | Some(SpanKind::Strong))
                    });
                match if band { ctx.mono_band_cols } else { ctx.mono_cols } {
                    Some(cols) if plain => wrap_columns(&text, cols),
                    _ => {
                        let key = (text.clone(), segments.clone());
                        match self.wrap_cache.get(&key) {
                            Some(starts) => starts,
                            None => {
                                let runs = segments_to_runs(
                                    &text,
                                    &segments,
                                    &ctx.font,
                                    ctx.theme.foreground,
                                    &ctx.theme,
                                );
                                let wrapped = window
                                    .text_system()
                                    .shape_text(
                                        text.clone().into(),
                                        ctx.font_size * scale,
                                        &runs,
                                        Some(w),
                                        None,
                                    )
                                    .ok()
                                    .and_then(|lines| lines.into_iter().next());
                                let mut starts = vec![0];
                                if let Some(wl) = wrapped {
                                    starts.extend(wl.wrap_boundaries.iter().map(|b| {
                                        wl.unwrapped_layout.runs[b.run_ix].glyphs[b.glyph_ix]
                                            .index
                                    }));
                                }
                                self.wrap_cache.cur.insert(key, starts.clone());
                                starts
                            }
                        }
                    }
                }
            }
        };
        // Char col where each row starts — highlight and caret columns
        // are char-based, byte offsets index the text slices. ASCII:
        // bytes are cols. Otherwise one pass over the char boundaries
        // (per-row `chars().count()` was quadratic on long lines).
        let (row_cols, line_chars): (Vec<usize>, usize) = if text.is_ascii() {
            (row_starts.clone(), text.len())
        } else {
            let mut cols = Vec::with_capacity(row_starts.len());
            let mut chars = 0;
            let mut ci = text.char_indices().peekable();
            for &b in &row_starts {
                while ci.next_if(|&(cb, _)| cb < b).is_some() {
                    chars += 1;
                }
                cols.push(chars);
            }
            (cols, text.chars().count())
        };
        let last = row_starts.len() - 1;
        // The caret's row: the last row starting at or before its byte (a
        // byte on a boundary belongs to the row the boundary opens).
        let caret_row = (i == ctx.cur_line).then(|| {
            let byte = caret_bytes(&text, ctx.cur_col).0;
            row_starts.partition_point(|&b| b <= byte) - 1
        });

        for k in 0..=last {
            let b0 = row_starts[k];
            let b1 = row_starts.get(k + 1).copied().unwrap_or(text.len());
            let c0 = row_cols[k];
            let c1 = row_cols.get(k + 1).copied().unwrap_or(line_chars);
            if caret_row == Some(k) {
                caret_at = Some(out.len() - base);
            }
            // Line number on the first row only; continuation rows carry
            // same-width blanks so their text aligns. Relative mode is
            // hybrid: the cursor line shows its absolute number, others
            // the distance to it.
            let num_width = ctx.num_width;
            let gutter = (self.line_numbers != LineNumbers::Off).then(|| {
                if k > 0 {
                    return (format!(" {:>num_width$}  ", "").into(), ctx.theme.muted);
                }
                let n = match self.line_numbers {
                    LineNumbers::Relative if i != ctx.cur_line => i.abs_diff(ctx.cur_line),
                    _ => i + 1,
                };
                let color =
                    if i == ctx.cur_line { ctx.theme.foreground } else { ctx.theme.muted };
                (format!(" {n:>num_width$}  ").into(), color)
            });
            out.push(LineElement {
                text: text[b0..b1].to_string().into(),
                segments: slice_segments(&segments, b0, b1),
                caret: (caret_row == Some(k)).then(|| LineCaret {
                    col: ctx.cur_col - c0,
                    block: ctx.mode != Mode::Insert,
                }),
                selection: selection.and_then(|h| clip_row_highlight(h, c0, c1, k == last)),
                search: search
                    .iter()
                    .filter_map(|&h| clip_row_highlight(h, c0, c1, k == last))
                    .collect(),
                scroll_x: self.scroll_x.clone(),
                caret_paint: self.caret_paint.clone(),
                gutter,
                follow_h: ctx.wrap_width.is_none(),
                scale,
                // A wrapped band line closes its border only on its outermost
                // visual rows; middle rows keep the sides running through.
                decor: match decor {
                    Some(RowDecor::CodeBand { top, bottom }) => Some(RowDecor::CodeBand {
                        top: top && k == 0,
                        bottom: bottom && k == last,
                    }),
                    d => d,
                },
            });
        }
        caret_at
    }

    /// `:e {path}` — open `path` for editing. A nonexistent file opens as a
    /// blank buffer that `:w` creates (`Document::open` is vim-lazy). Relative
    /// names resolve under the vault root, so a new note lands in — and shows up
    /// in — the vault. Opening lands in a buffer, so nothing is discarded — the
    /// exception is `:e` on the already-open file, vim's reload-from-disk,
    /// which drops unsaved changes only with `bang` (`:e!`).
    fn edit(&mut self, name: &str, bang: bool, window: &mut Window) {
        if name.is_empty() {
            self.message = Some("E32: No file name".into());
            return;
        }
        let path = resolve(&self.vault.root, name);
        if self.doc().path() == Some(path.as_path()) {
            if !self.may_discard(bang) {
                return;
            }
            // Reload in place: same tab (preview flag kept), fresh Document —
            // the undo history goes with it, since it indexes the old text.
            self.buffers[self.active].doc = open_or_empty(&path);
            self.switch_to(self.active, window);
            return;
        }
        self.open_path(path, window);
    }

    /// `:enew` — a blank, unnamed buffer in the preview slot; name it on the
    /// first `:w {name}`.
    fn enew(&mut self, window: &mut Window) {
        self.show_preview(Document::new(""), window);
    }

    /// Tab / `:b`-match display name: vault-relative path (`.md` dropped) for
    /// pathed buffers, `[No Name]` for scratch.
    fn buffer_display(&self, b: &Buffer) -> String {
        b.doc
            .path()
            .map(|p| rel_display(&self.vault.root, p))
            .unwrap_or_else(|| "[No Name]".into())
    }

    /// `:b {arg}` — switch buffer by tab number, `#` (alternate), or name.
    fn buffer_switch(&mut self, arg: Option<&str>, window: &mut Window) {
        let Some(arg) = arg else {
            self.message = Some("E471: Argument required".into());
            return;
        };
        if arg == "#" {
            self.buffer_alternate(window);
            return;
        }
        let names: Vec<String> = self.buffers.iter().map(|b| self.buffer_display(b)).collect();
        match match_buffer(arg, &names) {
            Ok(i) => self.activate(i, window),
            Err(msg) => self.message = Some(msg),
        }
    }

    /// `:bn` — cycle forward through the tabs, wrapping.
    fn buffer_next(&mut self, window: &mut Window) {
        self.activate((self.active + 1) % self.buffers.len(), window);
    }

    /// `:bp` — cycle backward through the tabs, wrapping.
    fn buffer_prev(&mut self, window: &mut Window) {
        let n = self.buffers.len();
        self.activate((self.active + n - 1) % n, window);
    }

    /// `Ctrl-6` / `:b #` — bounce to the previously active buffer.
    fn buffer_alternate(&mut self, window: &mut Window) {
        match self.alternate {
            Some(i) => self.activate(i, window),
            None => self.message = Some("E23: No alternate file".into()),
        }
    }

    /// Close tab `i` (`:bd` semantics): refuse while dirty unless `bang`. The
    /// last tab is replaced by a scratch buffer — `buffers` is never empty.
    fn close_buffer(&mut self, i: usize, bang: bool, window: &mut Window) {
        if self.buffers[i].doc.is_dirty() && !bang {
            self.message = Some("E89: No write since last change (add ! to override)".into());
            return;
        }
        self.buffers.remove(i);
        // The removal shifted every index above it; re-point (or drop) alternate.
        self.alternate = match self.alternate {
            Some(a) if a == i => None,
            Some(a) if a > i => Some(a - 1),
            other => other,
        };
        if self.buffers.is_empty() {
            self.buffers.push(Buffer { doc: Document::new(""), preview: true });
        }
        if self.active == i {
            // Closed the active tab: land on the next one (clamped). switch_to,
            // not activate — the dead slot must not become the alternate.
            self.switch_to(i.min(self.buffers.len() - 1), window);
        } else if self.active > i {
            self.active -= 1; // same buffer, shifted index
        }
        // Idempotent when switch_to above already wrote; the other branches
        // need it.
        self.save_session();
    }

    /// `:q`(`!`) — quit the app; refuses while any buffer has unsaved changes.
    fn quit(&mut self, bang: bool, cx: &mut Context<Self>) {
        if !bang {
            if let Some(b) = self.buffers.iter().find(|b| b.doc.is_dirty()) {
                let name = self.buffer_display(b);
                self.message =
                    Some(format!("E162: No write since last change for buffer \"{name}\""));
                return;
            }
        }
        self.save_session();
        cx.quit();
    }

    /// `:ls` — the open-buffer picker: tab number + display name (+ `[+]`).
    fn open_buffer_picker(&mut self) {
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
    fn open_file_picker(&mut self) {
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
    fn open_insert_link_picker(&mut self) {
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
    fn open_command_palette(&mut self) {
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
    fn picker_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
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
    fn render_picker(&self, theme: &Theme) -> Option<impl IntoElement> {
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

    /// The tab row above the editor: one tab per buffer, `{n}: {basename}`,
    /// `●` when dirty, italic while a preview, muted + struck through when the
    /// backing file has been deleted out from under it. Click switches;
    /// middle-click closes (`:bd` semantics, no force).
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

    /// Navigate the sidebar tree (`Pane::Sidebar`): `j`/`k` move the cursor over
    /// visible rows, `l`/Enter toggles a folder or opens a file, `h` collapses an
    /// open folder or jumps to the parent folder, Escape returns.
    ///
    /// File operations (sidebar-local, independent of editor vim state):
    /// `o`/`O` create in the cursor's / the parent folder, `dd` moves to the
    /// vault's `.trash` (inside Trash it deletes forever, confirmed), `cc`
    /// renames, `yy`/`x` load the file register, `p` pastes it into the
    /// cursor's folder (yank copies, cut moves).
    fn sidebar_key(&mut self, key: &str, shift: bool, window: &mut Window) {
        // A pending doubled op (`d`/`c`/`y`) either completes or dies here.
        if let Some(first) = self.pending_sidebar.take() {
            match (first, key) {
                ('d', "d") => self.trash_selected(window),
                ('c', "c") => self.open_rename_prompt(),
                ('y', "y") => self.yank_selected(),
                _ => {} // any other key cancels the op
            }
            return;
        }
        if key == "o" {
            self.open_create_prompt(shift); // works in an empty vault too
            return;
        }
        let rows = self.vault.visible_rows(&self.expanded);
        if rows.is_empty() {
            return;
        }
        self.selected = self.selected.min(rows.len() - 1);
        match key {
            "j" | "down" => self.selected = (self.selected + 1).min(rows.len() - 1),
            "k" | "up" => self.selected = self.selected.saturating_sub(1),
            "l" | "enter" => {
                let row = &rows[self.selected];
                if row.is_dir {
                    self.toggle(row.path.clone());
                } else {
                    self.open_path(row.path.clone(), window); // refocuses the editor pane
                    self.pane = Pane::Editor;
                }
            }
            "h" => {
                let row = &rows[self.selected];
                if row.is_dir && row.expanded {
                    self.toggle(row.path.clone()); // collapse
                } else if let Some(parent) = row.path.parent() {
                    // Jump to the enclosing folder's row; top-level rows have no
                    // folder parent (the vault root isn't a row), so they stay put.
                    if let Some(idx) = rows.iter().position(|r| r.is_dir && r.path == parent) {
                        self.selected = idx;
                    }
                }
            }
            "d" | "c" | "y" if !shift => {
                self.pending_sidebar = key.chars().next();
            }
            "x" if !shift => self.cut_selected(),
            "p" if !shift => self.paste_register(),
            "escape" => self.pane = Pane::Editor,
            _ => {}
        }
    }

    /// The sidebar row under the cursor, or `None` in an empty vault.
    fn selected_row(&self) -> Option<Row> {
        let mut rows = self.vault.visible_rows(&self.expanded);
        (!rows.is_empty()).then(|| rows.swap_remove(self.selected.min(rows.len() - 1)))
    }

    /// The folder file ops target: the folder under the sidebar cursor, a file
    /// row's containing folder, or the vault root in an empty vault.
    fn cursor_dir(&self) -> PathBuf {
        match self.selected_row() {
            Some(r) if r.is_dir => r.path,
            Some(r) => r
                .path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| self.vault.root.clone()),
            None => self.vault.root.clone(),
        }
    }

    /// Park the sidebar cursor on `path`'s row, expanding ancestors so it's
    /// visible. The render pass scrolls it on screen.
    fn select_path(&mut self, path: &Path) {
        expand_ancestors(&self.vault.root, path, &mut self.expanded);
        if let Some(i) = self
            .vault
            .visible_rows(&self.expanded)
            .iter()
            .position(|r| r.path == path)
        {
            self.selected = i;
        }
    }

    /// After an entry moved on disk (rename / cut-paste / trash), re-point any
    /// open buffer at the old path — or inside the old folder — to the new
    /// location, so saves land in the file's new home.
    fn repoint_buffers(&mut self, old: &Path, new: &Path) {
        let mut changed = false;
        for b in &mut self.buffers {
            let Some(rest) = b
                .doc
                .path()
                .and_then(|p| p.strip_prefix(old).ok().map(Path::to_path_buf))
            else {
                continue;
            };
            let dest = if rest.as_os_str().is_empty() { new.to_path_buf() } else { new.join(&rest) };
            b.doc.set_path(dest);
            changed = true;
        }
        if changed {
            self.save_session();
        }
    }

    /// `o`/`O` — grow an editable phantom row right below the cursor (vim's
    /// open-line, for files): type the name in place, Enter creates, Esc
    /// cancels. `parent` targets one level up (clamped to the vault root).
    fn open_create_prompt(&mut self, parent: bool) {
        let mut dir = self.cursor_dir();
        if parent && dir != self.vault.root {
            if let Some(p) = dir.parent() {
                dir = p.to_path_buf();
            }
        }
        let rows = self.vault.visible_rows(&self.expanded);
        // The phantom renders indented one level under its folder's row; the
        // sorted position comes from the post-commit rescan. `o` grows it
        // below the cursor (vim's open-line); `O` targets a folder the cursor
        // isn't in, so it sits under that folder's row instead — as its first
        // child, or at the top of the tree for the root (which has no row).
        let depth = rows.iter().find(|r| r.path == dir).map_or(0, |r| r.depth + 1);
        let at = if parent {
            rows.iter().position(|r| r.path == dir).map_or(0, |i| i + 1)
        } else if rows.is_empty() {
            0
        } else {
            self.selected.min(rows.len() - 1) + 1
        };
        self.sidebar_scroll.scroll_to_item(at, ScrollStrategy::Bottom);
        let rel = dir
            .strip_prefix(&self.vault.root)
            .unwrap_or(&dir)
            .to_string_lossy()
            .replace('\\', "/");
        let label = if rel.is_empty() { "New".to_string() } else { format!("New in {rel}/") };
        self.prompt = Some(FilePrompt {
            label,
            input: String::new(),
            action: PromptAction::Create { dir, at, depth },
        });
    }

    /// `cc` — the cursor row's label becomes editable in place, prefilled
    /// with the display name (a `.md` file's stem, any other entry's full
    /// name); Enter renames within the folder.
    fn open_rename_prompt(&mut self) {
        let Some(row) = self.selected_row() else { return };
        if row.path == self.vault.root.join(".trash") {
            self.message = Some("cannot rename the Trash".into());
            return;
        }
        self.prompt = Some(FilePrompt {
            label: "Rename".into(),
            input: row.name,
            action: PromptAction::Rename { target: row.path },
        });
    }

    /// `dd` — move the cursor row into the vault's `.trash`, closing any
    /// buffer open on it (a tab pointing into the trash is a trap). A dirty
    /// buffer blocks the whole op — write or discard first. Inside Trash it
    /// becomes the permanent delete (confirmed); on the Trash row itself it
    /// empties the trash.
    fn trash_selected(&mut self, window: &mut Window) {
        let Some(row) = self.selected_row() else { return };
        let trash = self.vault.root.join(".trash");
        if row.path.starts_with(&trash) {
            self.prompt = Some(FilePrompt {
                label: format!("Delete \"{}\" forever? (y/n) ", row.name),
                input: String::new(),
                action: PromptAction::ConfirmDelete { target: row.path },
            });
            return;
        }
        let dirty = self
            .buffers
            .iter()
            .find(|b| b.doc.is_dirty() && b.doc.path().is_some_and(|p| p.starts_with(&row.path)))
            .map(|b| self.buffer_display(b));
        if let Some(name) = dirty {
            self.message = Some(format!("E89: No write since last change for buffer \"{name}\""));
            return;
        }
        if let Err(e) = std::fs::create_dir_all(&trash) {
            self.message = Some(format!("trash failed: {e}"));
            return;
        }
        let name = row.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let dest = unique_dest(&trash, &name);
        match std::fs::rename(&row.path, &dest) {
            Ok(()) => {
                // All clean (checked above), so close_buffer can't refuse.
                // Closing the active buffer reveals the next one in the
                // sidebar; put the cursor back on the deletion site after.
                let keep = self.selected;
                while let Some(i) = self
                    .buffers
                    .iter()
                    .position(|b| b.doc.path().is_some_and(|p| p.starts_with(&row.path)))
                {
                    self.close_buffer(i, false, window);
                }
                self.selected = keep;
                self.message = Some(format!("moved to Trash: {}", row.name));
            }
            Err(e) => self.message = Some(format!("trash failed: {e}")),
        }
        self.rescan_vault();
    }

    /// `yy` — load the file register for a `p` copy.
    fn yank_selected(&mut self) {
        let Some(row) = self.selected_row() else { return };
        if row.is_dir {
            // ponytail: files only — folder copy needs a recursive-copy path,
            // add when someone wants it. (Folder *move* works via `x`.)
            self.message = Some("can only yank files".into());
            return;
        }
        self.message = Some(format!("yanked: {}", row.name));
        self.file_register = Some((row.path, false));
    }

    /// `x` — load the file register for a `p` move. Folders too (a move is
    /// one rename); cutting inside Trash is the restore path.
    fn cut_selected(&mut self) {
        let Some(row) = self.selected_row() else { return };
        if row.path == self.vault.root.join(".trash") {
            self.message = Some("cannot cut the Trash".into());
            return;
        }
        self.message = Some(format!("cut: {}", row.name));
        self.file_register = Some((row.path, true));
    }

    /// `p` — drop the register into the cursor's folder: a yank copies (and
    /// can paste again), a cut moves (and empties the register).
    fn paste_register(&mut self) {
        let Some((src, cut)) = self.file_register.clone() else {
            self.message = Some("nothing to paste".into());
            return;
        };
        if !src.exists() {
            self.message = Some("cut/yanked file no longer exists".into());
            self.file_register = None;
            return;
        }
        let dir = self.cursor_dir();
        let Some(name) = src.file_name() else { return };
        let name = name.to_string_lossy().into_owned();
        // A move onto a taken name is an error; a copy numbers itself instead
        // (`note 2.md`), so pasting a yank into its own folder duplicates.
        let (dest, result) = if cut {
            let dest = dir.join(&name);
            if dest == src {
                return; // moving onto itself: nothing to do
            }
            if dest.exists() {
                self.message = Some(format!("already exists: {name}"));
                return;
            }
            if src.is_dir() && dest.starts_with(&src) {
                self.message = Some("cannot move a folder into itself".into());
                return;
            }
            let r = std::fs::rename(&src, &dest);
            (dest, r)
        } else {
            let dest = unique_dest(&dir, &name);
            let r = std::fs::copy(&src, &dest).map(|_| ());
            (dest, r)
        };
        match result {
            Ok(()) => {
                if cut {
                    self.repoint_buffers(&src, &dest);
                    self.file_register = None;
                }
                self.rescan_vault();
                self.select_path(&dest);
                let verb = if cut { "moved" } else { "copied" };
                let final_name = dest.file_name().unwrap_or_default().to_string_lossy();
                self.message = Some(format!("{verb}: {final_name}"));
            }
            Err(e) => self.message = Some(format!("paste failed: {e}")),
        }
    }

    /// Keystrokes while a file-op prompt is open. Mirrors `picker_key`:
    /// printable chars extend the input, Backspace trims, Enter applies, Esc
    /// cancels. The delete confirmation is single-key: `y` commits, anything
    /// else cancels.
    fn prompt_key(&mut self, ev: &KeyDownEvent, window: &mut Window) {
        let key = ev.keystroke.key.as_str();
        if matches!(
            self.prompt.as_ref().map(|p| &p.action),
            Some(PromptAction::ConfirmDelete { .. })
        ) {
            if let Some(FilePrompt { action: PromptAction::ConfirmDelete { target }, .. }) =
                self.prompt.take()
            {
                if key == "y" {
                    self.delete_forever(&target);
                }
            }
            return;
        }
        let m = ev.keystroke.modifiers;
        match key {
            "escape" => self.prompt = None,
            "enter" => {
                // take() drops the prompt borrow before the action mutates self.
                if let Some(p) = self.prompt.take() {
                    self.apply_prompt(p, window);
                }
            }
            "backspace" => {
                if let Some(p) = self.prompt.as_mut() {
                    p.input.pop();
                }
            }
            _ if !m.control && !m.alt && !m.platform => {
                if let Some(s) = ev.keystroke.key_char.clone() {
                    if let Some(p) = self.prompt.as_mut() {
                        p.input.push_str(&s);
                    }
                }
            }
            _ => {}
        }
    }

    fn apply_prompt(&mut self, prompt: FilePrompt, window: &mut Window) {
        let input = prompt.input.trim();
        if input.is_empty() {
            return;
        }
        match prompt.action {
            PromptAction::Create { dir, .. } => self.create_entry(&dir, input, window),
            PromptAction::Rename { target } => self.rename_entry(&target, input),
            PromptAction::ConfirmDelete { .. } => {} // handled in prompt_key
        }
    }

    /// Create `input` inside `dir`: a folder when it ends in `/`, else a file
    /// (bare names default to `.md`) that opens for editing. Slashes inside
    /// the name create the intermediate folders.
    fn create_entry(&mut self, dir: &Path, input: &str, window: &mut Window) {
        if let Some(folder) = input.strip_suffix('/') {
            if folder.is_empty() {
                return;
            }
            match std::fs::create_dir_all(dir.join(folder)) {
                Ok(()) => {
                    self.rescan_vault();
                    self.select_path(&dir.join(folder));
                    self.message = Some(format!("created: {folder}/"));
                }
                Err(e) => self.message = Some(format!("create failed: {e}")),
            }
            return;
        }
        let path = dir.join(with_md_ext(input));
        if path.exists() {
            self.message = Some(format!("already exists: {input}"));
            return;
        }
        let result = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(&path, ""));
        match result {
            Ok(()) => {
                self.rescan_vault();
                self.open_path(path, window); // reveals the row, refocuses the editor
                self.pane = Pane::Editor;
            }
            Err(e) => self.message = Some(format!("create failed: {e}")),
        }
    }

    /// Apply `cc`: rename `target` to `input` within its folder. File names
    /// default to `.md` like `:e`/`:w`; folder names are taken as typed.
    fn rename_entry(&mut self, target: &Path, input: &str) {
        let new_name = if target.is_dir() { PathBuf::from(input) } else { with_md_ext(input) };
        let Some(parent) = target.parent() else { return };
        let dest = parent.join(new_name);
        if dest == target {
            return;
        }
        if dest.exists() {
            self.message = Some(format!("already exists: {input}"));
            return;
        }
        match std::fs::rename(target, &dest) {
            Ok(()) => {
                self.repoint_buffers(target, &dest);
                self.rescan_vault();
                self.select_path(&dest);
                let name = dest.file_name().unwrap_or_default().to_string_lossy().into_owned();
                self.message = Some(format!("renamed to: {name}"));
            }
            Err(e) => self.message = Some(format!("rename failed: {e}")),
        }
    }

    /// `y` on the delete confirmation — the only operation that destroys data.
    fn delete_forever(&mut self, target: &Path) {
        let result = if target.is_dir() {
            std::fs::remove_dir_all(target)
        } else {
            std::fs::remove_file(target)
        };
        let name = target.file_name().unwrap_or_default().to_string_lossy();
        self.message = Some(match result {
            Ok(()) => format!("deleted: {name}"),
            Err(e) => format!("delete failed: {e}"),
        });
        self.rescan_vault();
    }

    /// Entry point for every raw `KeyDown`. darknotes drives its own repeat
    /// cadence instead of trusting the OS/platform backend, which is wildly
    /// inconsistent across them: macOS's native auto-repeat is gated by
    /// System Settings and stays slow even at its fastest slider, while
    /// gpui's X11 backend (WSLg included) never marks a repeat at all —
    /// every pulse the X server's own auto-repeat generates arrives here
    /// looking like a fresh press.
    ///
    /// So a `KeyDown` for the stroke we're already repeating (`repeat_stroke`)
    /// is treated as exactly that: an echo from the backend, not a new press,
    /// and swallowed — `arm_key_repeat`'s own timer is what drives the
    /// cadence from here. A deliberate second tap of the same key is never
    /// swallowed because it's always preceded by a `KeyUp` (see `on_key_up`),
    /// which clears `repeat_stroke` first.
    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.key_repeat_interval > 0 && self.repeat_stroke.as_ref() == Some(&ev.keystroke) {
            return;
        }
        self.handle_key(ev, window, cx);
        if self.key_repeat_interval > 0 {
            self.arm_key_repeat(ev.keystroke.clone(), window, cx);
        }
    }

    /// A physical key release: stop repeating it, if it was the one
    /// repeating (only one stroke repeats at a time, so a `KeyUp` for
    /// anything else is a no-op here).
    fn on_key_up(&mut self, ev: &KeyUpEvent, _window: &mut Window, _cx: &mut Context<Self>) {
        if self.repeat_stroke.as_ref() == Some(&ev.keystroke) {
            self.repeat_stroke = None;
            self.repeat_timer = None;
        }
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
            self.prompt_key(ev, window);
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
        let actions = self.vim.on_key(ks);
        let entering_insert = mode_before == Mode::Normal && self.vim.mode == Mode::Insert;
        // Checkpoint a single undoable unit. Insert-mode edits are excluded so the
        // whole session coalesces into the entering-insert checkpoint; everything
        // else (normal- and visual-mode mutations) gets its own.
        let mutates = mode_before != Mode::Insert && actions.iter().any(Action::mutates);
        if entering_insert || mutates {
            self.doc_mut().checkpoint();
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

    /// The single execution seam every input grammar funnels through.
    fn apply(&mut self, action: Action, window: &mut Window, cx: &mut Context<Self>) {
        let renumbers = action.renumbers();
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
            Action::DeleteSelection { linewise } => self.doc_mut().delete_selection(linewise),
            Action::YankSelection { linewise } => self.doc_mut().yank_selection(linewise),
            Action::IndentSelection { width, dedent } => {
                self.doc_mut().indent_selection(width, dedent)
            }
            Action::CollapseSelection => self.doc_mut().collapse_selection(),
            Action::DeleteMotion(m, n) => self.doc_mut().delete_motion(m, n),
            Action::DeleteLines(n) => self.doc_mut().delete_lines(n),
            Action::DeleteLinesVertical { count, up } => self.doc_mut().delete_lines_dir(count, up),
            Action::DeleteCharUnder(n) => self.doc_mut().delete_char_under(n),
            Action::YankMotion(m, n) => self.doc_mut().yank_motion(m, n),
            Action::YankLines(n) => self.doc_mut().yank_lines(n),
            Action::DeleteObject { obj, change } => self.doc_mut().delete_object(obj, change),
            Action::YankObject(obj) => self.doc_mut().yank_object(obj),
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
            Action::ExecuteCommand(cmd) => self.exec_command(&cmd, window, cx),
            Action::Search { query, backward } => self.do_search(query, backward),
            Action::SearchNext { reverse, count } => self.search_next(reverse, count),
            Action::BufferNext => self.buffer_next(window),
            Action::BufferPrev => self.buffer_prev(window),
            Action::FollowLink => self.follow_link(window, cx),
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
    /// like `:e`); an external URL opens in the browser. Off a link it's a
    /// silent no-op.
    fn follow_link(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (line, col) = self.doc().caret_line_col();
        let text = line_text(&self.doc().rope, line);
        if let Some(target) = markdown::wikilink_at(&text, col) {
            match resolve_link(&self.vault.root, &self.vault.files, &target) {
                Some(path) => self.open_path(path, window),
                None => self.edit(&target, false, window),
            }
        } else if let Some(url) = markdown::url_at(&text, col) {
            cx.open_url(&url);
        }
    }

    /// A submitted `/`/`?` query. An empty query repeats the last search in the
    /// new direction. The jump starts from where the prompt opened — incsearch
    /// may have dragged the caret elsewhere while typing.
    fn do_search(&mut self, query: String, backward: bool) {
        let origin = self.search.origin.take().unwrap_or_else(|| self.doc().caret_offset());
        if !query.is_empty() {
            self.search.query = query;
        }
        if self.search.query.is_empty() {
            self.message = Some("E35: No previous regular expression".into());
            return;
        }
        self.search.backward = backward;
        self.search.hl = true;
        self.doc_mut().jump_to(origin);
        self.find_and_jump(backward, 1);
    }

    /// `n`/`N`: repeat the last search; `reverse` flips its stored direction.
    fn search_next(&mut self, reverse: bool, count: usize) {
        if self.search.query.is_empty() {
            self.message = Some("E35: No previous regular expression".into());
            return;
        }
        self.search.hl = true; // `n` after `:noh` re-lights the matches
        self.find_and_jump(self.search.backward != reverse, count);
    }

    /// Jump `count` matches from the caret, honoring wrapscan, with vim's wrap
    /// and not-found messages. The caret stays put when nothing is found.
    fn find_and_jump(&mut self, backward: bool, count: usize) {
        let q = &self.search.query;
        let matches = find_matches(&self.doc().rope, q, search_sensitive(q, &self.search_cfg));
        let mut at = self.doc().caret_offset();
        let mut wrapped = false;
        for _ in 0..count.max(1) {
            match next_match(&matches, at, backward, self.search_cfg.wrapscan) {
                Some((i, w)) => {
                    at = matches[i].0;
                    wrapped |= w;
                }
                None => {
                    self.message = Some(if matches.is_empty() {
                        format!("E486: Pattern not found: {q}")
                    } else if backward {
                        format!("E384: search hit TOP without match for: {q}")
                    } else {
                        format!("E385: search hit BOTTOM without match for: {q}")
                    });
                    return;
                }
            }
        }
        self.doc_mut().jump_to(at);
        if wrapped {
            self.message = Some(if backward {
                "search hit TOP, continuing at BOTTOM".into()
            } else {
                "search hit BOTTOM, continuing at TOP".into()
            });
        }
    }

    /// Track the search-prompt lifecycle around each key: capture the caret
    /// when `/`/`?` opens, live-preview the nearest match while typing
    /// (incsearch), restore the caret on cancel. A submitted search lands via
    /// `apply`, which consumes `origin` before this runs.
    fn sync_search_prompt(&mut self, mode_before: Mode) {
        let in_prompt = self.vim.mode == Mode::Command && self.vim.prompt() != ':';
        if in_prompt {
            if mode_before != Mode::Command {
                self.search.origin = Some(self.doc().caret_offset());
            }
            if self.search_cfg.incsearch {
                let origin = self.search.origin.unwrap_or(0);
                let q = self.vim.command_line();
                let jump = (!q.is_empty())
                    .then(|| {
                        let matches =
                            find_matches(&self.doc().rope, q, search_sensitive(q, &self.search_cfg));
                        next_match(&matches, origin, self.vim.prompt() == '?', self.search_cfg.wrapscan)
                            .map(|(i, _)| matches[i].0)
                    })
                    .flatten();
                self.doc_mut().jump_to(jump.unwrap_or(origin)); // no match → sit at origin
            }
        } else if let Some(origin) = self.search.origin.take() {
            self.doc_mut().jump_to(origin); // Esc / backspace-past-prompt cancelled
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

/// Bare names get a `.md` extension; anything with an extension is left alone.
/// Deliberate even though the vault holds mixed file types: `:e foo` stays a
/// quick note creator; opening `schema.sql` means typing its real name (or
/// picking it from the sidebar/switcher, which pass full paths).
fn with_md_ext(name: &str) -> PathBuf {
    let p = PathBuf::from(name);
    if p.extension().is_none() {
        p.with_extension("md")
    } else {
        p
    }
}

/// First non-existing `dir/name`, numbering before the extension when taken
/// (`note 2.md`, `note 3.md`, …) — so moving into `.trash` never collides.
fn unique_dest(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let p = Path::new(name);
    let stem = p.file_stem().unwrap_or_default().to_string_lossy();
    let ext = p
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    (2..)
        .map(|n| dir.join(format!("{stem} {n}{ext}")))
        .find(|p| !p.exists())
        .expect("some numbered name is free")
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

/// Vault-relative path of `path`, forward-slashed — the switcher match key and
/// label. The implied `.md` is dropped (`projects/ideas`); any other extension
/// is kept (`sql/schema.sql`).
fn rel_display(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    let rel = if rel.extension().is_some_and(|e| e == "md") {
        rel.with_extension("")
    } else {
        rel.to_path_buf()
    };
    rel.to_string_lossy().replace('\\', "/")
}

/// Resolve a wikilink target to a vault file: vault-relative path (`.md`
/// dropped) or bare file stem, case-insensitive, first hit in vault order.
// ponytail: first match wins on duplicate stems; rank by path length if
// collisions ever matter.
fn resolve_link(root: &Path, files: &[PathBuf], target: &str) -> Option<PathBuf> {
    files
        .iter()
        .find(|p| {
            rel_display(root, p).eq_ignore_ascii_case(target)
                || p.file_stem()
                    .is_some_and(|s| s.to_string_lossy().eq_ignore_ascii_case(target))
        })
        .cloned()
}

/// Resolve a `:b` argument against buffer display names (vault-relative, `.md`
/// dropped): a 1-based tab number, an exact name/basename match, else a unique
/// case-insensitive substring match. `#` (alternate) is handled by the caller.
/// Numbers are tab positions, not vim's stable buffer ids — positions are what
/// the tabline shows.
fn match_buffer(arg: &str, names: &[String]) -> Result<usize, String> {
    if let Ok(n) = arg.parse::<usize>() {
        return if (1..=names.len()).contains(&n) {
            Ok(n - 1)
        } else {
            Err(format!("E86: Buffer {n} does not exist"))
        };
    }
    let exact: Vec<usize> = names
        .iter()
        .enumerate()
        .filter(|(_, n)| n.as_str() == arg || n.rsplit('/').next() == Some(arg))
        .map(|(i, _)| i)
        .collect();
    match exact.as_slice() {
        [i] => return Ok(*i),
        [] => {}
        _ => return Err(format!("E93: More than one match for {arg}")),
    }
    let needle = arg.to_lowercase();
    let subs: Vec<usize> = names
        .iter()
        .enumerate()
        .filter(|(_, n)| n.to_lowercase().contains(&needle))
        .map(|(i, _)| i)
        .collect();
    match subs.as_slice() {
        [i] => Ok(*i),
        [] => Err(format!("E94: No matching buffer for {arg}")),
        _ => Err(format!("E93: More than one match for {arg}")),
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

/// Mark every folder between the vault `root` and `path` as expanded, so a
/// nested file's row is visible. `root` itself isn't a row, so it's excluded.
fn expand_ancestors(root: &Path, path: &Path, set: &mut HashSet<PathBuf>) {
    let mut cur = path.parent();
    while let Some(dir) = cur {
        if dir == root || !dir.starts_with(root) {
            break;
        }
        set.insert(dir.to_path_buf());
        cur = dir.parent();
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
        let plan = match &self.rows_cache {
            Some(c) if c.key == key => Plan::Hit,
            // Relative line numbers re-label every row on a caret line
            // change, so they can't take the two-line patch.
            Some(c)
                if caret_only_change(&c.key, &key)
                    && self.line_numbers != LineNumbers::Relative =>
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
                // reveal — can render differently (conceal swap, caret,
                // gutter emphasis). Rebuild those lines and splice in place.
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
                self.rows_cache =
                    Some(RowsCache { key, rows: rows.clone(), cur_row, line_rows: c.line_rows });
                (rows, cur_row)
            }
            Plan::Full => {
                let ctx = self.row_ctx(wrap_width, &theme, window);
                let (rows, cur_row, line_rows) = self.build_rows(&ctx, window);
                let rows = Rc::new(rows);
                self.rows_cache =
                    Some(RowsCache { key, rows: rows.clone(), cur_row, line_rows });
                (rows, cur_row)
            }
        };
        let editor_row_count = lines.len();

        // Keep the caret on screen, but only when its row actually moved — so
        // the mouse wheel can scroll freely without snapping back every frame.
        // A buffer switch recenters instead (the shared scroll handle still
        // holds the old buffer's offset). Like vim, landing on a soft-wrapped
        // line pulls the whole line into view, not just the caret's row: the
        // scroll target is the line's last visual row moving down, its first
        // moving up, clamped so the caret itself stays visible when a single
        // line wraps taller than the viewport. One scroll_to_item call only —
        // gpui keeps a single deferred scroll per frame, last call wins.
        let line_h = px(self.font_size * 22.0 / 15.0);
        if self.center_on_render {
            self.scroll.scroll_to_item_strict(cur_row, ScrollStrategy::Center);
            self.center_on_render = false;
        } else if cur_row != self.last_row {
            let c = self.rows_cache.as_ref().unwrap();
            let line = self.doc().rope.char_to_line(c.key.caret);
            let first: usize = c.line_rows[..line].iter().map(|&n| n as usize).sum();
            let last = first + c.line_rows[line] as usize - 1;
            // Rows that fit fully in the viewport (1 before first layout).
            let fit = self
                .scroll
                .0
                .borrow()
                .last_item_size
                .map_or(1, |s| (s.item.height / line_h).floor() as usize)
                .max(1);
            if cur_row > self.last_row {
                let target = last.min(cur_row + fit - 1);
                self.scroll.scroll_to_item(target, ScrollStrategy::Bottom);
            } else {
                let target = first.max(cur_row.saturating_sub(fit - 1));
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
        let scroll_x = self.scroll_x.clone();
        let caret_paint = self.caret_paint.clone();

        // Mode reads as a colored pill; command mode keeps the raw `:` prompt
        // and a file-op prompt shows its hint instead. The filename (and dirty
        // flag) live in the tabline.
        let (pill, bar) = if let Some(p) = &self.prompt {
            // Create/rename input renders inline in the tree; this is a hint.
            (None, p.label.clone())
        } else if mode == Mode::Command {
            (None, format!("{}{}", self.vim.prompt(), self.vim.command_line()))
        } else {
            let pill = match mode {
                Mode::Insert => ("INSERT", theme.mode_insert),
                Mode::Visual => ("VISUAL", theme.mode_visual),
                Mode::VisualLine => ("VISUAL LINE", theme.mode_visual),
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

        // Blank phantom lines past EOF give the list overscroll room, vim-style:
        // `zz`/`zt` keep working near the bottom of the file, and the wheel can
        // scroll until the last real line sits at the top of the viewport. Sized
        // from the previous frame's viewport height (zero before first layout).
        let overscroll = self
            .scroll
            .0
            .borrow()
            .last_item_size
            .map_or(0, |s| (s.item.height / line_h).ceil() as usize)
            .saturating_sub(1);

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
                        uniform_list("lines", editor_row_count + overscroll, move |range, _win, _cx| {
                            range
                                .map(|i| {
                                    // Phantom overscroll row past EOF: blank, no gutter.
                                    if i >= editor_row_count {
                                        return LineElement {
                                            text: "".into(),
                                            segments: Vec::new(),
                                            caret: None,
                                            selection: None,
                                            search: Vec::new(),
                                            scroll_x: scroll_x.clone(),
                                            caret_paint: caret_paint.clone(),
                                            gutter: None,
                                            follow_h: false,
                                            scale: 1.0,
                                            decor: None,
                                        };
                                    }
                                    lines[i].clone()
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
    }
}

/// One caret on the cursor line. `block` is vim's normal/command block caret
/// (inverts the char under it); otherwise it's the insert-mode bar.
#[derive(Clone, Copy)]
struct LineCaret {
    col: usize,
    block: bool,
}

/// How the caret paints this frame: solid accent (focused, blink phase on),
/// hidden (focused, blink phase off), or dim `muted` (editor unfocused —
/// solid, no blinking).
#[derive(Clone, Copy, PartialEq)]
enum CaretPaint {
    Solid,
    Hidden,
    Dim,
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
/// as an overlay. Shaping keys on `text` + `segments`, never the caret position,
/// so GPUI's shaped-line cache reuses layouts as the caret moves within a line.
/// When `render_markdown` is on, non-cursor lines carry concealed text/segments
/// and the cursor line carries source, so only the two lines a vertical move
/// swaps between re-shape — everything else stays cached.
/// With soft-wrap, one of these is one *visual row*: `build_rows` slices a
/// wrapped line into per-row text/segments/highlights, so this element never
/// needs to know about wrapping.
#[derive(Clone)]
struct LineElement {
    text: SharedString,
    /// Styled segments matching `text` (concealed or source), mapped to runs in
    /// `prepaint`. Independent of the caret, so the cache keys on content alone.
    segments: Vec<Segment>,
    /// `Some` only on the cursor line.
    caret: Option<LineCaret>,
    /// `Some` when part of this line falls inside the visual selection. Columns
    /// index this element's `text` — concealed lines get spans already remapped
    /// through the conceal map.
    selection: Option<Highlight>,
    /// Search-match column spans within this line (hlsearch/incsearch), in the
    /// same (possibly concealed) coordinates as `selection`.
    search: Vec<Highlight>,
    /// Shared horizontal scroll offset. The cursor line writes it (prepaint),
    /// every line reads it (paint) — see `Editor::scroll_x`.
    scroll_x: Rc<Cell<Pixels>>,
    /// How the caret paints (blink phase + focus), shared like `scroll_x` —
    /// see `Editor::caret_paint`.
    caret_paint: Rc<Cell<CaretPaint>>,
    /// Pre-formatted line-number string and its color. `None` when the gutter is
    /// off. Painted at a fixed left position; the text is shifted right past it.
    gutter: Option<(SharedString, Hsla)>,
    /// Nudge `scroll_x` to keep the caret horizontally on screen — nowrap
    /// only. A wrapped row never overflows, and its caret reaching the right
    /// edge must not shift the pane. Only the caret row acts on it.
    follow_h: bool,
    /// Font-size multiplier for this row (concealed heading lines shape
    /// larger). 1.0 everywhere else; the gutter always stays at body size.
    scale: f32,
    /// Block-level paint decoration (code band / quote bar / rule hairline).
    /// `None` on the cursor line and with markdown rendering off.
    decor: Option<RowDecor>,
}

struct LinePrepaint {
    shaped: ShapedLine,
    /// Shaped line-number gutter and its width; `None` when the gutter is off.
    gutter: Option<ShapedLine>,
    gutter_w: Pixels,
    /// `(x within the line, width)` of the selection quad; `None` if unselected.
    selection: Option<(Pixels, Pixels)>,
    /// `(x within the line, width)` of each search-match quad.
    search: Vec<(Pixels, Pixels)>,
    /// `(x within the line, width)` of the caret quad; `None` off the cursor line.
    caret: Option<(Pixels, Pixels)>,
    /// Block caret only: the glyph under the caret, repainted dark over the
    /// block. `(font, glyph, x within the line)`. `None` for the bar and at EOL.
    caret_glyph: Option<(FontId, GlyphId, Pixels)>,
    /// `(x0, x1, checked)` of a task box's transparent `[ ]` span; the box
    /// paints centered in it. `None` when the row has none (or shows source —
    /// those rows carry Marker, not Task).
    task: Option<(Pixels, Pixels, bool)>,
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
        cx: &mut App,
    ) -> LinePrepaint {
        let style = window.text_style();
        let font = style.font();
        let fg = style.color;
        let base_size = style.font_size.to_pixels(window.rem_size());
        // Headings shape larger; the gutter below keeps body size so line
        // numbers stay column-aligned across rows.
        let font_size = base_size * self.scale;

        // Styled runs from this line's markdown spans. Shaping keys on text +
        // markup, never the caret, so the layout cache keeps hitting as the
        // cursor moves; the caret below is an overlay that re-shapes nothing.
        let theme = *cx.global::<Theme>();
        let runs = segments_to_runs(&self.text, &self.segments, &font, fg, &theme);
        // Shape the line-number gutter (if any). It sits at a fixed left position
        // and never scrolls, so the text below is shifted right by its width.
        let gutter = self.gutter.as_ref().map(|(text, color)| {
            let runs = [run(&font, text.len(), *color)];
            window
                .text_system()
                .shape_line(text.clone(), base_size, &runs, None)
        });
        let gutter_w = gutter.as_ref().map_or(Pixels::ZERO, |g| g.width);
        let shaped = window
            .text_system()
            .shape_line(self.text.clone(), font_size, &runs, None);

        let (caret, caret_glyph) = match self.caret {
            None => (None, None),
            Some(c) => {
                let (caret_byte, under_end) = caret_bytes(&self.text, c.col);
                let x = shaped.x_for_index(caret_byte);
                // Follow the caret horizontally: keep it `margin` inside both
                // edges of the pane. Only the cursor row writes scroll_x; every
                // row reads it in paint. A short line (caret near x=0) snaps the
                // offset back to 0 on its own.
                // ponytail: margin ≈ 2 chars; no mouse-wheel/`zh`/`zl` scroll yet.
                if self.follow_h {
                    let margin = font_size * 2.;
                    let viewport = bounds.size.width - gutter_w;
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
                }
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

        // Search matches never cover the newline (`to_eol` is always false), so
        // both edges resolve through the shaped line like the selection above.
        let search = self
            .search
            .iter()
            .map(|h| {
                let x0 = shaped.x_for_index(caret_bytes(&self.text, h.start_col).0);
                let x1 = shaped.x_for_index(caret_bytes(&self.text, h.end_col).0);
                (x0, x1 - x0)
            })
            .collect();

        // A task row's box target: the pixel span of its `[ ]` bytes.
        let mut task = None;
        let mut byte = 0;
        for seg in &self.segments {
            if let Some(SpanKind::Task(checked)) = seg.kind {
                task =
                    Some((shaped.x_for_index(byte), shaped.x_for_index(byte + seg.len), checked));
                break;
            }
            byte += seg.len;
        }

        LinePrepaint {
            shaped,
            gutter,
            gutter_w,
            selection,
            search,
            caret,
            caret_glyph,
            task,
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
        let theme = *cx.global::<Theme>();
        // The gutter sits flush left and never scrolls; paint it first, outside
        // the text's clip so scrolled text can't bleed over it.
        if let Some(gutter) = &prepaint.gutter {
            let _ = gutter.paint(bounds.origin, line_height, window, cx);
        }
        // Text starts past the gutter and shifts left by the horizontal scroll
        // offset; clip to the area right of the gutter so left-overflow stops at
        // the gutter and right-overflow stops at the pane edge.
        let text_origin_x = bounds.origin.x + prepaint.gutter_w;
        // Code rows inset all their content (text, caret, highlights) by
        // CODE_MARGIN + CODE_PAD so it clears the band border; wrap width
        // shrank to match in `append_line_rows`.
        let pad = match self.decor {
            Some(RowDecor::CodeBand { .. }) => CODE_MARGIN + CODE_PAD,
            _ => Pixels::ZERO,
        };
        let ox = text_origin_x + pad - self.scroll_x.get();
        let text_bounds = Bounds::new(
            point(text_origin_x, bounds.origin.y),
            size(bounds.size.width - prepaint.gutter_w, bounds.size.height),
        );
        // Decorations paint first (beneath everything) and outside the text
        // mask — they pin to the pane edge, never scroll, and stay inside the
        // row horizontally, so they don't need the clip. Outside it, a band
        // row can bleed 1px into the row below: bordered/rounded quads render
        // with antialiased edges, and two abutting edges on a fractional
        // pixel boundary each blend with the backdrop, leaving a hairline
        // seam between rows — overlapping the opaque quads hides it.
        match self.decor {
            Some(RowDecor::CodeBand { top, bottom }) => {
                let edge = |on: bool| if on { px(1.) } else { Pixels::ZERO };
                let radius = |on: bool| if on { px(4.) } else { Pixels::ZERO };
                let bleed = if bottom { Pixels::ZERO } else { px(1.) };
                let band_bounds = Bounds::new(
                    point(text_bounds.origin.x + CODE_MARGIN, text_bounds.origin.y),
                    size(text_bounds.size.width - CODE_MARGIN * 2., text_bounds.size.height + bleed),
                );
                window.paint_quad(
                    fill(band_bounds, theme.code_bg)
                        .corner_radii(Corners {
                            top_left: radius(top),
                            top_right: radius(top),
                            bottom_right: radius(bottom),
                            bottom_left: radius(bottom),
                        })
                        .border_widths(Edges {
                            top: edge(top),
                            right: px(1.),
                            bottom: edge(bottom),
                            left: px(1.),
                        })
                        .border_color(theme.border),
                )
            }
            Some(RowDecor::QuoteBar) => window.paint_quad(fill(
                Bounds::new(text_bounds.origin, size(px(3.), line_height)),
                theme.muted,
            )),
            Some(RowDecor::Rule) => window.paint_quad(fill(
                Bounds::new(
                    point(text_origin_x, bounds.origin.y + (line_height - px(1.)) / 2.),
                    size(text_bounds.size.width, px(1.)),
                ),
                theme.border,
            )),
            None => {}
        }
        window.with_content_mask(Some(ContentMask { bounds: text_bounds }), |window| {
            // Paint order, bottom-up: search-match quads, selection
            // highlight, caret quad, the line, the inverted caret glyph, and
            // the task box over its transparent source bytes.
            for &(x, width) in &prepaint.search {
                let origin = point(ox + x, bounds.origin.y);
                window.paint_quad(fill(
                    Bounds::new(origin, size(width, line_height)),
                    theme.search_match,
                ));
            }
            if let Some((x, width)) = prepaint.selection {
                let origin = point(ox + x, bounds.origin.y);
                window.paint_quad(fill(
                    Bounds::new(origin, size(width, line_height)),
                    theme.selection,
                ));
            }
            let caret_paint = self.caret_paint.get();
            if let Some((x, width)) = prepaint.caret {
                if caret_paint != CaretPaint::Hidden {
                    let color = if caret_paint == CaretPaint::Dim {
                        theme.muted
                    } else {
                        theme.accent
                    };
                    let origin = point(ox + x, bounds.origin.y);
                    window.paint_quad(fill(
                        Bounds::new(origin, size(width, line_height)),
                        color,
                    ));
                }
            }
            let shaped = &prepaint.shaped;
            let _ = shaped.paint(point(ox, bounds.origin.y), line_height, window, cx);
            // A hidden caret's block quad isn't there, so keep the glyph in
            // its normal color instead of repainting it dark.
            if let Some((font_id, glyph_id, gx)) =
                prepaint.caret_glyph.filter(|_| caret_paint != CaretPaint::Hidden)
            {
                // Match the baseline `ShapedLine::paint` uses: line is vertically
                // centered, glyph sits on the baseline (`paint_glyph` y is baseline).
                let padding_top = (line_height - shaped.ascent - shaped.descent) / 2.;
                let baseline = point(ox + gx, bounds.origin.y + padding_top + shaped.ascent);
                let _ =
                    window.paint_glyph(baseline, font_id, glyph_id, shaped.font_size, theme.background);
            }
            // Task box centered over its `[ ]` span: outlined when unchecked,
            // accent-filled with a check when done. Whole-pixel origin — a
            // 1px border at a fractional x antialiases unevenly (one edge
            // crisp, the other ghosted).
            if let Some((x0, x1, checked)) = prepaint.task {
                let s = px(12.).min(line_height);
                let b = Bounds::new(
                    point(
                        (ox + (x0 + x1 - s) / 2.).round(),
                        (bounds.origin.y + (line_height - s) / 2.).round(),
                    ),
                    size(s, s),
                );
                if checked {
                    window.paint_quad(fill(b, theme.accent).corner_radii(px(3.)));
                    // Lucide check through the Phase-4 icon pipeline; its
                    // 24-viewBox padding insets the stroke, so it paints
                    // across the full box.
                    let _ = window.paint_svg(
                        b,
                        "icons/check.svg".into(),
                        TransformationMatrix::unit(),
                        theme.background,
                        cx,
                    );
                } else {
                    window.paint_quad(
                        outline(b, theme.muted, BorderStyle::default()).corner_radii(px(3.)),
                    );
                }
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

/// Map a line's flattened segments to styled runs covering the whole line —
/// `shape_line` drops glyphs unless the run lengths sum to the byte length. An
/// empty line keeps one zero-length default run, matching the prior behavior.
fn segments_to_runs(
    text: &str,
    segments: &[Segment],
    font: &Font,
    fg: Hsla,
    theme: &Theme,
) -> Vec<TextRun> {
    if segments.is_empty() {
        return vec![run(font, text.len(), fg)];
    }
    segments
        .iter()
        .map(|seg| {
            let (color, weight, style, background_color) = segment_style(seg.kind, fg, theme);
            let mut font = font.clone();
            font.weight = weight;
            font.style = style;
            TextRun { len: seg.len, font, color, background_color, underline: None, strikethrough: None }
        })
        .collect()
}

/// Font-size multiplier for a concealed line's segments: H1 1.2×, H2 1.1×,
/// everything else (H3+ included) body size — the fixed 22/15 line box caps
/// how large a row can shape. A heading whose every byte is covered by a
/// higher-priority span (e.g. `# **all bold**`) flattens with no Heading
/// segment left and stays at body size — rare enough to ignore.
fn heading_scale(segments: &[Segment]) -> f32 {
    let level = segments.iter().find_map(|s| match s.kind {
        Some(SpanKind::Heading(n)) => Some(n),
        _ => None,
    });
    match level {
        Some(1) => 1.2,
        Some(2) => 1.1,
        _ => 1.0,
    }
}

/// Block-level paint decoration for a row. Applied only to concealed rows —
/// the cursor line and raw view show plain source, like conceal.
#[derive(Clone, Copy, Debug, PartialEq)]
enum RowDecor {
    /// `code_bg` band behind a fence/code line, inset `CODE_MARGIN` from the
    /// pane edges and drawn with a 1px side border. `top`/`bottom` mark the
    /// block's first/last line, which
    /// close the border and round its corners; `append_line_rows` further
    /// restricts them to the first/last visual row of a wrapped line.
    CodeBand { top: bool, bottom: bool },
    /// 3px bar at the left edge of a blockquote line (its `>` conceals).
    QuoteBar,
    /// Hairline across the row replacing a `---`/`***`/`___` line.
    Rule,
}

/// Horizontal margin of a code band's quad from the pane edges.
const CODE_MARGIN: Pixels = px(8.);

/// Text inset inside a code band, so content clears the border. Band text
/// sits `CODE_MARGIN + CODE_PAD` from the pane edge; wrap width shrinks by
/// twice that sum for band lines, keeping wrapped rows inside the border.
const CODE_PAD: Pixels = px(8.);

/// Decoration for line `line`, from its spans. The scanner pushes a line's
/// role span first, so the leading span's kind decides — which also covers
/// empty in-fence lines (their zero-length `CodeText` span still leads) where
/// the flattened segments would be empty. A code band's `top`/`bottom` come
/// from whether the neighboring lines are in-band.
// ponytail: back-to-back fenced blocks (no blank line between) merge into one
// band; track fence open/close state here if that ever reads wrong.
fn row_decor(spans: &[Vec<Span>], line: usize) -> Option<RowDecor> {
    let lead = |i: usize| spans.get(i).and_then(|s| s.first()).map(|s| s.kind);
    match lead(line) {
        Some(SpanKind::CodeText | SpanKind::CodeFence) => {
            let band =
                |i: usize| matches!(lead(i), Some(SpanKind::CodeText | SpanKind::CodeFence));
            Some(RowDecor::CodeBand {
                top: line == 0 || !band(line - 1),
                bottom: !band(line + 1),
            })
        }
        Some(SpanKind::BlockQuote) => Some(RowDecor::QuoteBar),
        Some(SpanKind::Rule) => Some(RowDecor::Rule),
        _ => None,
    }
}

/// The two fence lines (`[open, close]`) of the fenced block containing
/// `line`, `[None, None]` when it isn't in one; `close` is `None` while the
/// block is unclosed at EOF. Scans leading span kinds from the top, tracking
/// open/close state like the scanner — adjacent blocks make a purely local
/// opener-vs-closer test ambiguous. Revealing these along with the cursor
/// line keeps a block's fences visible while editing inside it.
// ponytail: O(line) rescan per caret move (index + `first` per line); track
// state incrementally if a profile ever blames it.
fn fence_block(spans: &[Vec<Span>], line: usize) -> [Option<usize>; 2] {
    let lead = |i: usize| spans.get(i).and_then(|s| s.first()).map(|s| s.kind);
    let mut open = None;
    for i in 0..=line {
        if lead(i) == Some(SpanKind::CodeFence) {
            open = match open {
                // `line` itself is this block's closing fence.
                Some(top) if i == line => return [Some(top), Some(i)],
                Some(_) => None, // a block closed above `line`
                None => Some(i),
            };
        }
    }
    let Some(top) = open else { return [None, None] };
    let close = (line + 1..spans.len()).find(|&i| lead(i) == Some(SpanKind::CodeFence));
    [Some(top), close]
}

/// Visual style for a flattened segment: (color, weight, slant, background).
/// `None` is default body text.
fn segment_style(
    kind: Option<SpanKind>,
    fg: Hsla,
    theme: &Theme,
) -> (Hsla, FontWeight, FontStyle, Option<Hsla>) {
    let normal = (fg, FontWeight::NORMAL, FontStyle::Normal, None);
    let Some(kind) = kind else { return normal };
    match kind {
        SpanKind::Heading(_) => (theme.heading, FontWeight::BOLD, FontStyle::Normal, None),
        SpanKind::Strong => (strong_color(fg), FontWeight::BOLD, FontStyle::Normal, None),
        SpanKind::Code | SpanKind::CodeText | SpanKind::CodeFence => {
            (theme.code, FontWeight::NORMAL, FontStyle::Normal, Some(theme.code_bg))
        }
        SpanKind::Link => (theme.link, FontWeight::NORMAL, FontStyle::Normal, None),
        SpanKind::BlockQuote => (theme.muted, FontWeight::NORMAL, FontStyle::Italic, None),
        SpanKind::Frontmatter | SpanKind::Marker | SpanKind::Rule => {
            (theme.muted, FontWeight::NORMAL, FontStyle::Normal, None)
        }
        // Concealed rows paint a real box over the `[ ]` bytes: the glyphs
        // shape (reserving the box's width in the layout) but paint
        // transparent. Source view remaps Task to Marker before this runs.
        SpanKind::Task(_) => (Hsla { a: 0., ..fg }, FontWeight::NORMAL, FontStyle::Normal, None),
        SpanKind::ListItem => normal,
    }
}

// ponytail: gpui's `layout_line` (text_system.rs) infers "same font" from
// "same decoration" (color/underline/strikethrough) and merges adjacent runs
// on that basis without checking weight — so a Strong run flanked by same-`fg`
// text (exactly what concealment produces once the differently-colored `**`
// marker is dropped) gets folded into the surrounding regular-weight run and
// silently loses its bold. Nudging alpha by an imperceptible amount keeps the
// two runs "different" so gpui resolves the bold font instead of assuming it's
// unchanged. Remove this workaround (and the color nudge it does) whichever
// comes first: (1) `LineElement` switches its shaping call from `shape_line`
// to `shape_text` — likely when line wrapping is implemented, since
// `shape_text`'s `process_line` already resolves fonts per-run correctly and
// this bug can't occur there; or (2) a gpui upgrade fixes `layout_line` to
// compare fonts directly instead of inferring sameness from decoration
// (reported upstream to zed-industries/zed).
fn strong_color(fg: Hsla) -> Hsla {
    Hsla { a: (fg.a - 0.001).max(0.0), ..fg }
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

/// The only difference between two row keys is where the caret sits: same
/// content, same mode, no selection, no highlighted search. Then only the old
/// and new cursor lines can render differently (conceal swap, caret quad,
/// gutter emphasis), so the cached rows can be patched instead of rebuilt.
fn caret_only_change(old: &RowsKey, new: &RowsKey) -> bool {
    old.revision == new.revision
        && old.wrap_width == new.wrap_width
        && old.mode == new.mode
        && old.sel.is_none()
        && new.sel.is_none()
        && old.q.is_empty()
        && new.q.is_empty()
}

/// Greedy word wrap for plain ASCII text in a monospace font: row-start byte
/// offsets for rows of at most `cols` chars, breaking after the last space in
/// the row, or mid-word when one word overruns a whole row. Only valid where
/// byte == char == column — the caller gates on ASCII.
// ponytail: a break can leave a space at a row edge — cosmetic, vim-like.
fn wrap_columns(text: &str, cols: usize) -> Vec<usize> {
    let cols = cols.max(1);
    let bytes = text.as_bytes();
    let mut starts = vec![0];
    let (mut row_start, mut last_space) = (0usize, None);
    let mut i = 0;
    while i < bytes.len() {
        if i - row_start == cols {
            let next = last_space.map_or(i, |s: usize| s + 1);
            starts.push(next);
            row_start = next;
            last_space = None;
            i = next;
            continue;
        }
        if bytes[i] == b' ' {
            last_space = Some(i);
        }
        i += 1;
    }
    starts
}

/// Slice a line's styling segments down to the byte range `[b0, b1)` of one
/// wrapped visual row. Kinds are kept; lengths clip to the range.
fn slice_segments(segments: &[Segment], b0: usize, b1: usize) -> Vec<Segment> {
    let mut out = Vec::new();
    let mut pos = 0;
    for seg in segments {
        let (s, e) = (pos, pos + seg.len);
        pos = e;
        let (a, b) = (s.max(b0), e.min(b1));
        if a < b {
            out.push(Segment { len: b - a, kind: seg.kind });
        }
    }
    out
}

/// Clip a line-level column highlight to one visual row's char range
/// `[c0, c1)`, re-based to row-local columns. `to_eol` (fill past the last
/// char) only survives on the line's last row, where the line actually ends;
/// `None` when nothing of the span lands on this row.
fn clip_row_highlight(h: Highlight, c0: usize, c1: usize, last_row: bool) -> Option<Highlight> {
    let to_eol = h.to_eol && last_row;
    let a = h.start_col.max(c0);
    let b = h.end_col.min(c1).max(a);
    if a >= b && !to_eol {
        return None;
    }
    Some(Highlight { start_col: a - c0, end_col: b - c0, to_eol })
}

/// Remap a source-column highlight onto a concealed line: char col → source
/// byte, through the conceal map, → display char col. `None` when the span was
/// entirely concealed away (nothing visible to highlight).
fn remap_highlight(h: Highlight, source: &str, c: &markdown::Concealed) -> Option<Highlight> {
    let d0 = c.map[caret_bytes(source, h.start_col).0];
    let d1 = c.map[caret_bytes(source, h.end_col).0];
    if d0 >= d1 && !h.to_eol {
        return None;
    }
    Some(Highlight {
        start_col: c.text[..d0].chars().count(),
        end_col: c.text[..d1].chars().count(),
        to_eol: h.to_eol,
    })
}

/// Count-preserving case fold for search: a char's first lowercase char. Full
/// `to_lowercase()` can change char counts (ß→ss), which would corrupt the
/// char-offset math that matches share with `Document`.
fn fold(c: char, sensitive: bool) -> char {
    if sensitive {
        c
    } else {
        c.to_lowercase().next().unwrap_or(c)
    }
}

/// Whether `query` matches case-sensitively under the config: sensitive unless
/// `ignorecase`, except `smartcase` re-sensitizes an uppercase-bearing query.
fn search_sensitive(query: &str, cfg: &SearchConfig) -> bool {
    !cfg.ignorecase || (cfg.smartcase && query.chars().any(char::is_uppercase))
}

/// Every match of `query` as absolute char ranges `[start, end)`, scanned per
/// line (a literal one-line query can't span newlines). Overlapping matches
/// step by one char, like vim.
// ponytail: naive O(chars × query) window scan; notes-sized docs make it cheap.
// A folded-copy + memmem search if a profile ever says otherwise.
fn find_matches(rope: &ropey::Rope, query: &str, sensitive: bool) -> Vec<(usize, usize)> {
    let needle: Vec<char> = query.chars().map(|c| fold(c, sensitive)).collect();
    if needle.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for i in 0..rope.len_lines() {
        let start = rope.line_to_char(i);
        // The trailing '\n' rides along harmlessly — the needle never has one.
        let hay: Vec<char> = rope.line(i).chars().map(|c| fold(c, sensitive)).collect();
        for j in 0..hay.len().saturating_sub(needle.len() - 1) {
            if hay[j..j + needle.len()] == needle[..] {
                out.push((start + j, start + j + needle.len()));
            }
        }
    }
    out
}

/// Index into `matches` of the nearest match starting strictly after `from`
/// (strictly before, when `backward`), wrapping around if `wrap`; the second
/// value reports that it wrapped. Strictness is what makes `n` on a match
/// start jump to the *next* one.
fn next_match(
    matches: &[(usize, usize)],
    from: usize,
    backward: bool,
    wrap: bool,
) -> Option<(usize, bool)> {
    if backward {
        match matches.iter().rposition(|&(s, _)| s < from) {
            Some(i) => Some((i, false)),
            None => (wrap && !matches.is_empty()).then(|| (matches.len() - 1, true)),
        }
    } else {
        match matches.iter().position(|&(s, _)| s > from) {
            Some(i) => Some((i, false)),
            None => (wrap && !matches.is_empty()).then_some((0, true)),
        }
    }
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
    use super::{
        caret_bytes, clip_row_highlight, fence_block, filter_items, find_matches, match_buffer,
        next_match, heading_scale, rel_display, remap_highlight, resolve, resolve_link,
        row_decor, search_sensitive, segment_style, slice_segments, unique_dest, wrap_columns,
        Highlight, PickItem, RowDecor,
    };
    use crate::config::Search as SearchConfig;
    use crate::markdown::{self, SpanKind};
    use gpui::Hsla;
    use ropey::Rope;
    use std::path::{Path, PathBuf};

    #[test]
    fn unique_dest_numbers_taken_names() {
        let dir = std::env::temp_dir().join("darknotes_unique_dest_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Free name: used as-is. Taken: numbered before the extension,
        // skipping numbers that are themselves taken.
        assert_eq!(unique_dest(&dir, "a.md"), dir.join("a.md"));
        std::fs::write(dir.join("a.md"), "").unwrap();
        assert_eq!(unique_dest(&dir, "a.md"), dir.join("a 2.md"));
        std::fs::write(dir.join("a 2.md"), "").unwrap();
        assert_eq!(unique_dest(&dir, "a.md"), dir.join("a 3.md"));
        // Extension-less names (folders) number at the end.
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        assert_eq!(unique_dest(&dir, "sub"), dir.join("sub 2"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wrap_columns_breaks_words_and_walls() {
        // Word break: "hello worl|d…" overflows at 10; the row breaks after
        // the space, so row 2 starts at 'w'.
        assert_eq!(wrap_columns("hello world foo", 10), vec![0, 6]);
        // No spaces: hard breaks every `cols`.
        assert_eq!(wrap_columns("aaaaaaaaaaaa", 5), vec![0, 5, 10]);
        // Exact fit and empty: single row.
        assert_eq!(wrap_columns("aaaaa", 5), vec![0]);
        assert_eq!(wrap_columns("", 5), vec![0]);
        // cols 0 clamps to 1 instead of looping forever.
        assert_eq!(wrap_columns("ab", 0), vec![0, 1]);
    }

    #[test]
    fn slice_segments_clips_to_row_range() {
        use markdown::Segment;
        let segs = vec![
            Segment { len: 3, kind: Some(SpanKind::Marker) },
            Segment { len: 5, kind: None },
        ];
        // Row [2, 6): one byte of the marker, three of the body.
        assert_eq!(
            slice_segments(&segs, 2, 6),
            vec![Segment { len: 1, kind: Some(SpanKind::Marker) }, Segment { len: 3, kind: None }]
        );
        // A row past the segments' end is unstyled.
        assert!(slice_segments(&segs, 8, 12).is_empty());
    }

    #[test]
    fn clip_row_highlight_splits_across_rows() {
        // A line wrapped at col 10; selection [4, 14) reaching the newline.
        let h = Highlight { start_col: 4, end_col: 14, to_eol: true };
        // First row [0,10): local [4,10), not at line end → no to_eol fill.
        assert!(matches!(
            clip_row_highlight(h, 0, 10, false),
            Some(Highlight { start_col: 4, end_col: 10, to_eol: false })
        ));
        // Last row [10,16): local [0,4), keeps the to_eol fill.
        assert!(matches!(
            clip_row_highlight(h, 10, 16, true),
            Some(Highlight { start_col: 0, end_col: 4, to_eol: true })
        ));
        // A span the row misses entirely.
        let short = Highlight { start_col: 0, end_col: 3, to_eol: false };
        assert!(clip_row_highlight(short, 10, 16, true).is_none());
        // Zero-width span at EOL survives through to_eol (linewise selection
        // covering a wrapped line's newline).
        let eol = Highlight { start_col: 14, end_col: 14, to_eol: true };
        assert!(matches!(
            clip_row_highlight(eol, 10, 16, true),
            Some(Highlight { start_col: 4, end_col: 4, to_eol: true })
        ));
    }

    #[test]
    fn strong_color_differs_from_plain_text() {
        // Strong must not share an exact color with plain body text: gpui's
        // layout_line treats equal-decoration adjacent runs as equal-font and
        // merges them, dropping bold weight when concealment leaves Strong
        // flanked by plain `fg` text (see `strong_color`'s doc comment).
        let fg: Hsla = gpui::rgb(0xcccccc).into();
        let theme = crate::theme::Theme::by_name("dark").unwrap();
        let (strong_color, _, _, _) = segment_style(Some(SpanKind::Strong), fg, &theme);
        let (plain_color, _, _, _) = segment_style(None, fg, &theme);
        assert_ne!(strong_color, plain_color);
    }

    #[test]
    fn heading_scale_steps_down_by_level() {
        let seg = |kind| markdown::Segment { len: 4, kind };
        assert_eq!(heading_scale(&[seg(Some(SpanKind::Heading(1)))]), 1.2);
        // Level wins even after inline spans (e.g. Strong) split the line.
        assert_eq!(
            heading_scale(&[seg(Some(SpanKind::Heading(2))), seg(Some(SpanKind::Strong))]),
            1.1
        );
        assert_eq!(heading_scale(&[seg(Some(SpanKind::Heading(3)))]), 1.0);
        assert_eq!(heading_scale(&[seg(None)]), 1.0);
        assert_eq!(heading_scale(&[]), 1.0);
    }

    #[test]
    fn fence_block_finds_enclosing_fences() {
        // 0 a, 1 open, 2 code, 3 close, 4 b, 5 open, 6 code (unclosed)
        let spans = markdown::parse(&Rope::from_str("a\n```\ncode\n```\nb\n```\nx\n"));
        assert_eq!(fence_block(&spans, 0), [None, None]);
        for line in 1..=3 {
            assert_eq!(fence_block(&spans, line), [Some(1), Some(3)], "line {line}");
        }
        assert_eq!(fence_block(&spans, 4), [None, None]);
        assert_eq!(fence_block(&spans, 6), [Some(5), None]); // unclosed at EOF
        // Adjacent blocks: an opener right after a closer keeps its role.
        let spans = markdown::parse(&Rope::from_str("```\n```\n```\nx\n```\n"));
        assert_eq!(fence_block(&spans, 1), [Some(0), Some(1)]);
        assert_eq!(fence_block(&spans, 3), [Some(2), Some(4)]);
    }

    #[test]
    fn row_decor_from_leading_span() {
        let spans = markdown::parse(&Rope::from_str("# h\n> q\n```\ncode\n\n```\nx\n---\n"));
        let band = |top, bottom| Some(RowDecor::CodeBand { top, bottom });
        let expect = [
            None,                     // heading
            Some(RowDecor::QuoteBar), // > q
            band(true, false),        // opening fence
            band(false, false),       // code
            band(false, false),       // empty in-fence line (zero-len span)
            band(false, true),        // closing fence
            None,                     // plain text
            Some(RowDecor::Rule),     // ---
        ];
        for (i, want) in expect.iter().enumerate() {
            assert_eq!(row_decor(&spans, i), *want, "line {i}");
        }
    }

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

    #[test]
    fn find_matches_folds_case_and_counts_chars() {
        // Offsets are char-based: 'é' is one char, two bytes.
        let rope = Rope::from_str("Foo fOO\néfoo\n");
        assert_eq!(find_matches(&rope, "foo", false), vec![(0, 3), (4, 7), (9, 12)]);
        // Sensitive keeps only the exact-case match.
        assert_eq!(find_matches(&rope, "foo", true), vec![(9, 12)]);
        // Overlapping matches step by one; the empty query matches nothing.
        let rope = Rope::from_str("aaaa");
        assert_eq!(find_matches(&rope, "aa", true), vec![(0, 2), (1, 3), (2, 4)]);
        assert!(find_matches(&rope, "", true).is_empty());
    }

    #[test]
    fn search_sensitive_matrix() {
        let cfg = |ignorecase, smartcase| SearchConfig {
            ignorecase,
            smartcase,
            ..Default::default()
        };
        assert!(search_sensitive("foo", &cfg(false, false))); // ignorecase off
        assert!(!search_sensitive("foo", &cfg(true, false)));
        assert!(!search_sensitive("foo", &cfg(true, true))); // all-lower stays loose
        assert!(search_sensitive("Foo", &cfg(true, true))); // uppercase re-sensitizes
        assert!(!search_sensitive("Foo", &cfg(true, false))); // …only with smartcase
    }

    #[test]
    fn highlight_remaps_onto_concealed_text() {
        let line = "**templates** more";
        let segs = markdown::flatten(line.len(), &markdown::parse(&Rope::from_str(line))[0]);
        let c = markdown::conceal(line, &segs);
        assert_eq!(c.text, "templates more");
        // "templates" sits at source cols 2..11; concealed it starts the line.
        let h = Highlight { start_col: 2, end_col: 11, to_eol: false };
        let r = remap_highlight(h, line, &c).unwrap();
        assert_eq!((r.start_col, r.end_col), (0, 9));
        // A span entirely inside a dropped marker has nothing visible left.
        let h = Highlight { start_col: 0, end_col: 2, to_eol: false };
        assert!(remap_highlight(h, line, &c).is_none());

        // Display columns are chars, not bytes, on multibyte lines.
        let line = "**é** x";
        let segs = markdown::flatten(line.len(), &markdown::parse(&Rope::from_str(line))[0]);
        let c = markdown::conceal(line, &segs);
        assert_eq!(c.text, "é x");
        let h = Highlight { start_col: 2, end_col: 3, to_eol: false }; // the é
        let r = remap_highlight(h, line, &c).unwrap();
        assert_eq!((r.start_col, r.end_col), (0, 1));
    }

    #[test]
    fn next_match_is_strict_and_wraps() {
        let m = [(0, 2), (5, 7), (10, 12)];
        // Strictly after: sitting on a match start jumps to the next one.
        assert_eq!(next_match(&m, 0, false, true), Some((1, false)));
        assert_eq!(next_match(&m, 6, false, true), Some((2, false)));
        // Past the last: wrap around or fail.
        assert_eq!(next_match(&m, 10, false, true), Some((0, true)));
        assert_eq!(next_match(&m, 10, false, false), None);
        // Backward mirrors it.
        assert_eq!(next_match(&m, 10, true, true), Some((1, false)));
        assert_eq!(next_match(&m, 0, true, true), Some((2, true)));
        assert_eq!(next_match(&m, 0, true, false), None);
        assert_eq!(next_match(&[], 0, false, true), None);
    }

    #[test]
    fn rel_display_strips_root_and_md() {
        let root = Path::new("/v");
        assert_eq!(rel_display(root, Path::new("/v/sub/note.md")), "sub/note");
        assert_eq!(rel_display(root, Path::new("/v/notes.v2.md")), "notes.v2");
        // Non-.md extensions are kept.
        assert_eq!(rel_display(root, Path::new("/v/sql/schema.sql")), "sql/schema.sql");
    }

    #[test]
    fn match_buffer_number_exact_partial() {
        let names: Vec<String> =
            ["projects/ideas", "daily/today", "daily/ideas-old", "[No Name]"]
                .map(String::from)
                .into();
        // 1-based tab numbers, range-checked.
        assert_eq!(match_buffer("2", &names), Ok(1));
        assert!(match_buffer("0", &names).unwrap_err().starts_with("E86"));
        assert!(match_buffer("5", &names).unwrap_err().starts_with("E86"));
        // Exact full name, then exact basename, win over substring hits.
        assert_eq!(match_buffer("projects/ideas", &names), Ok(0));
        assert_eq!(match_buffer("ideas", &names), Ok(0)); // basename beats "ideas-old" substring
        assert_eq!(match_buffer("[No Name]", &names), Ok(3));
        // Unique substring (case-insensitive) matches; ambiguous/missing error.
        assert_eq!(match_buffer("TODAY", &names), Ok(1));
        assert!(match_buffer("daily", &names).unwrap_err().starts_with("E93"));
        assert!(match_buffer("nope", &names).unwrap_err().starts_with("E94"));
    }

    #[test]
    fn resolve_link_matches_path_or_stem_case_insensitively() {
        let root = Path::new("/v");
        let files: Vec<PathBuf> =
            ["/v/projects/ideas.md", "/v/daily/today.md"].map(PathBuf::from).into();
        // Vault-relative path (`.md` dropped) and bare stem both resolve.
        assert_eq!(resolve_link(root, &files, "projects/ideas"), Some(files[0].clone()));
        assert_eq!(resolve_link(root, &files, "ideas"), Some(files[0].clone()));
        assert_eq!(resolve_link(root, &files, "TODAY"), Some(files[1].clone()));
        assert_eq!(resolve_link(root, &files, "nope"), None);
    }
}
