//! Open buffers and the files behind them: the tab list, buffer switching
//! (`:b`/`:bn`/gt-style nav), `:e`/`:enew`/`:today`, closing, and the vault
//! path helpers (name → path resolution, wikilink targets, daily notes).
//!
//! A child module of `editor` so methods can touch private `Editor` state.

use std::path::{Component, Path, PathBuf};

use gpui::{Context, Window};

use crate::document::Document;

use super::{Editor, FilePrompt, PendingView, PromptAction};

/// One open buffer: a `Document` plus tab metadata.
pub(super) struct Buffer {
    pub(super) doc: Document,
    /// VS Code-style preview tab: the next file open replaces this buffer
    /// instead of adding a tab; the first edit commits it (clears the flag).
    /// At most one preview buffer exists at a time.
    pub(super) preview: bool,
    /// Document line that was at the top of the viewport when this buffer was
    /// last active, so switching back restores the view instead of recentering.
    /// Only meaningful while inactive — the active buffer's view lives in the
    /// shared scroll handle, which `Editor::top_line` reads.
    pub(super) top: usize,
}

impl Editor {
    pub(super) fn doc(&self) -> &Document {
        &self.buffers[self.active].doc
    }

    pub(super) fn doc_mut(&mut self) -> &mut Document {
        &mut self.buffers[self.active].doc
    }

    /// Open a file by path (sidebar click / Enter / switcher / `:e`): switch
    /// to its buffer if one is already open, else show it in the preview slot.
    /// Records where we left, so `Ctrl-O` returns to it — every by-path open
    /// funnels through here, so no open site can forget to.
    pub(super) fn open_path(&mut self, path: PathBuf, window: &mut Window) {
        self.push_jump(); // before the switch: reads the caret we're leaving
        self.open_path_quiet(path, window);
    }

    /// `open_path` without recording — for the `Ctrl-O`/`Ctrl-I` restore, which
    /// must not rewrite the history it is walking.
    pub(super) fn open_path_quiet(&mut self, path: PathBuf, window: &mut Window) {
        if let Some(i) = self.buffers.iter().position(|b| b.doc.path() == Some(path.as_path())) {
            self.activate(i, window);
            return;
        }
        self.show_preview(open_or_empty(&path), window);
    }

    /// Make buffer `i` the active one: reset vim/keymap state, restore its
    /// view, and refocus the editor so keys keep flowing after a click or
    /// command. Leaves `alternate` untouched — `activate` (a user-facing
    /// switch) records that.
    pub(super) fn switch_to(&mut self, i: usize, window: &mut Window) {
        // Park the outgoing buffer's view on it before `active` moves, so
        // coming back lands where we left. Only on a real switch: an in-place
        // doc replacement (`show_preview`, `:e!`) arrives with `i == active`
        // having already set the view it wants. Bounds-checked —
        // `close_buffer` gets here with `active` still on the slot it removed.
        if i != self.active && self.active < self.buffers.len() {
            let top = self.top_line();
            self.buffers[self.active].top = top;
        }
        self.active = i;
        self.vim.reset(); // clears transient state, keeps config (tab width)
        self.keymap.clear(); // a pending binding sequence dies with the buffer
        self.seq_timer = None;
        // A mid-prompt buffer switch (Ctrl-P) must not restore a stale caret
        // into the new buffer. The query itself survives — vim search is global.
        self.search.origin = None;
        // The scroll handle is shared across buffers and still holds the old
        // offset; put this buffer's own view back. Deferred to the render pass —
        // the line's visual row needs this buffer's wrap map.
        self.pending_scroll =
            Some(PendingView { top: self.buffers[i].top, caret: self.doc().caret_offset() });
        self.reveal_current();
        window.focus(&self.focus);
        self.save_session();
    }

    /// Switch to buffer `i`, recording where we came from for `Ctrl-6`/`:b #`.
    ///
    /// Deliberately *not* a jumplist entry: cycling between open buffers
    /// (`Ctrl-6`, `:bn`, `gt`) is a move between containers, not a jump to a
    /// position, and recording it would make `Ctrl-O` replay tab visits.
    /// By-path opens record in `open_path`.
    pub(super) fn activate(&mut self, i: usize, window: &mut Window) {
        if i != self.active {
            self.alternate = Some(self.active);
        }
        self.switch_to(i, window);
    }

