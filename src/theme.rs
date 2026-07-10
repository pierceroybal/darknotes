use gpui::{hsla, rgb, Global, Hsla};

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
    /// Wikilinks, markdown links, and bare URLs — color only, body weight.
    pub link: Hsla,
    /// 1px pane separators: sidebar/editor edge, below the tabline, above the
    /// status bar.
    pub border: Hsla,
    /// Hover wash on sidebar rows and inactive tabs. Carries alpha so the same
    /// wash reads correctly over both the sidebar and tabline backgrounds.
    pub hover: Hsla,
    /// Status-bar mode pill backgrounds; NORMAL uses `accent`. Pill text is
    /// painted in `background`, so these must keep it readable.
    pub mode_insert: Hsla,
    pub mode_visual: Hsla,
}

impl Global for Theme {}

/// Built-in palettes, name → constructor. `by_name` and `names` both read
/// this table, so the `:theme` listing can never drift from what resolves.
const BUILTINS: &[(&str, fn() -> Theme)] = &[
    ("dark", Theme::dark),
    ("light", Theme::light),
    ("ayu-mirage", Theme::ayu_mirage),
    ("kanagawa", Theme::kanagawa),
    ("everforest", Theme::everforest),
    ("gruvbox-material", Theme::gruvbox_material),
    ("catppuccin-mocha", Theme::catppuccin_mocha),
];

impl Theme {
    /// Resolve a config `theme = "<name>"` to a built-in palette. `None` for an
    /// unknown name, so the caller can warn and fall back to the default.
    pub fn by_name(name: &str) -> Option<Self> {
        BUILTINS.iter().find(|(n, _)| *n == name).map(|(_, f)| f())
    }

