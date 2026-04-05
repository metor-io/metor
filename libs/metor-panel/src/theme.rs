use std::borrow::Cow;
use std::sync::Arc;

use gpui::{App, Global, Hsla};

/// Convert an RGB hex literal and alpha to [`Hsla`] at compile time.
const fn hex(rgb: u32, a: f32) -> Hsla {
    let r = ((rgb >> 16) & 0xFF) as f32 / 255.0;
    let g = ((rgb >> 8) & 0xFF) as f32 / 255.0;
    let b = (rgb & 0xFF) as f32 / 255.0;

    let max = if r > g {
        if r > b { r } else { b }
    } else if g > b {
        g
    } else {
        b
    };
    let min = if r < g {
        if r < b { r } else { b }
    } else if g < b {
        g
    } else {
        b
    };
    let delta = max - min;
    let l = (max + min) / 2.0;

    let s = if l == 0.0 || l == 1.0 {
        0.0
    } else if l < 0.5 {
        delta / (2.0 * l)
    } else {
        delta / (2.0 - 2.0 * l)
    };

    let h = if delta == 0.0 {
        0.0
    } else if max == r {
        let mut h = ((g - b) / delta) / 6.0;
        if h < 0.0 {
            h += 1.0;
        }
        h
    } else if max == g {
        ((b - r) / delta + 2.0) / 6.0
    } else {
        ((r - g) / delta + 4.0) / 6.0
    };

    Hsla { h, s, l, a }
}

/// Color palette and typography for the panel UI.
#[derive(Clone)]
pub struct Theme {
    pub name: &'static str,
    pub font_family: &'static str,
    pub bg_primary: Hsla,
    pub bg_secondary: Hsla,
    /// Elevated surface (e.g. palette, popover).
    pub bg_elevated: Hsla,

    pub text_primary: Hsla,
    pub text_secondary: Hsla,
    pub text_tertiary: Hsla,

    pub border_primary: Hsla,

    /// Background for selected/highlighted items.
    pub selection_bg: Hsla,
    /// Text selection highlight in text fields.
    pub text_selection: Hsla,
    /// Semi-transparent overlay for drop targets.
    pub drop_target: Hsla,

    /// Pill/badge background.
    pub pill_bg: Hsla,
    /// Pill/badge border.
    pub pill_border: Hsla,

    pub line_color: Hsla,
    pub line_colors: [Hsla; 8],
    pub grid_color: Hsla,
    pub axis_color: Hsla,
    pub zero_line_color: Hsla,
}

/// Wrapper for storing the active theme as a gpui Global.
pub struct ActiveTheme(pub Arc<Theme>);

impl Global for ActiveTheme {}

/// Read the active theme from the gpui global state.
pub fn theme(cx: &App) -> Arc<Theme> {
    cx.global::<ActiveTheme>().0.clone()
}

/// Set the active theme. Notifies all global observers so views repaint.
pub fn set_theme(cx: &mut App, theme: Arc<Theme>) {
    cx.set_global(ActiveTheme(theme));
}

/// All built-in themes.
pub fn all_themes() -> &'static [&'static Theme] {
    static THEMES: &[&Theme] = &[&DARK, &CATPPUCCIN_MOCHA, &CATPPUCCIN_MACCHIATO, &CATPPUCCIN_LATTE, &AYU_DARK];
    THEMES
}

// ── Default line colors (shared across themes that don't override) ──

const LINE_COLORS: [Hsla; 8] = [
    hex(0xff7c1f, 1.0), // orange
    hex(0x4aa0e0, 1.0), // blue
    hex(0x40b060, 1.0), // green
    hex(0xe04040, 1.0), // red
    hex(0xb070e0, 1.0), // purple
    hex(0x40c0b0, 1.0), // cyan
    hex(0xe0c030, 1.0), // yellow
    hex(0xe070a0, 1.0), // pink
];

// ── Built-in themes ─────────────────────────────────────────────────

