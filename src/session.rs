//! Machine-owned restore state (open buffers, active tab, carets):
//! `session.toml` in the config dir, keyed by canonicalized vault root.
//! Best-effort on every IO path — a missing or corrupt file behaves like no
//! session; a failed write keeps the previous one and warns on stderr.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize)]
struct Session {
    vaults: BTreeMap<String, VaultSession>,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct VaultSession {
    /// Index into `files` (not into `Editor.buffers` — pathless buffers are
    /// dropped at snapshot time and the index re-pointed).
    pub active: usize,
    pub files: Vec<FileEntry>,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct FileEntry {
    /// Absolute (canonicalized at snapshot time — the next launch's cwd can
    /// differ, so vault-relative or as-opened paths would dangle).
    pub path: PathBuf,
    pub preview: bool,
    /// Caret as absolute char offset; `Document::jump_to` clamps it, so a
    /// file that shrank since last session is safe.
    pub caret: usize,
}

/// Stable per-vault key: canonicalized so `darknotes .` and an absolute-path
/// launch of the same vault share one entry.
fn key(vault: &Path) -> String {
    std::fs::canonicalize(vault)
        .unwrap_or_else(|_| vault.to_path_buf())
        .display()
        .to_string()
}

fn session_path() -> Option<PathBuf> {
    Some(crate::config::config_dir()?.join("session.toml"))
}

fn load() -> Session {
    let Some(p) = session_path() else { return Session::default() };
    let Ok(text) = std::fs::read_to_string(&p) else { return Session::default() };
    match toml::from_str(&text) {
        Ok(s) => s,
        Err(e) => {
            // Starting fresh is the documented behavior; say so, because the
            // alternative reading of every tab vanishing is a lost vault.
            eprintln!("darknotes: ignoring unreadable {}: {e}", p.display());
            Session::default()
        }
    }
}

pub fn restore(vault: &Path) -> Option<VaultSession> {
    load().vaults.remove(&key(vault))
}

/// Load-modify-write so other vaults' entries survive.
pub fn record(vault: &Path, entry: VaultSession) {
    let mut s = load();
    s.vaults.insert(key(vault), entry);
    let (Some(p), Ok(text)) = (session_path(), toml::to_string(&s)) else { return };
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // Atomic: this one file holds every vault's restore state, and a write
    // interrupted partway through would strand all of them, not just this one.
    if let Err(e) = crate::document::atomic_write(&p, &text) {
        eprintln!("darknotes: could not write {}: {e}", p.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_toml() {
        let mut s = Session::default();
        s.vaults.insert(
            "/home/x/notes".into(),
            VaultSession {
                active: 1,
                files: vec![
                    FileEntry { path: "/home/x/notes/a.md".into(), preview: false, caret: 42 },
                    FileEntry { path: "/home/x/notes/b.md".into(), preview: true, caret: 0 },
                ],
            },
        );
        let text = toml::to_string(&s).unwrap();
        let back: Session = toml::from_str(&text).unwrap();
        assert_eq!(back.vaults, s.vaults);
    }
}
