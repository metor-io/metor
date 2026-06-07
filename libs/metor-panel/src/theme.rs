use std::borrow::Cow;
use std::sync::Arc;

use gpui::{App, Global, Hsla};

/// Build an [`Hsla`] from a packed `0xRRGGBB` literal in a const context.
///
/// Theme tables are `static`, so the conversion has to happen without
/// floating-point intrinsics or runtime allocation.
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

/// Complete color and typography palette for the panel UI.
///
/// All colors live here. Widgets must never hardcode an [`Hsla`] literal;
/// reference a theme field instead so palettes stay swappable.
#[derive(Clone)]
pub struct Theme {
    pub name: &'static str,
    pub font_family: &'static str,
    pub bg_primary: Hsla,
    pub bg_secondary: Hsla,
    /// Surface lifted above the page, used by palettes and popovers.
    pub bg_elevated: Hsla,

    pub text_primary: Hsla,
    pub text_secondary: Hsla,
    pub text_tertiary: Hsla,

    pub border_primary: Hsla,

    /// Highlight for selected rows and active tabs.
    pub selection_bg: Hsla,
    /// Inline text-field selection range.
    pub text_selection: Hsla,
    /// Translucent overlay drawn over a drop target during drag.
    pub drop_target: Hsla,

    pub pill_bg: Hsla,
    pub pill_border: Hsla,

    /// Default color for a plot line when no per-series color is set.
    pub line_color: Hsla,
    /// Categorical palette cycled through when a plot has several series.
    pub line_colors: [Hsla; 8],
    pub grid_color: Hsla,
    pub axis_color: Hsla,
    pub zero_line_color: Hsla,

    /// "On" color for checkboxes, switches, and traffic-light tiles. Replaces
    /// per-call-site indexing into `line_colors`.
    pub control_active: Hsla,
    /// Dimmed variant of `control_active` used as the track behind a checked
    /// switch knob. Same hue, lower presence.
    pub control_active_track: Hsla,
    /// Error accent (build failures, invalid arg pills). Used wherever the
    /// UI needs a "this is wrong" tint.
    pub error_accent: Hsla,
}

impl Theme {
    /// Semi-transparent `bg_secondary` for plot chrome strips — the legend
    /// background and the axis fills that mask GPU-frame edges straying into
    /// the chrome. Derived so it tracks whatever `bg_secondary` a theme picks.
    pub fn plot_chrome_bg(&self) -> Hsla {
        Hsla {
            a: 0.5,
            ..self.bg_secondary
        }
    }

    /// Edge color for a connection awaiting validation in the node editor:
    /// a faded `text_tertiary`.
    pub fn edge_pending(&self) -> Hsla {
        Hsla {
            a: 0.4,
            ..self.text_tertiary
        }
    }

    /// Solid color for an alarm severity (`severity_index`: 0 = info, 1 = warning,
    /// 2 = critical), used for limit lines, severity chips, and the status bar.
    /// Critical tracks the theme's `error_accent`; warning/info are fixed amber/blue
    /// that read on both light and dark themes.
    pub fn alarm_color(&self, severity_index: usize) -> Hsla {
        match severity_index {
            2 => self.error_accent,
            1 => hex(0xe0a030, 1.0),
            _ => hex(0x4090e0, 1.0),
        }
    }

    /// Low-alpha [`alarm_color`](Self::alarm_color) for out-of-bounds plot tinting.
    pub fn alarm_tint(&self, severity_index: usize) -> Hsla {
        Hsla {
            a: 0.10,
            ..self.alarm_color(severity_index)
        }
    }
}

/// Global wrapper that makes the active [`Theme`] addressable from any view.
pub struct ActiveTheme(pub Arc<Theme>);

impl Global for ActiveTheme {}

/// Read the currently active theme. Cheap: clones an `Arc`.
pub fn theme(cx: &App) -> Arc<Theme> {
    cx.global::<ActiveTheme>().0.clone()
}

/// Swap the active theme. Triggers global observers so every view repaints.
pub fn set_theme(cx: &mut App, theme: Arc<Theme>) {
    cx.set_global(ActiveTheme(theme));
}

/// Every theme compiled into the binary, in the order the settings UI displays them.
pub fn all_themes() -> &'static [&'static Theme] {
    static THEMES: &[&Theme] = &[
        &DARK,
        &CATPPUCCIN_MOCHA,
        &CATPPUCCIN_MACCHIATO,
        &CATPPUCCIN_LATTE,
        &AYU_DARK,
        &EVERFOREST_DARK,
        &EVERFOREST_LIGHT,
        &ROSE_PINE,
        &ROSE_PINE_MOON,
        &ROSE_PINE_DAWN,
        &MAKING_SOFTWARE,
        &DEPARTURE,
    ];
    THEMES
}

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