pub static DARK: Theme = Theme {
    name: "Dark",
    font_family: "Berkeley Mono",

    bg_primary:   hex(0x1f1d1b, 1.0),
    bg_secondary: hex(0x151413, 1.0),
    bg_elevated:  hex(0x252321, 1.0),

    text_primary:   hex(0xf5efe6, 1.0),
    text_secondary: hex(0x857f79, 1.0),
    text_tertiary:  hex(0x6b6661, 1.0),

    border_primary: hex(0x36322f, 1.0),

    selection_bg:   hex(0x36322f, 1.0),
    text_selection: hex(0x2d4a6b, 0.6),
    drop_target:    hex(0xe08a30, 0.15),

    pill_bg:     hex(0x404040, 1.0),
    pill_border: hex(0x595959, 1.0),

    line_color:      hex(0xe08a30, 1.0),
    line_colors:     LINE_COLORS,
    grid_color:      hex(0x2e2b28, 1.0),
    axis_color:      hex(0x4d4843, 1.0),
    zero_line_color: hex(0x6b6560, 1.0),
};

pub static CATPPUCCIN_MOCHA: Theme = Theme {
    name: "Catppuccin Mocha",
    font_family: "Berkeley Mono",

    bg_primary:   hex(0x1e1e2e, 1.0),
    bg_secondary: hex(0x11111b, 1.0),
    bg_elevated:  hex(0x313244, 1.0),

    text_primary:   hex(0xcdd6f4, 1.0),
    text_secondary: hex(0xbac2de, 1.0),
    text_tertiary:  hex(0xa6adc8, 1.0),

    border_primary: hex(0x313244, 1.0),

    selection_bg:   hex(0x45475a, 1.0),
    text_selection: hex(0x585b70, 0.6),
    drop_target:    hex(0xf9e2af, 0.15),

    pill_bg:     hex(0x45475a, 1.0),
    pill_border: hex(0x585b70, 1.0),

    line_color:      hex(0xf9e2af, 1.0),
    line_colors: [
        hex(0xfab387, 1.0), // peach
        hex(0x89b4fa, 1.0), // blue
        hex(0xa6e3a1, 1.0), // green
        hex(0xf38ba8, 1.0), // red
        hex(0xcba6f7, 1.0), // mauve
        hex(0x94e2d5, 1.0), // teal
        hex(0xf9e2af, 1.0), // yellow
        hex(0xf5c2e7, 1.0), // pink
    ],
    grid_color:      hex(0x313244, 1.0),
    axis_color:      hex(0x45475a, 1.0),
    zero_line_color: hex(0x585b70, 1.0),
};

pub static CATPPUCCIN_MACCHIATO: Theme = Theme {
    name: "Catppuccin Macchiato",
    font_family: "Berkeley Mono",

    bg_primary:   hex(0x24273a, 1.0),
    bg_secondary: hex(0x1e2030, 1.0),
    bg_elevated:  hex(0x363a4f, 1.0),

    text_primary:   hex(0xcad3f5, 1.0),
    text_secondary: hex(0xb8c0e0, 1.0),
    text_tertiary:  hex(0xa5adcb, 1.0),

    border_primary: hex(0x363a4f, 1.0),

    selection_bg:   hex(0x494d64, 1.0),
    text_selection: hex(0x5b6078, 0.6),
    drop_target:    hex(0xeed49f, 0.15),

    pill_bg:     hex(0x494d64, 1.0),
    pill_border: hex(0x5b6078, 1.0),

    line_color:      hex(0xeed49f, 1.0),
    line_colors: [
        hex(0xf5a97f, 1.0), // peach
        hex(0x8aadf4, 1.0), // blue
        hex(0xa6da95, 1.0), // green
        hex(0xed8796, 1.0), // red
        hex(0xc6a0f6, 1.0), // mauve
        hex(0x8bd5ca, 1.0), // teal
        hex(0xeed49f, 1.0), // yellow
        hex(0xf5bde6, 1.0), // pink
    ],
    grid_color:      hex(0x363a4f, 1.0),
    axis_color:      hex(0x494d64, 1.0),
    zero_line_color: hex(0x5b6078, 1.0),
};