    /// Show `doc` in the preview slot: replace the existing preview buffer, or
    /// append a new preview tab.
    pub(super) fn show_preview(&mut self, doc: Document, window: &mut Window) {
        let buf = Buffer { doc, preview: true, top: 0 };
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

    /// Guard before discarding the active buffer's changes (the in-place `:e`
    /// reload): `true` if it's safe, else sets the vim E37 message and returns
    /// `false`. `bang` (`:e!`) forces it through.
    pub(super) fn may_discard(&mut self, bang: bool) -> bool {
        if self.doc().is_dirty() && !bang {
            self.message = Some("E37: No write since last change (add ! to override)".into());
            false
        } else {
            true
        }
    }

    /// `:e {path}` — open `path` for editing. A nonexistent file opens as a
    /// blank buffer that `:w` creates (`Document::open` is vim-lazy). Relative
    /// names resolve under the vault root, so a new note lands in — and shows up
    /// in — the vault. Opening lands in a buffer, so nothing is discarded — the
    /// exception is `:e` on the already-open file, vim's reload-from-disk,
    /// which drops unsaved changes only with `bang` (`:e!`).
    pub(super) fn edit(&mut self, name: &str, bang: bool, window: &mut Window) {
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
            // The fresh Document's caret is back at the top, so the view goes
            // with it; a parked `top` from this tab's last switch-away would
            // strand the caret off screen.
            self.buffers[self.active].top = 0;
            self.switch_to(self.active, window);
            return;
        }
        self.open_path(path, window);
    }

    /// `:enew` — a blank, unnamed buffer in the preview slot; name it on the
    /// first `:w {name}`.
    pub(super) fn enew(&mut self, window: &mut Window) {
        self.show_preview(Document::new(""), window);
    }

    /// `:today` — open today's daily note, creating `daily/YYYY-MM-DD.md`
    /// (seeded from `templates/daily.md` when present) on first use.
    pub(super) fn today(&mut self, window: &mut Window) {
        self.open_new_note("daily", "%Y-%m-%d", "templates/daily.md", None, window);
    }

    /// Create-if-missing and open a note, the shared body of `:today` and every
    /// `[notes.*]` command. `arg` fills a `{arg}` placeholder in `name` — see
    /// `create_note`.
    pub(super) fn open_new_note(
        &mut self,
        dir: &str,
        name: &str,
        template: &str,
        arg: Option<&str>,
        window: &mut Window,
    ) {
        match create_note(&self.vault.root, dir, name, template, arg) {
            Ok(path) => {
                self.rescan_vault(); // a first-of-its-kind note must show in the sidebar
                self.open_path(path, window);
            }
            Err(e) => self.message = Some(format!("create failed: {e}")),
        }
    }

    /// `:capture {text}` — append a task line to the vault's `inbox.md`,
    /// creating it on first use. Nothing about the current buffer moves: no
    /// tab, no caret, no focus. That is the whole feature — capture that costs
    /// navigation doesn't get used.
    ///
    /// Text that already opens a markdown list item is appended verbatim, so a
    /// bare thought (`- idea: …`) rides the same command as a task.
    pub(super) fn capture(&mut self, text: &str) {
        let path = self.vault.root.join("inbox.md");
        let is_new = !path.exists();
        let line = if text.starts_with(['-', '*', '+']) {
            text.to_string()
        } else {
            format!("- [ ] {text}")
        };
        // ponytail: writes disk, not the buffer. An `inbox.md` open and clean
        // reloads via the watcher; open and dirty gets W12 and shows the line
        // only after the user resolves it. Same contract as any outside edit —
        // and the same path a `darknotes capture` CLI would take.
        match append_line(&path, &line) {
            Ok(()) => {
                if is_new {
                    self.rescan_vault(); // a first-ever capture must show in the sidebar
                }
                self.message = Some("captured to inbox.md".into());
            }
            Err(e) => self.message = Some(format!("capture failed: {e}")),
        }
    }

    /// Tab / `:b`-match display name: vault-relative path (`.md` dropped) for
    /// pathed buffers, `[No Name]` for scratch.
    pub(super) fn buffer_display(&self, b: &Buffer) -> String {
        b.doc
            .path()
            .map(|p| rel_display(&self.vault.root, p))
            .unwrap_or_else(|| "[No Name]".into())
    }

