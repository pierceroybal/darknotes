//! The sidebar file tree: cursor navigation, folder expand/collapse, and
//! file operations (create, rename, trash, yank/cut/paste, delete-forever)
//! with their inline prompts.
//!
//! A child module of `editor` so methods can touch private `Editor` state.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use gpui::{KeyDownEvent, ScrollStrategy, Window};

use crate::vault::Row;

use super::{unique_dest, with_md_ext, Editor, Pane};

/// A sidebar file-op prompt — modal like the picker. Create and rename edit
/// inline in the tree: the input renders as the row being created/renamed
/// (vim's open-line, for files) with `label` as a status-line hint. The
/// delete confirmation lives in the status line alone. Printable keys extend
/// `input`, Backspace trims, Enter applies via `action`, Esc — or clicking
/// another row — cancels.
pub(super) struct FilePrompt {
    /// Status-line text: a hint (`"New in notes/"`) while editing inline,
    /// the whole question for the delete confirmation.
    pub(super) label: String,
    pub(super) input: String,
    pub(super) action: PromptAction,
}

/// What Enter does with a finished file prompt.
pub(super) enum PromptAction {
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

impl Editor {
    /// Expand or collapse a folder, then clamp the cursor — collapsing drops the
    /// rows beneath it, so `selected` can land past the end.
    pub(super) fn toggle(&mut self, dir: PathBuf) {
        if !self.expanded.remove(&dir) {
            self.expanded.insert(dir);
        }
        let n = self.vault.visible_rows(&self.expanded).len();
        self.selected = self.selected.min(n.saturating_sub(1));
    }

    /// Reveal the open buffer's file in the sidebar: expand collapsed ancestors,
    /// park the sidebar cursor on its row, and scroll it into view. A pathless
    /// buffer (or one outside the vault) parks the cursor at the top.
    pub(super) fn reveal_current(&mut self) {
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

    /// Navigate the sidebar tree (`Pane::Sidebar`): `j`/`k` move the cursor over
    /// visible rows, `l`/Enter toggles a folder or opens a file, `h` collapses an
    /// open folder or jumps to the parent folder, Escape returns.
    ///
    /// File operations (sidebar-local, independent of editor vim state):
    /// `o`/`O` create in the cursor's / the parent folder, `dd` moves to the
    /// vault's `.trash` (inside Trash it deletes forever, confirmed), `cc`
    /// renames, `yy`/`x` load the file register, `p` pastes it into the
    /// cursor's folder (yank copies, cut moves).
    pub(super) fn sidebar_key(&mut self, key: &str, shift: bool, window: &mut Window) {
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
    pub(super) fn select_path(&mut self, path: &Path) {
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
    pub(super) fn prompt_key(&mut self, ev: &KeyDownEvent, window: &mut Window) {
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
}

/// Mark every folder between the vault `root` and `path` as expanded, so a
/// nested file's row is visible. `root` itself isn't a row, so it's excluded.
pub(super) fn expand_ancestors(root: &Path, path: &Path, set: &mut HashSet<PathBuf>) {
    let mut cur = path.parent();
    while let Some(dir) = cur {
        if dir == root || !dir.starts_with(root) {
            break;
        }
        set.insert(dir.to_path_buf());
        cur = dir.parent();
    }
}