    /// All built-in palette names, for the bare `:theme` listing.
    pub fn names() -> impl Iterator<Item = &'static str> {
        BUILTINS.iter().map(|(n, _)| *n)
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
            link: rgb(0x6cabdd).into(),
            border: rgb(0x2c2c2c).into(),
            hover: hsla(0., 0., 1., 0.06),
            mode_insert: rgb(0x98c379).into(),
            mode_visual: rgb(0xc678dd).into(),
        }
    }

    /// Ayu Mirage — deep blue-slate ground with warm amber accent; cool cyans
    /// and blues against warm strings/yellows keep hues well separated.
    /// Values approximate the canonical ayu palette.
    fn ayu_mirage() -> Self {
        Self {
            background: rgb(0x1f2430).into(),
            foreground: rgb(0xcccac2).into(),
            accent: rgb(0xffcc66).into(),
            selection: rgb(0x2c3f5e).into(),
            search_match: rgb(0x4d4226).into(),
            sidebar_background: rgb(0x1c212b).into(),
            sidebar_foreground: rgb(0x707a8c).into(),
            sidebar_active_foreground: rgb(0xd9d7ce).into(),
            sidebar_current_background: rgb(0x2a3346).into(),
            sidebar_cursor_background: rgb(0x374362).into(),
            status_background: rgb(0x242936).into(),
            status_foreground: rgb(0x707a8c).into(),
            heading: rgb(0x73d0ff).into(),
            code: rgb(0xd5ff80).into(),
            code_bg: rgb(0x242936).into(),
            muted: rgb(0x6c7986).into(),
            link: rgb(0x5ccfe6).into(),
            border: rgb(0x323a4d).into(),
            hover: hsla(0., 0., 1., 0.06),
            mode_insert: rgb(0x87d96c).into(),
            mode_visual: rgb(0xdfbfff).into(),
        }
    }

    /// Kanagawa Wave — ink-dark ground, parchment foreground, muted wave blues
    /// and violets. Values approximate the canonical kanagawa palette.
    fn kanagawa() -> Self {
        Self {
            background: rgb(0x1f1f28).into(),
            foreground: rgb(0xdcd7ba).into(),
            accent: rgb(0xe6c384).into(),
            selection: rgb(0x2d4f67).into(),
            search_match: rgb(0x4e4531).into(),
            sidebar_background: rgb(0x16161d).into(),
            sidebar_foreground: rgb(0x9a9484).into(),
            sidebar_active_foreground: rgb(0xf0ead8).into(),
            sidebar_current_background: rgb(0x252535).into(),
            sidebar_cursor_background: rgb(0x363654).into(),
            status_background: rgb(0x2a2a37).into(),
            status_foreground: rgb(0x727169).into(),
            heading: rgb(0x7e9cd8).into(),
            code: rgb(0x98bb6c).into(),
            code_bg: rgb(0x2a2a37).into(),
            muted: rgb(0x727169).into(),
            link: rgb(0x7fb4ca).into(),
            border: rgb(0x363646).into(),
            hover: hsla(0., 0., 1., 0.06),
            mode_insert: rgb(0x98bb6c).into(),
            mode_visual: rgb(0x957fb8).into(),
        }
    }

    /// Everforest (dark medium) — warm sage-on-forest greens with soft red/
    /// yellow/aqua accents. Values approximate the canonical everforest palette.
    fn everforest() -> Self {
        Self {
            background: rgb(0x2d353b).into(),
            foreground: rgb(0xd3c6aa).into(),
            accent: rgb(0xdbbc7f).into(),
            // Everforest's signature muted-plum visual highlight.
            selection: rgb(0x543a48).into(),
            search_match: rgb(0x4a4839).into(),
            sidebar_background: rgb(0x272e33).into(),
            sidebar_foreground: rgb(0x859289).into(),
            sidebar_active_foreground: rgb(0xd3c6aa).into(),
            sidebar_current_background: rgb(0x3a454a).into(),
            sidebar_cursor_background: rgb(0x475258).into(),
            status_background: rgb(0x343f44).into(),
            status_foreground: rgb(0x859289).into(),
            heading: rgb(0xa7c080).into(),
            code: rgb(0x83c092).into(),
            code_bg: rgb(0x343f44).into(),
            muted: rgb(0x7a8478).into(),
            link: rgb(0x7fbbb3).into(),
            border: rgb(0x404a50).into(),
            hover: hsla(0., 0., 1., 0.06),
            mode_insert: rgb(0xa7c080).into(),
            mode_visual: rgb(0xd699b6).into(),
        }
    }

    /// Gruvbox Material (dark medium) — the classic warm retro palette with
    /// its saturation dialed down. Values approximate the canonical
    /// gruvbox-material palette.
    fn gruvbox_material() -> Self {
        Self {
            background: rgb(0x282828).into(),
            foreground: rgb(0xd4be98).into(),
            accent: rgb(0xd8a657).into(),
            selection: rgb(0x45403d).into(),
            search_match: rgb(0x4e4228).into(),
            sidebar_background: rgb(0x242221).into(),
            sidebar_foreground: rgb(0x928374).into(),
            sidebar_active_foreground: rgb(0xddc7a1).into(),
            sidebar_current_background: rgb(0x3c3836).into(),
            sidebar_cursor_background: rgb(0x504945).into(),
            status_background: rgb(0x32302f).into(),
            status_foreground: rgb(0x928374).into(),
            heading: rgb(0xe78a4e).into(),
            code: rgb(0xa9b665).into(),
            code_bg: rgb(0x32302f).into(),
            muted: rgb(0x7c6f64).into(),
            link: rgb(0x7daea3).into(),
            border: rgb(0x373432).into(),
            hover: hsla(0., 0., 1., 0.06),
            mode_insert: rgb(0xa9b665).into(),
            mode_visual: rgb(0xd3869b).into(),
        }
    }

    /// Catppuccin Mocha — soft pastels on a blue-tinted charcoal base; wide
    /// hue spread at low saturation. Values approximate the canonical
    /// catppuccin palette.
    fn catppuccin_mocha() -> Self {
        Self {
            background: rgb(0x1e1e2e).into(),
            foreground: rgb(0xcdd6f4).into(),
            // Rosewater, catppuccin's canonical cursor color — pale, so the
            // background-painted glyph cutout stays readable.
            accent: rgb(0xf5e0dc).into(),
            selection: rgb(0x45475a).into(),
            search_match: rgb(0x504a38).into(),
            sidebar_background: rgb(0x181825).into(),
            sidebar_foreground: rgb(0x7f849c).into(),
            sidebar_active_foreground: rgb(0xcdd6f4).into(),
            sidebar_current_background: rgb(0x313244).into(),
            sidebar_cursor_background: rgb(0x3d3e54).into(),
            status_background: rgb(0x313244).into(),
            status_foreground: rgb(0x7f849c).into(),
            heading: rgb(0x89b4fa).into(),
            code: rgb(0xa6e3a1).into(),
            code_bg: rgb(0x2a2a3e).into(),
            muted: rgb(0x6c7086).into(),
            link: rgb(0x74c7ec).into(),
            border: rgb(0x313244).into(),
            hover: hsla(0., 0., 1., 0.06),
            mode_insert: rgb(0xa6e3a1).into(),
            mode_visual: rgb(0xcba6f7).into(),
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
            link: rgb(0x2a6fb0).into(),
            border: rgb(0xd9d9d6).into(),
            hover: hsla(0., 0., 0., 0.05),
            mode_insert: rgb(0x3d8a3d).into(),
            mode_visual: rgb(0x8f4bab).into(),
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
        for name in Theme::names() {
            assert!(Theme::by_name(name).is_some(), "{name} in table but unresolvable");
        }
        assert!(Theme::by_name("nope").is_none());
    }
}