pub static DARK: Theme = Theme {
    name: "Dark",
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
    line_colors: LINE_COLORS,
    grid_color: hex(0x2e2b28, 1.0),
    axis_color: hex(0x4d4843, 1.0),
    zero_line_color: hex(0x6b6560, 1.0),

    control_active: hex(0x40b060, 1.0),
    control_active_track: hex(0x40b060, 0.7),
    error_accent: hex(0xe04040, 1.0),
};

pub static CATPPUCCIN_MOCHA: Theme = Theme {
    name: "Catppuccin Mocha",
    font_family: "Berkeley Mono",

    bg_primary: hex(0x1e1e2e, 1.0),
    bg_secondary: hex(0x11111b, 1.0),
    bg_elevated: hex(0x313244, 1.0),

    text_primary: hex(0xcdd6f4, 1.0),
    text_secondary: hex(0xbac2de, 1.0),
    text_tertiary: hex(0xa6adc8, 1.0),

    border_primary: hex(0x313244, 1.0),

    selection_bg: hex(0x45475a, 1.0),
    text_selection: hex(0x585b70, 0.6),
    drop_target: hex(0xf9e2af, 0.15),

    pill_bg: hex(0x45475a, 1.0),
    pill_border: hex(0x585b70, 1.0),

    line_color: hex(0xf9e2af, 1.0),
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
    grid_color: hex(0x313244, 1.0),
    axis_color: hex(0x45475a, 1.0),
    zero_line_color: hex(0x585b70, 1.0),

    control_active: hex(0xa6e3a1, 1.0),
    control_active_track: hex(0xa6e3a1, 0.7),
    error_accent: hex(0xf38ba8, 1.0),
};

pub static CATPPUCCIN_MACCHIATO: Theme = Theme {
    name: "Catppuccin Macchiato",
    font_family: "Berkeley Mono",

    bg_primary: hex(0x24273a, 1.0),
    bg_secondary: hex(0x1e2030, 1.0),
    bg_elevated: hex(0x363a4f, 1.0),

    text_primary: hex(0xcad3f5, 1.0),
    text_secondary: hex(0xb8c0e0, 1.0),
    text_tertiary: hex(0xa5adcb, 1.0),

    border_primary: hex(0x363a4f, 1.0),

    selection_bg: hex(0x494d64, 1.0),
    text_selection: hex(0x5b6078, 0.6),
    drop_target: hex(0xeed49f, 0.15),

    pill_bg: hex(0x494d64, 1.0),
    pill_border: hex(0x5b6078, 1.0),

    line_color: hex(0xeed49f, 1.0),
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
    grid_color: hex(0x363a4f, 1.0),
    axis_color: hex(0x494d64, 1.0),
    zero_line_color: hex(0x5b6078, 1.0),

    control_active: hex(0xa6da95, 1.0),
    control_active_track: hex(0xa6da95, 0.7),
    error_accent: hex(0xed8796, 1.0),
};

pub static CATPPUCCIN_LATTE: Theme = Theme {
    name: "Catppuccin Latte",
    font_family: "Berkeley Mono",

    bg_primary: hex(0xeff1f5, 1.0),
    bg_secondary: hex(0xe6e9ef, 1.0),
    bg_elevated: hex(0xdce0e8, 1.0),

    text_primary: hex(0x4c4f69, 1.0),
    text_secondary: hex(0x5c5f77, 1.0),
    text_tertiary: hex(0x6c6f85, 1.0),

    border_primary: hex(0xccd0da, 1.0),

    selection_bg: hex(0xbcc0cc, 1.0),
    text_selection: hex(0x7287fd, 0.3),
    drop_target: hex(0xdf8e1d, 0.15),

    pill_bg: hex(0xccd0da, 1.0),
    pill_border: hex(0xbcc0cc, 1.0),

    line_color: hex(0xdf8e1d, 1.0),
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
    grid_color: hex(0xdce0e8, 1.0),
    axis_color: hex(0xccd0da, 1.0),
    zero_line_color: hex(0xbcc0cc, 1.0),

    control_active: hex(0x40a02b, 1.0),
    control_active_track: hex(0x40a02b, 0.7),
    error_accent: hex(0xd20f39, 1.0),
};