pub static CATPPUCCIN_LATTE: Theme = Theme {
    name: "Catppuccin Latte",
    font_family: "Berkeley Mono",

    bg_primary:   hex(0xeff1f5, 1.0),
    bg_secondary: hex(0xe6e9ef, 1.0),
    bg_elevated:  hex(0xdce0e8, 1.0),

    text_primary:   hex(0x4c4f69, 1.0),
    text_secondary: hex(0x5c5f77, 1.0),
    text_tertiary:  hex(0x6c6f85, 1.0),

    border_primary: hex(0xccd0da, 1.0),

    selection_bg:   hex(0xbcc0cc, 1.0),
    text_selection: hex(0x7287fd, 0.3),
    drop_target:    hex(0xdf8e1d, 0.15),

    pill_bg:     hex(0xccd0da, 1.0),
    pill_border: hex(0xbcc0cc, 1.0),

    line_color:      hex(0xdf8e1d, 1.0),
    line_colors: [
        hex(0xfe640b, 1.0), // peach
        hex(0x1e66f5, 1.0), // blue
        hex(0x40a02b, 1.0), // green
        hex(0xd20f39, 1.0), // red
        hex(0x8839ef, 1.0), // mauve
        hex(0x179299, 1.0), // teal
        hex(0xdf8e1d, 1.0), // yellow
        hex(0xea76cb, 1.0), // pink
    ],
    grid_color:      hex(0xdce0e8, 1.0),
    axis_color:      hex(0xccd0da, 1.0),
    zero_line_color: hex(0xbcc0cc, 1.0),
};

pub static AYU_DARK: Theme = Theme {
    name: "Ayu Dark",
    font_family: "Berkeley Mono",

    bg_primary:   hex(0x0b0e14, 1.0),
    bg_secondary: hex(0x0f131a, 1.0),
    bg_elevated:  hex(0x1a1e28, 1.0),

    text_primary:   hex(0xbfbdb6, 1.0),
    text_secondary: hex(0x565b66, 1.0),
    text_tertiary:  hex(0x6c7380, 1.0),

    border_primary: hex(0x11151c, 1.0),

    selection_bg:   hex(0x475266, 1.0),
    text_selection: hex(0x3d4455, 0.6),
    drop_target:    hex(0x59c2ff, 0.15),

    pill_bg:     hex(0x1a1e28, 1.0),
    pill_border: hex(0x2a2e38, 1.0),

    line_color:      hex(0xe6b450, 1.0),
    line_colors: [
        hex(0xe6b450, 1.0), // orange/yellow
        hex(0x59c2ff, 1.0), // blue
        hex(0x7fd962, 1.0), // green
        hex(0xd95757, 1.0), // red
        hex(0xd2a6ff, 1.0), // purple
        hex(0x95e6cb, 1.0), // teal
        hex(0xffb454, 1.0), // yellow
        hex(0xf07178, 1.0), // pink/red
    ],
    grid_color:      hex(0x11151c, 1.0),
    axis_color:      hex(0x1a1e28, 1.0),
    zero_line_color: hex(0x2a2e38, 1.0),
};

/// Register the embedded fonts with gpui's text system.
pub fn register_fonts(cx: &App) {
    cx.text_system()
        .add_fonts(vec![
            Cow::Borrowed(include_bytes!("../assets/fonts/IBMPlexMono-Regular.ttf")),
            Cow::Borrowed(include_bytes!("../assets/fonts/IBMPlexMono-Bold.ttf")),
            Cow::Borrowed(include_bytes!("../assets/fonts/IBMPlexMono-Italic.ttf")),
        ])
        .expect("failed to register embedded fonts");
}