    /// `:b {arg}` — switch buffer by tab number, `#` (alternate), or name.
    pub(super) fn buffer_switch(&mut self, arg: Option<&str>, window: &mut Window) {
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
    pub(super) fn buffer_next(&mut self, window: &mut Window) {
        self.activate((self.active + 1) % self.buffers.len(), window);
    }

    /// `:bp` — cycle backward through the tabs, wrapping.
    pub(super) fn buffer_prev(&mut self, window: &mut Window) {
        let n = self.buffers.len();
        self.activate((self.active + n - 1) % n, window);
    }

    /// `Ctrl-6` / `:b #` — bounce to the previously active buffer.
    pub(super) fn buffer_alternate(&mut self, window: &mut Window) {
        match self.alternate {
            Some(i) => self.activate(i, window),
            None => self.message = Some("E23: No alternate file".into()),
        }
    }

    /// Close tab `i` (`:bd` semantics): refuse while dirty unless `bang`. The
    /// last tab is replaced by a scratch buffer — `buffers` is never empty.
    pub(super) fn close_buffer(&mut self, i: usize, bang: bool, window: &mut Window) {
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
            self.buffers.push(Buffer { doc: Document::new(""), preview: true, top: 0 });
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
    pub(super) fn quit(&mut self, bang: bool, cx: &mut Context<Self>) {
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

    /// Write every dirty buffer, stopping at the first failure so the error
    /// reaches the user instead of a partial save reporting success. The `Err`
    /// names the buffer, which is the only way to tell which one stopped it.
    ///
    /// Never forces: a note that changed on disk must not be overwritten just
    /// because the user is quitting. They resolve it with `:w!` or `:e!`.
    pub(super) fn save_all_dirty(&mut self) -> Result<(), String> {
        for i in 0..self.buffers.len() {
            if !self.buffers[i].doc.is_dirty() {
                continue;
            }
            if let Err(e) = self.buffers[i].doc.save(false) {
                return Err(format!("{}: {e}", self.buffer_display(&self.buffers[i])));
            }
        }
        Ok(())
    }

    /// Window-close guard (the titlebar X): `true` lets the close through.
    /// Unsaved buffers raise the quit confirmation and veto it — the same
    /// policy as `:q`, except the save/discard choice is offered here instead
    /// of left as an E162 for the user to resolve by hand.
    pub(crate) fn confirm_close(&mut self, cx: &mut Context<Self>) -> bool {
        // Clicking X again while the dialog is up must not stack a second one.
        if matches!(self.prompt.as_ref().map(|p| &p.action), Some(PromptAction::ConfirmQuit)) {
            return false;
        }
        let dirty: Vec<String> = self
            .buffers
            .iter()
            .filter(|b| b.doc.is_dirty())
            .map(|b| self.buffer_display(b))
            .collect();
        let Some(label) = unsaved_label(&dirty) else {
            self.save_session();
            return true;
        };
        self.prompt =
            Some(FilePrompt { label, input: String::new(), action: PromptAction::ConfirmQuit });
        cx.notify(); // the dialog is a render-state change, not a key event
        false
    }
}

/// The quit dialog's question for buffers named `names`, or `None` when there
/// is nothing unsaved and the window may just close. One note is named — it is
/// the common case, and this is the last word before a possible discard, so it
/// has to say what is at stake. Past one, a count reads better than a list and
/// keeps the dialog a fixed size.
fn unsaved_label(names: &[String]) -> Option<String> {
    match names {
        [] => None,
        [one] => Some(format!("1 unsaved note: {one}")),
        many => Some(format!("{} unsaved notes", many.len())),
    }
}

/// Bare names get a `.md` extension; anything with an extension is left alone.
/// Deliberate even though the vault holds mixed file types: `:e foo` stays a
/// quick note creator; opening `schema.sql` means typing its real name (or
/// picking it from the sidebar/switcher, which pass full paths).
pub(super) fn with_md_ext(name: &str) -> PathBuf {
    let p = PathBuf::from(name);
    if p.extension().is_none() {
        p.with_extension("md")
    } else {
        p
    }
}

/// First non-existing `dir/name`, numbering before the extension when taken
/// (`note 2.md`, `note 3.md`, …) — so moving into `.trash` never collides.
pub(super) fn unique_dest(dir: &Path, name: &str) -> PathBuf {
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
pub(super) fn resolve(root: &Path, name: &str) -> PathBuf {
    let p = with_md_ext(name);
    if p.is_absolute() {
        p
    } else {
        root.join(p)
    }
}

pub(super) fn open_or_empty(path: &Path) -> Document {
    Document::open(path).unwrap_or_else(|e| {
        eprintln!("darknotes: could not open {}: {e}", path.display());
        Document::new("")
    })
}

/// The `{arg}`/`{argN}` slots in a `[notes.*]` `name`, in order, as
/// `(byte range of the token, N)` with `N >= 1`; `{arg}` is `{arg1}`. A slot
/// is exactly `{arg`, digits, `}` — anything else (`{argx}`, `{arg0}`) is
/// literal text. Both `note_arg_count` and the substitution in `create_note`
/// read this, so they can't disagree about what gets replaced.
fn note_arg_slots(name: &str) -> impl Iterator<Item = (std::ops::Range<usize>, usize)> {
    name.match_indices("{arg").filter_map(move |(start, _)| {
        let digits_at = start + "{arg".len();
        let end = digits_at + name[digits_at..].find('}')?;
        let digits = &name[digits_at..end];
        let n = if digits.is_empty() {
            1
        } else {
            digits.parse::<usize>().ok().filter(|&n| n >= 1)?
        };
        Some((start..end + 1, n))
    })
}

/// How many positional arguments a `[notes.*]` `name` needs: the highest slot
/// it references, 0 if none. `{arg}` and `{arg1}` are the same slot, so a
/// `name` mixing them isn't an error, just redundant; `{arg3}` alone still
/// needs three words, with the first two unused.
pub(super) fn note_arg_count(name: &str) -> usize {
    note_arg_slots(name).map(|(_, n)| n).max().unwrap_or(0)
}

/// Split `raw` into exactly `count` (>= 1) positional arguments on
/// whitespace, right-anchored: the trailing `count - 1` words each become
/// their own argument, and everything before them — however many words — is
/// joined into the first with single spaces. This lets a multi-word first
/// argument coexist with single-word ones after it, as long as only the first
/// argument ever needs to hold multiple words. Returns `None` if `raw`
/// doesn't have at least `count` words.
fn split_note_args(raw: &str, count: usize) -> Option<Vec<String>> {
    let words: Vec<&str> = raw.split_whitespace().collect();
    if words.len() < count {
        return None;
    }
    let split_at = words.len() - (count - 1);
    let mut args = vec![words[..split_at].join(" ")];
    args.extend(words[split_at..].iter().map(|w| w.to_string()));
    Some(args)
}

/// Replace characters that are unsafe or reserved in a file/folder name on
/// common filesystems (colon included) or that could add path structure the
/// caller didn't ask for (slash, backslash), plus control characters, with `_`.
fn sanitize_arg(arg: &str) -> String {
    arg.chars()
        .map(|c| if c.is_control() || "/\\:*?\"<>|".contains(c) { '_' } else { c })
        .collect()
}

/// The note at `dir/{name}.md` under `root`, created if missing and seeded from
/// the vault-relative `template` when that file exists. `name` is formatted
/// through strftime against the local clock, so `%Y-%m-%d` rolls over at the
/// user's midnight, not UTC's — done before any `{arg}` substitution, so an
/// argument containing `%` can't be misread as a strftime specifier. An
/// existing note is never touched — a `name` with no date specifiers (and no
/// unresolved `{arg}`) resolves to the same path every time, so repeat
/// invocations reopen it. `name` may contain `/`; every folder on the way,
/// including ones contributed by an argument, is created as needed.
///
/// `arg` is user-typed text riding straight into a file path: each argument
/// is sanitized (`sanitize_arg`) so it can't add path segments, and a
/// substituted `name` is rejected if any segment is `.`/`..` — the one escape
/// a slash-free argument has left. A placeholder-free `name` isn't checked:
/// `..` there is the vault owner's config, the same authority as `:e ../foo`.
/// Too few words for the slots `name` references is an error, never an
/// unfilled slot.
// ponytail: template is copied verbatim; substitution inside the body rides the
// general templates feature when it lands.
fn create_note(
    root: &Path,
    dir: &str,
    name: &str,
    template: &str,
    arg: Option<&str>,
) -> std::io::Result<PathBuf> {
    // Fallible format: `name` is hand-edited config, and jiff's `strftime`
    // Display impl panics on an unknown specifier.
    let name = jiff::fmt::strtime::format(name, &jiff::Zoned::now())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let needed = note_arg_count(&name);
    let name = if needed == 0 {
        name
    } else {
        let args = split_note_args(arg.unwrap_or(""), needed).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("needs {needed} argument(s): {name:?}"),
            )
        })?;
        // One pass over the original text, so a substituted argument that
        // happens to contain `{arg2}` is never itself substituted.
        let mut filled = String::with_capacity(name.len());
        let mut tail = 0;
        for (range, n) in note_arg_slots(&name) {
            filled.push_str(&name[tail..range.start]);
            filled.push_str(&sanitize_arg(&args[n - 1]));
            tail = range.end;
        }
        filled.push_str(&name[tail..]);
        // Only an argument-filled name is checked, and then all of it: a
        // config-authored `../{arg}` is refused along with an argument of `..`.
        if Path::new(&filled)
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("note name escapes its folder: {filled}"),
            ));
        }
        filled
    };
    let name = if name.ends_with(".md") { name } else { format!("{name}.md") };
    let path = root.join(dir).join(name);
    if !path.exists() {
        let seed = if template.is_empty() {
            String::new()
        } else {
            std::fs::read_to_string(root.join(template)).unwrap_or_default()
        };
        std::fs::create_dir_all(path.parent().unwrap_or(root))?;
        std::fs::write(&path, seed)?;
    }
    Ok(path)
}