pub static AYU_DARK: Theme = Theme {
    name: "Ayu Dark",
    font_family: "Berkeley Mono",

    bg_primary: hex(0x0b0e14, 1.0),
    bg_secondary: hex(0x0f131a, 1.0),
    bg_elevated: hex(0x1a1e28, 1.0),

    text_primary: hex(0xbfbdb6, 1.0),
    text_secondary: hex(0x565b66, 1.0),
    text_tertiary: hex(0x6c7380, 1.0),

    border_primary: hex(0x11151c, 1.0),

    selection_bg: hex(0x475266, 1.0),
    text_selection: hex(0x3d4455, 0.6),
    drop_target: hex(0x59c2ff, 0.15),

    pill_bg: hex(0x1a1e28, 1.0),
    pill_border: hex(0x2a2e38, 1.0),

    line_color: hex(0xe6b450, 1.0),
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
    grid_color: hex(0x11151c, 1.0),
    axis_color: hex(0x1a1e28, 1.0),
    zero_line_color: hex(0x2a2e38, 1.0),

    control_active: hex(0x7fd962, 1.0),
    control_active_track: hex(0x7fd962, 0.7),
    error_accent: hex(0xff3333, 1.0),
};

pub static EVERFOREST_DARK: Theme = Theme {
    name: "Everforest Dark",
    font_family: "Berkeley Mono",

    bg_primary: hex(0x2d353b, 1.0),
    bg_secondary: hex(0x232a2e, 1.0),
    bg_elevated: hex(0x3d484d, 1.0),

    text_primary: hex(0xd3c6aa, 1.0),
    text_secondary: hex(0x9da9a0, 1.0),
    text_tertiary: hex(0x859289, 1.0),

    border_primary: hex(0x475258, 1.0),

    selection_bg: hex(0x4f585e, 1.0),
    text_selection: hex(0x543a48, 0.6),
    drop_target: hex(0xdbbc7f, 0.15),

    pill_bg: hex(0x3d484d, 1.0),
    pill_border: hex(0x4f585e, 1.0),

    line_color: hex(0xdbbc7f, 1.0),
    line_colors: [
        hex(0xe69875, 1.0), // orange
        hex(0x7fbbb3, 1.0), // blue
        hex(0xa7c080, 1.0), // green
        hex(0xe67e80, 1.0), // red
        hex(0xd699b6, 1.0), // purple
        hex(0x83c092, 1.0), // aqua
        hex(0xdbbc7f, 1.0), // yellow
        hex(0xd699b6, 1.0), // pink
    ],
    grid_color: hex(0x3d484d, 1.0),
    axis_color: hex(0x475258, 1.0),
    zero_line_color: hex(0x56635f, 1.0),

    control_active: hex(0xa7c080, 1.0),
    control_active_track: hex(0xa7c080, 0.7),
    error_accent: hex(0xe67e80, 1.0),
};

pub static EVERFOREST_LIGHT: Theme = Theme {
    name: "Everforest Light",
    font_family: "Berkeley Mono",

    bg_primary: hex(0xfdf6e3, 1.0),
    bg_secondary: hex(0xf4f0d9, 1.0),
    bg_elevated: hex(0xefebd4, 1.0),

    text_primary: hex(0x5c6a72, 1.0),
    text_secondary: hex(0x829181, 1.0),
    text_tertiary: hex(0x939f91, 1.0),

    border_primary: hex(0xe6e2cc, 1.0),

    selection_bg: hex(0xe0dcc7, 1.0),
    text_selection: hex(0xeaedc8, 0.8),
    drop_target: hex(0xdfa000, 0.15),

    pill_bg: hex(0xefebd4, 1.0),
    pill_border: hex(0xe0dcc7, 1.0),

    line_color: hex(0xdfa000, 1.0),
    line_colors: [
        hex(0xf57d26, 1.0), // orange
        hex(0x3a94c5, 1.0), // blue
        hex(0x8da101, 1.0), // green
        hex(0xf85552, 1.0), // red
        hex(0xdf69ba, 1.0), // purple
        hex(0x35a77c, 1.0), // aqua
        hex(0xdfa000, 1.0), // yellow
        hex(0xdf69ba, 1.0), // pink
    ],
    grid_color: hex(0xefebd4, 1.0),
    axis_color: hex(0xe6e2cc, 1.0),
    zero_line_color: hex(0xbdc3af, 1.0),

    control_active: hex(0x8da101, 1.0),
    control_active_track: hex(0x8da101, 0.7),
    error_accent: hex(0xf85552, 1.0),
};

