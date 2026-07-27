//! Open buffers and the files behind them: the tab list, buffer switching
//! (`:b`/`:bn`/gt-style nav), `:e`/`:enew`/`:today`, closing, and the vault
//! path helpers (name → path resolution, wikilink targets, daily notes).
//!
//! A child module of `editor` so methods can touch private `Editor` state.

use std::path::{Path, PathBuf};

use gpui::{Context, Window};

use crate::document::Document;

use super::Editor;

/// One open buffer: a `Document` plus tab metadata.
pub(super) struct Buffer {
    pub(super) doc: Document,
    /// VS Code-style preview tab: the next file open replaces this buffer
    /// instead of adding a tab; the first edit commits it (clears the flag).
    /// At most one preview buffer exists at a time.
    pub(super) preview: bool,
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
    pub(super) fn open_path(&mut self, path: PathBuf, window: &mut Window) {
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
    pub(super) fn activate(&mut self, i: usize, window: &mut Window) {
        if i != self.active {
            self.alternate = Some(self.active);
        }
        self.switch_to(i, window);
    }

    /// Show `doc` in the preview slot: replace the existing preview buffer, or
    /// append a new preview tab.
    pub(super) fn show_preview(&mut self, doc: Document, window: &mut Window) {
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
    /// (seeded from `templates/daily.md` when present) on first use. Local
    /// date, so the note rolls over at the user's midnight, not UTC's.
    pub(super) fn today(&mut self, window: &mut Window) {
        let date = jiff::Zoned::now().date().to_string();
        match create_daily(&self.vault.root, &date) {
            Ok(path) => {
                self.rescan_vault(); // the sidebar shows a first-of-the-day note immediately
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

/// The daily note for `date` (`daily/{date}.md` under `root`), created if
/// missing — seeded from `templates/daily.md` when that file exists, empty
/// otherwise. An existing note is never touched.
// ponytail: template is copied verbatim; variable substitution rides the
// general templates feature when it lands.
fn create_daily(root: &Path, date: &str) -> std::io::Result<PathBuf> {
    let path = root.join("daily").join(format!("{date}.md"));
    if !path.exists() {
        let seed =
            std::fs::read_to_string(root.join("templates/daily.md")).unwrap_or_default();
        std::fs::create_dir_all(root.join("daily"))?;
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
        append_line, create_daily, match_buffer, rel_display, resolve, resolve_link, unique_dest,
    };
    use std::path::{Path, PathBuf};

    #[test]
    fn create_daily_seeds_from_template_and_keeps_existing() {
        let root = std::env::temp_dir().join("darknotes_create_daily_test");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        // No template: created empty, `daily/` made on the way.
        let p = create_daily(&root, "2026-07-17").unwrap();
        assert_eq!(p, root.join("daily").join("2026-07-17.md"));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "");

        // An existing note is never overwritten.
        std::fs::write(&p, "notes").unwrap();
        create_daily(&root, "2026-07-17").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "notes");

        // A template seeds new days verbatim.
        std::fs::create_dir_all(root.join("templates")).unwrap();
        std::fs::write(root.join("templates").join("daily.md"), "# Log\n").unwrap();
        let p2 = create_daily(&root, "2026-07-18").unwrap();
        assert_eq!(std::fs::read_to_string(&p2).unwrap(), "# Log\n");

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
}
