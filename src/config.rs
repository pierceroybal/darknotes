//! Hand-edited user preferences, read once at startup from
//! `config.toml`. Durable prefs only — machine-owned restore state (open
//! buffers, cursor, scroll) belongs in a separate `session.json`, never here,
//! so churning state can't clobber a hand-edited file's comments.
//!
//! Extending the keymap: today only `insert_exit` is configurable (the one
//! binding asked for). A general `[keymap]` remap table — arbitrary
//! key-sequence → named command — waits on the command registry (see
//! `docs/foundation.md` Foundation 1). When that lands, add a `bindings` map
//! here and resolve names against the registry; `insert_exit` becomes one
//! entry rather than its own field.

use std::path::PathBuf;

use serde::Deserialize;

/// The starter config written to the user's config path on first run, so they
/// always have a commented file to hand-edit. Compiled in (not read from the
/// repo) so it ships inside the binary.
const DEFAULT_CONFIG: &str = include_str!("../config.default.toml");

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Vault opened when no path is given on the CLI. `~` expands to `$HOME`.
    pub vault: Option<String>,
    /// Built-in palette name (`"dark"`/`"light"`), resolved via `Theme::by_name`.
    /// An unknown name warns and falls back to the default at startup.
    pub theme: String,
    pub font_family: String,
    pub font_size: f32,
    /// Spaces inserted for a Tab (markdown has no literal tabs).
    pub tab_width: usize,
    pub keymap: Keymap,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Keymap {
    /// Insert-mode key sequence that acts as `<Esc>` (e.g. `"jk"`). Empty = off.
    pub insert_exit: String,
    /// Milliseconds to wait for the sequence to complete before its lead key is
    /// inserted as literal text (vim's `timeoutlen`).
    pub timeoutlen: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            vault: None,
            theme: "dark".into(),
            font_family: "DejaVu Sans Mono".into(),
            font_size: 15.0,
            tab_width: 2,
            keymap: Keymap::default(),
        }
    }
}

impl Default for Keymap {
    fn default() -> Self {
        Self { insert_exit: String::new(), timeoutlen: 1000 }
    }
}

impl Config {
    /// Load the config, seeding the file from the embedded default on first run
    /// so the user gets a commented file to edit (and a later onboarding flow
    /// can rewrite, e.g. the chosen vault). A parse error is reported to stderr
    /// and defaults are used, so a typo in the hand-edited file never bricks the
    /// editor.
    pub fn load() -> Self {
        let Some(path) = config_path() else {
            return parse_or_default(DEFAULT_CONFIG, "<embedded default>");
        };
        if !path.exists() {
            // Best-effort seed: a write failure (e.g. unwritable dir) just means
            // we run on the embedded default this session, surfaced via the read
            // fallback below.
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Err(e) = std::fs::write(&path, DEFAULT_CONFIG) {
                eprintln!("darknotes: could not create {}: {e}", path.display());
            }
        }
        match std::fs::read_to_string(&path) {
            Ok(text) => parse_or_default(&text, &path.display().to_string()),
            Err(_) => parse_or_default(DEFAULT_CONFIG, "<embedded default>"),
        }
    }

    /// Configured vault path, `~`-expanded; `None` if unset.
    pub fn vault_path(&self) -> Option<PathBuf> {
        self.vault.as_deref().map(expand_tilde)
    }
}

fn parse_or_default(text: &str, src: &str) -> Config {
    toml::from_str(text).unwrap_or_else(|e| {
        eprintln!("darknotes: {src}: {e}; using defaults");
        Config::default()
    })
}

// ponytail: XDG/HOME only — add `%APPDATA%` if/when Windows is targeted.
fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("darknotes").join("config.toml"))
}

fn expand_tilde(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        let c: Config = toml::from_str(
            r#"
            tab_width = 4
            [keymap]
            insert_exit = "jk"
        "#,
        )
        .unwrap();
        assert_eq!(c.tab_width, 4);
        assert_eq!(c.keymap.insert_exit, "jk");
        // Unspecified keys keep their defaults.
        assert_eq!(c.font_family, Config::default().font_family);
        assert_eq!(c.keymap.timeoutlen, 1000);
    }

    #[test]
    fn empty_config_is_all_defaults() {
        let c: Config = toml::from_str("").unwrap();
        assert_eq!(c.tab_width, 2);
        assert_eq!(c.theme, "dark");
        assert!(c.keymap.insert_exit.is_empty());
    }

    #[test]
    fn embedded_default_is_valid() {
        // The file seeded on first run must parse, or every fresh install warns
        // and silently runs on fallback defaults.
        let c: Config = toml::from_str(DEFAULT_CONFIG).expect("config.default.toml must parse");
        assert!(!c.font_family.is_empty());
    }
}
