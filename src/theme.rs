use gpui::{rgb, Global, Hsla};

/// The one semantic color palette. Every rendered color reads from here instead
/// of a hardcoded hex, so a theme swap is a single `cx.set_global(Theme)` and
/// (later) config can override it. Stored as a GPUI global: set once at startup,
/// read in `Editor::render` and `LineElement::paint`.
#[derive(Clone, Copy)]
pub struct Theme {
    pub background: Hsla,
    pub foreground: Hsla,
    /// Caret / cursor block. The block caret's glyph is repainted in `background`
    /// so the character reads as a cutout against the accent.
    pub accent: Hsla,
    pub selection: Hsla,
    pub sidebar_background: Hsla,
    /// Inactive sidebar row text.
    pub sidebar_foreground: Hsla,
    /// Open-file and cursor row text.
    pub sidebar_active_foreground: Hsla,
    /// Open-file row background.
    pub sidebar_current_background: Hsla,
    /// Sidebar-pane cursor (the row `j`/`k` move) background.
    pub sidebar_cursor_background: Hsla,
    pub status_background: Hsla,
    pub status_foreground: Hsla,
}

impl Global for Theme {}

impl Default for Theme {
    fn default() -> Self {
        Self {
            background: rgb(0x1a1a1a).into(),
            foreground: rgb(0xcccccc).into(),
            accent: rgb(0xffcc00).into(),
            selection: rgb(0x264f78).into(),
            sidebar_background: rgb(0x141414).into(),
            sidebar_foreground: rgb(0x9a9a9a).into(),
            sidebar_active_foreground: rgb(0xffffff).into(),
            sidebar_current_background: rgb(0x2a2a40).into(),
            sidebar_cursor_background: rgb(0x3a3a5a).into(),
            status_background: rgb(0x2a2a2a).into(),
            status_foreground: rgb(0x888888).into(),
        }
    }
}