pub static ROSE_PINE: Theme = Theme {
    name: "Rosé Pine",
    font_family: "Berkeley Mono",

    bg_primary: hex(0x1f1d2e, 1.0),
    bg_secondary: hex(0x191724, 1.0),
    bg_elevated: hex(0x26233a, 1.0),

    text_primary: hex(0xe0def4, 1.0),
    text_secondary: hex(0x908caa, 1.0),
    text_tertiary: hex(0x6e6a86, 1.0),

    border_primary: hex(0x403d52, 1.0),

    selection_bg: hex(0x524f67, 1.0),
    text_selection: hex(0xc4a7e7, 0.3),
    drop_target: hex(0xf6c177, 0.15),

    pill_bg: hex(0x403d52, 1.0),
    pill_border: hex(0x524f67, 1.0),

    line_color: hex(0xf6c177, 1.0),
    line_colors: [
        hex(0xf6c177, 1.0), // gold
        hex(0x31748f, 1.0), // pine
        hex(0x9ccfd8, 1.0), // foam
        hex(0xeb6f92, 1.0), // love
        hex(0xc4a7e7, 1.0), // iris
        hex(0xebbcba, 1.0), // rose
        hex(0xf6c177, 1.0), // gold
        hex(0xeb6f92, 1.0), // love
    ],
    grid_color: hex(0x21202e, 1.0),
    axis_color: hex(0x403d52, 1.0),
    zero_line_color: hex(0x524f67, 1.0),

    control_active: hex(0x9ccfd8, 1.0),
    control_active_track: hex(0x9ccfd8, 0.7),
    error_accent: hex(0xeb6f92, 1.0),
};

pub static ROSE_PINE_MOON: Theme = Theme {
    name: "Rosé Pine Moon",
    font_family: "Berkeley Mono",

    bg_primary: hex(0x2a273f, 1.0),
    bg_secondary: hex(0x232136, 1.0),
    bg_elevated: hex(0x393552, 1.0),

    text_primary: hex(0xe0def4, 1.0),
    text_secondary: hex(0x908caa, 1.0),
    text_tertiary: hex(0x6e6a86, 1.0),

    border_primary: hex(0x44415a, 1.0),

    selection_bg: hex(0x56526e, 1.0),
    text_selection: hex(0xc4a7e7, 0.3),
    drop_target: hex(0xf6c177, 0.15),

    pill_bg: hex(0x44415a, 1.0),
    pill_border: hex(0x56526e, 1.0),

    line_color: hex(0xf6c177, 1.0),
    line_colors: [
        hex(0xf6c177, 1.0), // gold
        hex(0x3e8fb0, 1.0), // pine
        hex(0x9ccfd8, 1.0), // foam
        hex(0xeb6f92, 1.0), // love
        hex(0xc4a7e7, 1.0), // iris
        hex(0xea9a97, 1.0), // rose
        hex(0xf6c177, 1.0), // gold
        hex(0xea9a97, 1.0), // rose
    ],
    grid_color: hex(0x2a283e, 1.0),
    axis_color: hex(0x44415a, 1.0),
    zero_line_color: hex(0x56526e, 1.0),

    control_active: hex(0x9ccfd8, 1.0),
    control_active_track: hex(0x9ccfd8, 0.7),
    error_accent: hex(0xeb6f92, 1.0),
};

pub static ROSE_PINE_DAWN: Theme = Theme {
    name: "Rosé Pine Dawn",
    font_family: "Berkeley Mono",

    bg_primary: hex(0xfffaf3, 1.0),
    bg_secondary: hex(0xfaf4ed, 1.0),
    bg_elevated: hex(0xf2e9e1, 1.0),

    text_primary: hex(0x575279, 1.0),
    text_secondary: hex(0x797593, 1.0),
    text_tertiary: hex(0x9893a5, 1.0),

    border_primary: hex(0xdfdad9, 1.0),

    selection_bg: hex(0xcecacd, 1.0),
    text_selection: hex(0x907aa9, 0.3),
    drop_target: hex(0xea9d34, 0.15),

    pill_bg: hex(0xf2e9e1, 1.0),
    pill_border: hex(0xdfdad9, 1.0),

    line_color: hex(0xea9d34, 1.0),
    line_colors: [
        hex(0xea9d34, 1.0), // gold
        hex(0x286983, 1.0), // pine
        hex(0x56949f, 1.0), // foam
        hex(0xb4637a, 1.0), // love
        hex(0x907aa9, 1.0), // iris
        hex(0xd7827e, 1.0), // rose
        hex(0xea9d34, 1.0), // gold
        hex(0xb4637a, 1.0), // love
    ],
    grid_color: hex(0xf4ede8, 1.0),
    axis_color: hex(0xdfdad9, 1.0),
    zero_line_color: hex(0xcecacd, 1.0),

    control_active: hex(0x56949f, 1.0),
    control_active_track: hex(0x56949f, 0.7),
    error_accent: hex(0xb4637a, 1.0),
};

