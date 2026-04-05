use std::borrow::Cow;

use gpui::Hsla;

/// Convert an RGB hex literal and alpha to [`Hsla`] at compile time.
/// Usage: `hex(0x1f1d1b, 1.0)` for an opaque dark brown.
const fn hex(rgb: u32, a: f32) -> Hsla {
    let r = ((rgb >> 16) & 0xFF) as f32 / 255.0;
    let g = ((rgb >> 8) & 0xFF) as f32 / 255.0;
    let b = (rgb & 0xFF) as f32 / 255.0;

    // f32::max/min aren't const, so do it manually
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
pub struct Theme {
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

pub static DARK: Theme = Theme {
    font_family: "Berkeley Mono",

    bg_primary: hex(0x1f1d1b, 1.0),
    bg_secondary: hex(0x151413, 1.0),
    bg_elevated: hex(0x252321, 1.0),

    text_primary: hex(0xf5efe6, 1.0),
    text_secondary: hex(0x857f79, 1.0),
    text_tertiary: hex(0x6b6661, 1.0),

    border_primary: hex(0x36322f, 1.0),

    selection_bg: hex(0x36322f, 1.0),
    text_selection: hex(0x2d4a6b, 0.6),
    drop_target: hex(0xe08a30, 0.15),

    pill_bg: hex(0x404040, 1.0),
    pill_border: hex(0x595959, 1.0),

    line_color: hex(0xe08a30, 1.0),
    line_colors: [
        hex(0xff7c1f, 1.0), // orange
        hex(0x4aa0e0, 1.0), // blue
        hex(0x40b060, 1.0), // green
        hex(0xe04040, 1.0), // red
        hex(0xb070e0, 1.0), // purple
        hex(0x40c0b0, 1.0), // cyan
        hex(0xe0c030, 1.0), // yellow
        hex(0xe070a0, 1.0), // pink
    ],
    grid_color: hex(0x2e2b28, 1.0),
    axis_color: hex(0x4d4843, 1.0),
    zero_line_color: hex(0x6b6560, 1.0),
};

/// Register the embedded IBM Plex Mono font with gpui's text system.
pub fn register_fonts(cx: &gpui::App) {
    cx.text_system()
        .add_fonts(vec![
            Cow::Borrowed(include_bytes!("../assets/fonts/IBMPlexMono-Regular.ttf")),
            Cow::Borrowed(include_bytes!("../assets/fonts/IBMPlexMono-Bold.ttf")),
            Cow::Borrowed(include_bytes!("../assets/fonts/IBMPlexMono-Italic.ttf")),
        ])
        .expect("failed to register embedded fonts");
}
