//! Filesystem watch on the vault root, for external edits (an agent, a
//! script, `git checkout`) that darknotes didn't make itself.

use std::path::Path;
use std::sync::mpsc;

use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};

/// Owns a recursive watch on the vault root. The `notify` backend delivers
/// events from its own thread into `rx`; `Editor` polls `drain` on a timer.
pub struct VaultWatcher {
    _watcher: RecommendedWatcher,
    rx: mpsc::Receiver<notify::Result<Event>>,
}

impl VaultWatcher {
    pub fn new(root: &Path) -> notify::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let mut watcher = notify::recommended_watcher(tx)?;
        watcher.watch(root, RecursiveMode::Recursive)?;
        Ok(Self { _watcher: watcher, rx })
    }

    /// Every event queued since the last call, dropping backend errors.
    pub fn drain(&self) -> Vec<Event> {
        self.rx.try_iter().filter_map(Result::ok).collect()
    }
}