pub static MAKING_SOFTWARE: Theme = Theme {
    name: "Making Software",
    font_family: "Berkeley Mono",

    bg_primary: hex(0xf4f5f9, 1.0),
    bg_secondary: hex(0xeceef5, 1.0),
    bg_elevated: hex(0xffffff, 1.0),

    text_primary: hex(0x000000, 1.0),
    text_secondary: hex(0x3d4358, 1.0),
    text_tertiary: hex(0x7a819b, 1.0),

    border_primary: hex(0xb9c7fd, 1.0),

    selection_bg: hex(0xe6ebff, 1.0),
    text_selection: hex(0xffe680, 0.55),
    drop_target: hex(0x1342ff, 0.12),

    pill_bg: hex(0xe6ebff, 1.0),
    pill_border: hex(0xb9c7fd, 1.0),

    line_color: hex(0x1342ff, 1.0),
    line_colors: [
        hex(0x1342ff, 1.0), // cobalt
        hex(0xe35d3a, 1.0), // brick / coral
        hex(0x2f8f4a, 1.0), // forest green
        hex(0xc02434, 1.0), // ink red
        hex(0x7a3fd1, 1.0), // violet
        hex(0x0e8a8a, 1.0), // teal
        hex(0xd9a200, 1.0), // amber
        hex(0xc94f9b, 1.0), // magenta
    ],
    grid_color: hex(0xe6ebff, 1.0),
    axis_color: hex(0xb9c7fd, 1.0),
    zero_line_color: hex(0x97acff, 1.0),

    control_active: hex(0x1342ff, 1.0),
    control_active_track: hex(0x1342ff, 0.7),
    error_accent: hex(0xc02434, 1.0),
};

pub static DEPARTURE: Theme = Theme {
    name: "Departure",
    font_family: "Berkeley Mono",

    bg_primary: hex(0x1a1815, 1.0),
    bg_secondary: hex(0x131210, 1.0),
    bg_elevated: hex(0x252220, 1.0),

    text_primary: hex(0xf0a040, 1.0),   // bright amber
    text_secondary: hex(0xb07830, 1.0), // mid amber
    text_tertiary: hex(0x6e4c20, 1.0),  // dim amber

    border_primary: hex(0x3a2a18, 1.0),

    selection_bg: hex(0x3a2a18, 1.0),
    text_selection: hex(0xf0a040, 0.25),
    drop_target: hex(0xf0a040, 0.15),

    pill_bg: hex(0x2a1f12, 1.0),
    pill_border: hex(0x5a3e20, 1.0),

    line_color: hex(0xf0a040, 1.0),
    line_colors: [
        hex(0xf0a040, 1.0), // amber
        hex(0x4ad0d0, 1.0), // cyan
        hex(0x80c060, 1.0), // green
        hex(0xe05050, 1.0), // red
        hex(0xd870b0, 1.0), // magenta
        hex(0xf8d080, 1.0), // pale amber
        hex(0x5090e0, 1.0), // blue
        hex(0xb0a040, 1.0), // olive
    ],
    grid_color: hex(0x2a2018, 1.0),
    axis_color: hex(0x3a2a18, 1.0),
    zero_line_color: hex(0x5a3e20, 1.0),

    control_active: hex(0xf0a040, 1.0),
    control_active_track: hex(0xf0a040, 0.7),
    error_accent: hex(0xe05050, 1.0),
};

/// Load the embedded IBM Plex Mono fonts into gpui's text system.
///
/// Call once at startup before any view requests a font.
pub fn register_fonts(cx: &App) {
    cx.text_system()
        .add_fonts(vec![
            Cow::Borrowed(include_bytes!("../assets/fonts/IBMPlexMono-Regular.ttf")),
            Cow::Borrowed(include_bytes!("../assets/fonts/IBMPlexMono-Bold.ttf")),
            Cow::Borrowed(include_bytes!("../assets/fonts/IBMPlexMono-Italic.ttf")),
        ])
        .expect("failed to register embedded fonts");
}
