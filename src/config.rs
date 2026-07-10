//! Hand-edited user preferences, read once at startup from
//! `config.toml`. Durable prefs only — machine-owned restore state (open
//! buffers, cursor, scroll) belongs in a separate `session.toml`, never here,
//! so churning state can't clobber a hand-edited file's comments.

use std::collections::BTreeMap;
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
    /// Built-in palette name, resolved via `Theme::by_name`; bare `:theme`
    /// lists the options. An unknown name warns and falls back to the default
    /// at startup.
    pub theme: String,
    pub font_family: String,
    /// Font for chrome (sidebar, tabline, status bar, picker). Empty = inherit
    /// `font_family` for all-mono chrome.
    pub ui_font_family: String,
    pub font_size: f32,
    /// Spaces inserted for a Tab (markdown has no literal tabs).
    pub tab_width: usize,
    /// Line-number gutter: off, absolute, or relative-to-cursor.
    pub line_numbers: LineNumbers,
    /// Render markdown by hiding syntax markers on every line except the one the
    /// cursor is on, which shows full source.
    pub render_markdown: bool,
    /// Soft-wrap long lines at the pane edge (vim 'wrap'; `:set nowrap` to
    /// scroll horizontally instead).
    pub wrap: bool,
    /// Blink the caret while the editor pane is focused. An unfocused editor
    /// (sidebar has keys) shows a solid dim caret regardless.
    pub cursor_blink: bool,
    /// Length in milliseconds of each blink phase (visible / hidden).
    /// 0 also disables blinking.
    pub cursor_blink_interval: u64,
    pub keymap: Keymap,
    pub search: Search,
}

/// Line-number gutter mode. `relative` is hybrid: the cursor line shows its
/// absolute number, others show the distance to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LineNumbers {
    #[default]
    Off,
    Absolute,
    Relative,
}

/// User key bindings, resolved by `keymap::Resolver` before the built-in vim
/// grammar. Each table maps a key sequence — whitespace-separated gpui
/// keystrokes (`"ctrl-s"`, `"space f"`, `"j k"`) — to a registry command name.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Keymap {
    /// Milliseconds to wait for a partially-typed multi-key binding before its
    /// buffered keys are handled as ordinary input (vim's `timeoutlen`).
    pub timeoutlen: u64,
    /// Bindings live in every editor mode (the Ctrl-chord layer). A binding on
    /// the same keys as a built-in default (`ctrl-s`, `ctrl-p`, `ctrl-shift-p`,
    /// `ctrl-r`) shadows it.
    pub global: BTreeMap<String, String>,
    /// Bindings live only in vim normal mode (e.g. `"space f" = "open-file"`).
    pub normal: BTreeMap<String, String>,
    /// Bindings live only in insert mode (e.g. `"j k" = "normal-mode"`).
    pub insert: BTreeMap<String, String>,
}

/// `/`-search behavior (vim option names). Defaults are notes-friendly:
/// ignorecase/incsearch on (vim ships them off); hlsearch off like vim, so
/// matches light up only while the search prompt is open.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct Search {
    /// Case-insensitive matching (vim 'ignorecase').
    pub ignorecase: bool,
    /// With `ignorecase`: an uppercase char in the query makes that search
    /// case-sensitive (vim 'smartcase').
    pub smartcase: bool,
    /// Highlight every match of the last search; `:noh` clears until the next
    /// search (vim 'hlsearch').
    pub hlsearch: bool,
    /// Jump to the nearest match live while the query is being typed
    /// (vim 'incsearch').
    pub incsearch: bool,
    /// Searches wrap around the ends of the file (vim 'wrapscan').
    pub wrapscan: bool,
}

impl Default for Search {
    fn default() -> Self {
        Self {
            ignorecase: true,
            smartcase: true,
            hlsearch: false,
            incsearch: true,
            wrapscan: true,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            vault: None,
            theme: "dark".into(),
            // Embedded at startup in main.rs; always available regardless of OS.
            font_family: "Courier Prime".into(),
            // Embedded at startup in main.rs, like the editor font.
            ui_font_family: "Inter".into(),
            font_size: 15.0,
            tab_width: 2,
            line_numbers: LineNumbers::Off,
            render_markdown: true,
            wrap: true,
            cursor_blink: true,
            cursor_blink_interval: 500,
            keymap: Keymap::default(),
            search: Search::default(),
        }
    }
}

impl Default for Keymap {
    fn default() -> Self {
        Self {
            timeoutlen: 1000,
            global: BTreeMap::new(),
            normal: BTreeMap::new(),
            insert: BTreeMap::new(),
        }
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

/// Shared config-dir resolution (`config.toml`, `session.toml`).
// ponytail: XDG/HOME only — add `%APPDATA%` if/when Windows is targeted.
pub fn config_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("darknotes"))
}

fn config_path() -> Option<PathBuf> {
    Some(config_dir()?.join("config.toml"))
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
            [keymap.insert]
            "j k" = "normal-mode"
        "#,
        )
        .unwrap();
        assert_eq!(c.tab_width, 4);
        assert_eq!(c.keymap.insert.get("j k").map(String::as_str), Some("normal-mode"));
        // Unspecified keys keep their defaults.
        assert_eq!(c.font_family, Config::default().font_family);
        assert_eq!(c.keymap.timeoutlen, 1000);
    }

    #[test]
    fn search_section_overrides_and_defaults() {
        let c: Config = toml::from_str(
            r#"
            [search]
            ignorecase = false
        "#,
        )
        .unwrap();
        assert!(!c.search.ignorecase);
        // Unspecified search keys keep their defaults.
        assert!(c.search.smartcase);
        assert!(!c.search.hlsearch); // off: highlight only while the prompt is open
        assert!(c.search.incsearch);
        assert!(c.search.wrapscan);
    }

    #[test]
    fn empty_config_is_all_defaults() {
        let c: Config = toml::from_str("").unwrap();
        assert_eq!(c.tab_width, 2);
        assert_eq!(c.theme, "dark");
        assert!(c.keymap.insert.is_empty());
        assert!(c.cursor_blink);
        assert_eq!(c.cursor_blink_interval, 500);
    }

    #[test]
    fn embedded_default_is_valid() {
        // The file seeded on first run must parse, or every fresh install warns
        // and silently runs on fallback defaults.
        let c: Config = toml::from_str(DEFAULT_CONFIG).expect("config.default.toml must parse");
        assert!(!c.font_family.is_empty());
    }
}
