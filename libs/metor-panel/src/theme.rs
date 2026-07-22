use std::borrow::Cow;
use std::sync::Arc;

use gpui::{App, Global, Hsla, SharedString};

use crate::config::{self, FontConfig, PanelConfig};

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
    /// Translucent band over plot regions whose data is remote-only or
    /// still downloading.
    pub plot_gap_band: Hsla,
    /// Alpha applied to a trace's color when it renders as a min/max
    /// envelope (LoD buckets) instead of raw samples, so operators can
    /// tell the two apart at a glance.
    pub plot_envelope_alpha: f32,

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
    /// Drop shadow behind the window when Linux client-side decorations are
    /// active. Derived rather than per-palette: translucent black reads
    /// correctly over any compositor wallpaper.
    pub fn window_shadow(&self) -> Hsla {
        Hsla {
            h: 0.,
            s: 0.,
            l: 0.,
            a: 0.4,
        }
    }

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

    /// Tint for a component-frame edge in the system graph — the blue family,
    /// the calmer of the two edge kinds since frame edges dominate a mission.
    pub fn frame_edge_color(&self) -> Hsla {
        self.line_colors[1]
    }

    /// Tint for a message edge in the system graph — the purple family, set
    /// apart from the frame-edge blue so the two kinds read at a glance.
    pub fn msg_edge_color(&self) -> Hsla {
        self.line_colors[4]
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

    /// A theme token at reduced alpha, for tints and dimmed borders derived
    /// from accent colors without introducing new literals at call sites.
    pub fn dim(color: Hsla, alpha: f32) -> Hsla {
        Hsla { a: alpha, ..color }
    }

    /// Solid color for a sequence channel's run state (`run_state_index`: 0 = idle,
    /// 1 = running, 2 = completed, 3 = aborted, 4 = stopped, 5 = failed). Tracks the
    /// theme's `control_active`/`error_accent` for the running/failed poles; the
    /// intermediate states use fixed hues that read on both light and dark themes.
    pub fn run_state_color(&self, run_state_index: usize) -> Hsla {
        match run_state_index {
            1 => self.control_active,   // running
            2 => hex(0x4090e0, 1.0),    // completed — calm blue
            3 => hex(0xe0a030, 1.0),    // aborted — amber (commanded safe stop)
            4 => hex(0xe06820, 1.0),    // stopped — orange (hard, possibly unsafe)
            5 => self.error_accent,     // failed
            _ => self.text_tertiary,    // idle / unknown
        }
    }

    /// Low-alpha [`run_state_color`](Self::run_state_color) for swatch/row backgrounds.
    pub fn run_state_tint(&self, run_state_index: usize) -> Hsla {
        Hsla {
            a: 0.12,
            ..self.run_state_color(run_state_index)
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
        &KINTSUGI,
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
    plot_gap_band: hex(0x6b6560, 0.12),
    plot_envelope_alpha: 0.55,

    control_active: hex(0x40b060, 1.0),
    control_active_track: hex(0x40b060, 0.7),
    error_accent: hex(0xe04040, 1.0),
};

pub static CATPPUCCIN_MOCHA: Theme = Theme {
    name: "Catppuccin Mocha",

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
    plot_gap_band: hex(0x585b70, 0.12),
    plot_envelope_alpha: 0.55,

    control_active: hex(0xa6e3a1, 1.0),
    control_active_track: hex(0xa6e3a1, 0.7),
    error_accent: hex(0xf38ba8, 1.0),
};

pub static CATPPUCCIN_MACCHIATO: Theme = Theme {
    name: "Catppuccin Macchiato",

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
    plot_gap_band: hex(0x5b6078, 0.12),
    plot_envelope_alpha: 0.55,

    control_active: hex(0xa6da95, 1.0),
    control_active_track: hex(0xa6da95, 0.7),
    error_accent: hex(0xed8796, 1.0),
};

pub static CATPPUCCIN_LATTE: Theme = Theme {
    name: "Catppuccin Latte",

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
    plot_gap_band: hex(0xbcc0cc, 0.12),
    plot_envelope_alpha: 0.55,

    control_active: hex(0x40a02b, 1.0),
    control_active_track: hex(0x40a02b, 0.7),
    error_accent: hex(0xd20f39, 1.0),
};

pub static AYU_DARK: Theme = Theme {
    name: "Ayu Dark",

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
    plot_gap_band: hex(0x2a2e38, 0.12),
    plot_envelope_alpha: 0.55,

    control_active: hex(0x7fd962, 1.0),
    control_active_track: hex(0x7fd962, 0.7),
    error_accent: hex(0xff3333, 1.0),
};

pub static EVERFOREST_DARK: Theme = Theme {
    name: "Everforest Dark",

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
    plot_gap_band: hex(0x56635f, 0.12),
    plot_envelope_alpha: 0.55,

    control_active: hex(0xa7c080, 1.0),
    control_active_track: hex(0xa7c080, 0.7),
    error_accent: hex(0xe67e80, 1.0),
};

pub static EVERFOREST_LIGHT: Theme = Theme {
    name: "Everforest Light",

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
    plot_gap_band: hex(0xbdc3af, 0.12),
    plot_envelope_alpha: 0.55,

    control_active: hex(0x8da101, 1.0),
    control_active_track: hex(0x8da101, 0.7),
    error_accent: hex(0xf85552, 1.0),
};

pub static ROSE_PINE: Theme = Theme {
    name: "Rosé Pine",

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
    plot_gap_band: hex(0x524f67, 0.12),
    plot_envelope_alpha: 0.55,

    control_active: hex(0x9ccfd8, 1.0),
    control_active_track: hex(0x9ccfd8, 0.7),
    error_accent: hex(0xeb6f92, 1.0),
};

pub static ROSE_PINE_MOON: Theme = Theme {
    name: "Rosé Pine Moon",

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
    plot_gap_band: hex(0x56526e, 0.12),
    plot_envelope_alpha: 0.55,

    control_active: hex(0x9ccfd8, 1.0),
    control_active_track: hex(0x9ccfd8, 0.7),
    error_accent: hex(0xeb6f92, 1.0),
};

pub static ROSE_PINE_DAWN: Theme = Theme {
    name: "Rosé Pine Dawn",

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
    plot_gap_band: hex(0xcecacd, 0.12),
    plot_envelope_alpha: 0.55,

    control_active: hex(0x56949f, 1.0),
    control_active_track: hex(0x56949f, 0.7),
    error_accent: hex(0xb4637a, 1.0),
};

pub static MAKING_SOFTWARE: Theme = Theme {
    name: "Making Software",

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
    plot_gap_band: hex(0x97acff, 0.12),
    plot_envelope_alpha: 0.55,

    control_active: hex(0x1342ff, 1.0),
    control_active_track: hex(0x1342ff, 0.7),
    error_accent: hex(0xc02434, 1.0),
};

pub static DEPARTURE: Theme = Theme {
    name: "Departure",

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
    plot_gap_band: hex(0x5a3e20, 0.12),
    plot_envelope_alpha: 0.55,

    control_active: hex(0xf0a040, 1.0),
    control_active_track: hex(0xf0a040, 0.7),
    error_accent: hex(0xe05050, 1.0),
};

/// Port of the "Kintsugi" VS Code theme (github.com/ahatem/vscode-kintsugi),
/// **Dark Flared** variant: the same warm charcoal surfaces as the standard
/// dark theme, but with the Flared variant's hotter, earth-toned accents —
/// terracotta, burnt orange, and golden amber — like fresh gold seams in the
/// pottery repair it's named for. The categorical line palette leads with those
/// warm tones, keeping a few cool anchors so series stay distinguishable.
pub static KINTSUGI: Theme = Theme {
    name: "Kintsugi Flared",

    bg_primary: hex(0x161618, 1.0),
    bg_secondary: hex(0x131314, 1.0),
    bg_elevated: hex(0x292928, 1.0),

    text_primary: hex(0xdddddd, 1.0),
    text_secondary: hex(0x969b8c, 1.0),
    text_tertiary: hex(0x75715e, 1.0),

    border_primary: hex(0x2a2a28, 1.0),

    selection_bg: hex(0x393b31, 1.0),
    text_selection: hex(0x6c7a8a, 0.4),
    drop_target: hex(0xdbad49, 0.15),

    pill_bg: hex(0x20201f, 1.0),
    pill_border: hex(0x3a3a36, 1.0),

    line_color: hex(0xdbad49, 1.0),
    line_colors: [
        hex(0xdbad49, 1.0), // golden amber
        hex(0x6c7a8a, 1.0), // blue
        hex(0xa3be8c, 1.0), // sage green
        hex(0xd66848, 1.0), // terracotta
        hex(0xb3a3d3, 1.0), // purple
        hex(0x678e87, 1.0), // teal
        hex(0xe08542, 1.0), // burnt orange
        hex(0x8fa3b3, 1.0), // slate
    ],
    grid_color: hex(0x1d1d1c, 1.0),
    axis_color: hex(0x444444, 1.0),
    zero_line_color: hex(0x5c584f, 1.0),
    plot_gap_band: hex(0x5c584f, 0.12),
    plot_envelope_alpha: 0.55,

    control_active: hex(0xdbad49, 1.0),
    control_active_track: hex(0xdbad49, 0.7),
    error_accent: hex(0xd66848, 1.0),
};

/// Family name of the font bundled into the binary. This is the guaranteed
/// fallback — it's always registered, so it always resolves.
pub const BUNDLED_FAMILY: &str = "IoskeleyMono Nerd Font";

/// Family the [`FontConfig::Auto`] mode reaches for first, when the system has
/// it installed. Falls back to [`BUNDLED_FAMILY`] otherwise.
pub const PREFERRED_FAMILY: &str = "Berkeley Mono";

/// Load the embedded fallback font into gpui's text system.
///
/// Call once at startup before any view requests a font. These bytes ship in
/// git as real data (not LFS), so the call can't fail the way the old
/// LFS-tracked fonts did when the crate was consumed without LFS hydration.
pub fn register_fonts(cx: &App) {
    cx.text_system()
        .add_fonts(vec![
            Cow::Borrowed(include_bytes!(
                "../assets/fonts/IoskeleyMonoNerdFont-Regular.ttf"
            )),
            Cow::Borrowed(include_bytes!(
                "../assets/fonts/IoskeleyMonoNerdFont-Bold.ttf"
            )),
            Cow::Borrowed(include_bytes!(
                "../assets/fonts/IoskeleyMonoNerdFont-Italic.ttf"
            )),
        ])
        .expect("failed to register embedded fonts");
}

/// Resolve a [`FontConfig`] to a concrete family the text system can render.
///
/// Both modes degrade to [`BUNDLED_FAMILY`] when their preferred family isn't
/// available, so the UI never ends up with an unresolvable font. Must be called
/// after [`register_fonts`] so the bundled family appears in the system list.
pub fn resolve_font_family(cx: &App, cfg: &PanelConfig) -> SharedString {
    let available = cx.text_system().all_font_names();
    let has = |name: &str| available.iter().any(|f| f == name);

    match &cfg.font {
        FontConfig::Auto => {
            if has(PREFERRED_FAMILY) {
                SharedString::new_static(PREFERRED_FAMILY)
            } else {
                SharedString::new_static(BUNDLED_FAMILY)
            }
        }
        FontConfig::Family(name) => {
            if has(name) {
                SharedString::from(name.clone())
            } else {
                tracing::warn!("font '{name}' not found; using bundled {BUNDLED_FAMILY}");
                SharedString::new_static(BUNDLED_FAMILY)
            }
        }
    }
}

/// The resolved UI font plus the [`FontConfig`] it came from.
///
/// Installed as a [`Global`] at startup. The render tree reads
/// [`font_family`] from here instead of from [`Theme`], so the font is
/// independent of the active color palette.
pub struct FontSettings {
    pub family: SharedString,
    pub config: PanelConfig,
}

impl Global for FontSettings {}

/// The family the UI should render with. Cheap: clones a [`SharedString`].
pub fn font_family(cx: &App) -> SharedString {
    cx.global::<FontSettings>().family.clone()
}

/// Apply a new font choice: re-resolve, swap the global, and persist it so the
/// choice survives a restart. The palette dismissal re-renders the root, which
/// picks up the new family.
pub fn set_font(cx: &mut App, font: FontConfig) {
    // Start from the live config so unrelated fields (e.g. `leader`) survive.
    let mut config = cx.global::<FontSettings>().config.clone();
    config.font = font;
    let family = resolve_font_family(cx, &config);
    if let Err(e) = config::save(&config) {
        tracing::error!(%e, "save config failed");
    }
    cx.set_global(FontSettings { family, config });
}
