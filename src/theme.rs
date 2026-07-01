use gpui::{rgb, Global, Hsla};

/// A semantic color palette. Every rendered color reads from here instead of a
/// hardcoded hex, so a theme swap is a single `cx.set_global(Theme)`. Stored as
/// a GPUI global: set once at startup from the configured `theme` name, read in
/// `Editor::render` and `LineElement::paint`. New palette = a `by_name` arm.
#[derive(Clone, Copy)]
pub struct Theme {
    pub background: Hsla,
    pub foreground: Hsla,
    /// Caret / cursor block. The block caret's glyph is repainted in `background`
    /// so the character reads as a cutout against the accent.
    pub accent: Hsla,
    pub selection: Hsla,
    /// Background quad under search matches (hlsearch/incsearch); text color is
    /// untouched, so it must keep `foreground` readable.
    pub search_match: Hsla,
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
    // Markdown rendering (see `markdown::SpanKind`). `muted` styles syntactic
    // punctuation (`##`, `**`, bullets); `code`/`code_bg` style inline and
    // fenced code.
    pub heading: Hsla,
    pub code: Hsla,
    pub code_bg: Hsla,
    pub muted: Hsla,
}

impl Global for Theme {}

impl Theme {
    /// Resolve a config `theme = "<name>"` to a built-in palette. `None` for an
    /// unknown name, so the caller can warn and fall back to the default.
    pub fn by_name(name: &str) -> Option<Self> {
        match name {
            "dark" => Some(Self::dark()),
            "light" => Some(Self::light()),
            _ => None,
        }
    }

    fn dark() -> Self {
        Self {
            background: rgb(0x1a1a1a).into(),
            foreground: rgb(0xcccccc).into(),
            accent: rgb(0xffcc00).into(),
            selection: rgb(0x264f78).into(),
            // Muted olive — yellow family reads as "search" without colliding
            // with the brighter accent caret sitting on the current match.
            search_match: rgb(0x54491f).into(),
            sidebar_background: rgb(0x141414).into(),
            sidebar_foreground: rgb(0x9a9a9a).into(),
            sidebar_active_foreground: rgb(0xffffff).into(),
            sidebar_current_background: rgb(0x2a2a40).into(),
            sidebar_cursor_background: rgb(0x3a3a5a).into(),
            status_background: rgb(0x2a2a2a).into(),
            status_foreground: rgb(0x888888).into(),
            heading: rgb(0x87b3ff).into(),
            code: rgb(0xb5cea8).into(),
            code_bg: rgb(0x262626).into(),
            muted: rgb(0x707070).into(),
        }
    }

    fn light() -> Self {
        Self {
            background: rgb(0xfbfbfa).into(),
            foreground: rgb(0x2b2b2b).into(),
            // Dark amber so the block caret's `background`-painted glyph cutout
            // still reads against it on a light field.
            accent: rgb(0xc77800).into(),
            selection: rgb(0xb3d4fc).into(),
            search_match: rgb(0xffe28a).into(),
            sidebar_background: rgb(0xf0f0ee).into(),
            sidebar_foreground: rgb(0x6b6b6b).into(),
            sidebar_active_foreground: rgb(0x1a1a1a).into(),
            sidebar_current_background: rgb(0xdde7f5).into(),
            sidebar_cursor_background: rgb(0xc7d4ee).into(),
            status_background: rgb(0xe9e9e6).into(),
            status_foreground: rgb(0x555555).into(),
            heading: rgb(0x1d63d1).into(),
            code: rgb(0x9a3b2f).into(),
            code_bg: rgb(0xecebe7).into(),
            muted: rgb(0x8a8a8a).into(),
        }
    }
}

impl Default for Theme {
    /// The dark palette is the default — first-run config and the fallback for
    /// an unknown configured name.
    fn default() -> Self {
        Self::dark()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_names_resolve_unknown_is_none() {
        assert!(Theme::by_name("dark").is_some());
        assert!(Theme::by_name("light").is_some());
        assert!(Theme::by_name("nope").is_none());
    }
}