/// Append `text` as its own line, creating the file if absent and supplying the
/// separator when the existing content doesn't end in a newline.
fn append_line(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let lead = match std::fs::read_to_string(path) {
        Ok(s) if !s.is_empty() && !s.ends_with('\n') => "\n",
        _ => "",
    };
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(f, "{lead}{text}")
}

/// Vault-relative path of `path`, forward-slashed — the switcher match key and
/// label. The implied `.md` is dropped (`projects/ideas`); any other extension
/// is kept (`sql/schema.sql`).
pub(super) fn rel_display(root: &Path, path: &Path) -> String {
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
pub(super) fn resolve_link(root: &Path, files: &[PathBuf], target: &str) -> Option<PathBuf> {
    files
        .iter()
        .find(|p| {
            rel_display(root, p).eq_ignore_ascii_case(target)
                || p.file_stem()
                    .is_some_and(|s| s.to_string_lossy().eq_ignore_ascii_case(target))
        })
        .cloned()
}

/// Whether a wikilink target may be resolved into a path. Note text is
/// untrusted — a synced or shared vault carries whatever its author wrote — and
/// a real target is a bare stem or a vault-relative path, so anything that
/// could leave the vault is refused. `..`, `.`, an absolute path, and a Windows
/// prefix are all non-`Normal` components.
///
/// A lexical check, not `canonicalize` + `starts_with`: the target usually does
/// not exist yet (that is how `[[New Note]]` then `:w` creates a note), which
/// makes `canonicalize` fail, and `starts_with` is itself lexical — for a root
/// of `/vault`, `/vault/../../etc/passwd.md` starts with `/vault`.
pub(super) fn vault_relative(target: &str) -> bool {
    let p = Path::new(target);
    p.components().all(|c| matches!(c, Component::Normal(_))) && p.components().next().is_some()
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

#[cfg(test)]
mod tests {
    use super::{
        append_line, create_note, match_buffer, note_arg_count, rel_display, resolve,
        resolve_link, unique_dest, unsaved_label, vault_relative,
    };
    use std::path::{Path, PathBuf};

    #[test]
    fn create_note_seeds_from_template_and_keeps_existing() {
        let root = std::env::temp_dir().join("darknotes_create_note_test");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        // No template: created empty, `daily/` made on the way.
        let p = create_note(&root, "daily", "2026-07-17", "templates/daily.md", None).unwrap();
        assert_eq!(p, root.join("daily").join("2026-07-17.md"));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "");

        // An existing note is never overwritten.
        std::fs::write(&p, "notes").unwrap();
        create_note(&root, "daily", "2026-07-17", "templates/daily.md", None).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "notes");

        // A template seeds new days verbatim.
        std::fs::create_dir_all(root.join("templates")).unwrap();
        std::fs::write(root.join("templates").join("daily.md"), "# Log\n").unwrap();
        let p2 = create_note(&root, "daily", "2026-07-18", "templates/daily.md", None).unwrap();
        assert_eq!(std::fs::read_to_string(&p2).unwrap(), "# Log\n");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn create_note_formats_strftime_name_and_rejects_bad_specifier() {
        let root = std::env::temp_dir().join("darknotes_create_note_strftime_test");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let p = create_note(&root, "", "%Y-%m-%d-standup", "", None).unwrap();
        let today = jiff::Zoned::now().date().to_string();
        assert_eq!(p, root.join(format!("{today}-standup.md")));

        // A bad specifier returns Err rather than panicking (jiff's strftime
        // Display impl panics on this; `strtime::format` doesn't).
        assert!(create_note(&root, "", "%K", "", None).is_err());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn create_note_substitutes_arg_into_subfolder() {
        let root = std::env::temp_dir().join("darknotes_create_note_arg_test");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        // `{arg}` builds a subfolder under `dir`, created on the way; `%`-free
        // arg text rides through untouched by the strftime pass.
        let p = create_note(&root, "bible-study", "{arg}/study", "", Some("genesis")).unwrap();
        assert_eq!(p, root.join("bible-study").join("genesis").join("study.md"));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "");

        // No `{arg}` in `name`: the argument is simply unused.
        let p2 = create_note(&root, "daily", "%Y-%m-%d", "", Some("ignored")).unwrap();
        let today = jiff::Zoned::now().date().to_string();
        assert_eq!(p2, root.join("daily").join(format!("{today}.md")));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn create_note_sanitizes_unsafe_characters_in_args() {
        let root = std::env::temp_dir().join("darknotes_create_note_sanitize_test");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        // A colon (invalid in file/folder names on several filesystems) is
        // swapped for an underscore before it ever reaches the path.
        let p = create_note(&root, "study", "{arg}", "", Some("12:12-19")).unwrap();
        assert_eq!(p, root.join("study").join("12_12-19.md"));

        // A slash in the argument can't inject a path segment the config
        // didn't ask for: it's sanitized before the path is built, so this
        // lands as one oddly named (but harmless, in-`dir`) file rather than
        // escaping anywhere.
        let p2 = create_note(&root, "study", "{arg}", "", Some("/etc/passwd")).unwrap();
        let expected = "/etc/passwd".replace('/', "_");
        assert_eq!(p2, root.join("study").join(format!("{expected}.md")));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn create_note_rejects_a_literal_parent_dir_arg() {
        let root = std::env::temp_dir().join("darknotes_create_note_escape_test");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        // Sanitizing `/` out of arguments closes off slash-based traversal;
        // a bare ".." (no slash to sanitize) is the one shape still capable
        // of pointing outside `dir`, so it's rejected outright.
        assert!(create_note(&root, "study", "{arg}", "", Some("..")).is_err());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn note_arg_count_reads_any_slot_number() {
        assert_eq!(note_arg_count("%Y-%m-%d"), 0);
        assert_eq!(note_arg_count("{arg}"), 1);
        assert_eq!(note_arg_count("{arg1}/{arg}"), 1);
        assert_eq!(note_arg_count("{arg1}/{arg2}"), 2);
        // The highest slot wins; lower ones needn't appear.
        assert_eq!(note_arg_count("{arg3}"), 3);
        // Any number of digits.
        assert_eq!(note_arg_count("{arg12}"), 12);
        // Not slots: literal text, no argument demanded.
        assert_eq!(note_arg_count("{argx}"), 0);
        assert_eq!(note_arg_count("{arg0}"), 0);
        assert_eq!(note_arg_count("{arg"), 0);
        assert_eq!(note_arg_count("{arg1{arg2}"), 2);
    }

    #[test]
    fn create_note_leaves_a_placeholder_free_name_unchecked() {
        let root = std::env::temp_dir().join("darknotes_create_note_unchecked_test");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        // `..` in a config-authored `name` is the vault owner's call (same
        // authority as `dir = "../x"`); only argument-filled names are guarded.
        let p = create_note(&root, "study", "../shared/note", "", None).unwrap();
        assert_eq!(p, root.join("study").join("../shared/note.md"));
        assert!(root.join("shared").join("note.md").exists());

        // With a placeholder in play the whole substituted name is checked,
        // config-authored `..` included.
        assert!(create_note(&root, "study", "../{arg}", "", Some("x")).is_err());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn create_note_splits_multiple_args_right_anchored() {
        let root = std::env::temp_dir().join("darknotes_create_note_multiarg_test");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        // Single-word book + reference.
        let p = create_note(&root, "bible-study", "{arg1}/{arg2}", "", Some("john 12:12-19"))
            .unwrap();
        assert_eq!(p, root.join("bible-study").join("john").join("12_12-19.md"));

        // A multi-word book name: everything but the last word (the
        // reference) is joined back into the first argument.
        let p2 = create_note(&root, "bible-study", "{arg1}/{arg2}", "", Some("1 John 3:16"))
            .unwrap();
        assert_eq!(p2, root.join("bible-study").join("1 John").join("3_16.md"));

        // Too few words for the placeholders the name references is an error.
        assert!(create_note(&root, "bible-study", "{arg1}/{arg2}", "", Some("genesis")).is_err());
        assert!(create_note(&root, "bible-study", "{arg1}/{arg2}", "", None).is_err());

        // An argument containing a slot token isn't re-substituted: the
        // replacement is one pass over the configured name.
        let p3 = create_note(&root, "bible-study", "{arg1}/{arg2}", "", Some("{arg2} x")).unwrap();
        assert_eq!(p3, root.join("bible-study").join("{arg2}").join("x.md"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn append_line_creates_and_separates() {
        let root = std::env::temp_dir().join("darknotes_append_line_test");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("inbox.md");

        // Absent file: created, no leading blank line.
        append_line(&path, "- [ ] one").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "- [ ] one\n");

        // Newline-terminated: appended straight on.
        append_line(&path, "- [ ] two").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "- [ ] one\n- [ ] two\n");

        // No trailing newline: one is supplied, so nothing joins the last line.
        std::fs::write(&path, "# Inbox").unwrap();
        append_line(&path, "- [ ] three").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "# Inbox\n- [ ] three\n");

        let _ = std::fs::remove_dir_all(&root);
    }

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
    fn resolve_roots_relative_names_and_defaults_md() {
        let root = Path::new("/vault");
        assert_eq!(resolve(root, "foo"), Path::new("/vault/foo.md"));
        assert_eq!(resolve(root, "sub/bar.md"), Path::new("/vault/sub/bar.md"));
        // Absolute paths are honored, not re-rooted under the vault.
        assert_eq!(resolve(root, "/elsewhere/baz"), Path::new("/elsewhere/baz.md"));
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

    #[test]
    fn unsaved_label_names_one_note_and_counts_the_rest() {
        // `None` is what lets the window close without asking.
        assert_eq!(unsaved_label(&[]), None);
        assert_eq!(
            unsaved_label(&["daily/2026-07-30".to_string()]).unwrap(),
            "1 unsaved note: daily/2026-07-30"
        );
        let three = ["a".to_string(), "b".to_string(), "c".to_string()];
        assert_eq!(unsaved_label(&three).unwrap(), "3 unsaved notes");
    }

    #[test]
    fn vault_relative_refuses_targets_that_can_leave_the_vault() {
        // What a real wikilink looks like.
        assert!(vault_relative("ideas"));
        assert!(vault_relative("projects/ideas"));
        assert!(vault_relative("New Note"));
        assert!(vault_relative("a.b/c.md"));
        // Traversal, absolute, and the `.`/empty degenerate cases. `resolve`
        // would root the first two under the vault and honor the third as
        // typed, so all of them have to be refused before they reach it.
        assert!(!vault_relative("../../.bashrc"));
        assert!(!vault_relative("notes/../../../.ssh/id_ed25519.pub"));
        assert!(!vault_relative("/home/you/.ssh/id_ed25519.pub"));
        assert!(!vault_relative("./x"));
        assert!(!vault_relative(""));
        // `starts_with` is lexical, so the check `vault_relative` replaces
        // would have passed this one.
        let escaped = resolve(Path::new("/vault"), "../../etc/passwd.md");
        assert!(escaped.starts_with("/vault"));
        assert!(!vault_relative("../../etc/passwd.md"));
    }
}
