//! Colors, palettes and the shared control metrics.
//!
//! Every color an element draws comes from [`Theme`]; no element hardcodes an [`Hsla`]. The
//! active palette is a [`Global`] ([`ActiveTheme`]), and [`ThemeRegistry`] holds every palette
//! a picker offers.

use std::sync::Arc;

use gpui::{App, Global, Hsla, Pixels, Rems, SharedString, Window, px, rems};

/// Height of a one-line control: rows, titlebar controls, tabs, pane headers and filter bars,
/// so chrome lines up wherever two of them meet.
pub const CONTROL_HEIGHT: f32 = 28.0;

/// Corner radius of a control's surface: hover and selection pills, tabs, titlebar segments.
pub const CONTROL_RADIUS: f32 = 4.0;

/// Backdrop blur (gaussian sigma) under floating surfaces, such as the palette and context
/// menus, so they read as frosted glass over whatever they cover. Only Metal draws it.
pub const SURFACE_BLUR: Pixels = px(16.0);

/// Build an [`Hsla`] from a packed `0xRRGGBB` literal in a const context.
///
/// Theme tables are `static`, so the conversion has to happen without
/// floating-point intrinsics or runtime allocation. Public so consumer
/// palettes are written the same way the built-ins are.
pub const fn hex(rgb: u32, a: f32) -> Hsla {
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

/// Complete color palette for the UI.
///
/// All colors live here. Elements never hardcode an [`Hsla`] literal; they reference a theme
/// field instead, so palettes stay swappable.
///
/// Apps add palettes with [`register_theme`]. Build one by overriding a built-in rather than
/// listing every field, so fields added later inherit a sensible value instead of breaking the
/// build:
///
/// ```
/// use metor_gpui::{DARK, Theme, hex};
///
/// let mission = Theme {
///     name: "Mission".into(),
///     bg_primary: hex(0x101418, 1.0),
///     ..DARK.clone()
/// };
/// # let _ = mission;
/// ```
#[derive(Clone)]
pub struct Theme {
    /// Display name in the theme picker, and the registry's identity: a
    /// second theme with the same name replaces the first.
    pub name: SharedString,
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
    /// Sky and ground of an attitude ball, and any other horizon-split
    /// instrument. Named rather than indexed out of `line_colors`, because
    /// the categorical palette's order is a per-theme choice — one theme
    /// leads with blue — and a horizon painted upside down reads as a fault
    /// in the instrument rather than as a theme.
    pub horizon_sky: Hsla,
    pub horizon_ground: Hsla,
    pub grid_color: Hsla,
    pub axis_color: Hsla,
    pub zero_line_color: Hsla,
    /// Tone ramp for intensity fields — spectrogram magnitude, value
    /// density. Ordered low intensity to high and monotonic in lightness,
    /// starting near `bg_secondary` so a barely-occupied cell fades into
    /// the plot and ending far from it so a hot cell cannot be missed.
    pub density_stops: [Hsla; 5],

    /// "On" color for checkboxes, switches, and traffic-light tiles. Replaces
    /// per-call-site indexing into `line_colors`.
    pub control_active: Hsla,
    /// Error accent (build failures, invalid arg pills). Used wherever the
    /// UI needs a "this is wrong" tint.
    pub error_accent: Hsla,
    /// Colors for the lifecycle states of jobs, tasks and sequences, or `None` to derive them
    /// from the accents above (see [`StateColors::derive`]).
    pub state: Option<StateColors>,
}

/// A generic lifecycle state, drawn in its [`StateColors`] color.
///
/// The set is deliberately generic: each app maps its own states onto it, so every palette
/// colors them consistently.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StateTone {
    /// Not started and not waiting on anything.
    Idle,
    /// Waiting to start.
    Queued,
    /// Running now.
    Active,
    Success,
    Failure,
    /// Stopped on request before it finished.
    Cancelled,
    /// Never ran because it was not needed.
    Skipped,
}

/// The color of each [`StateTone`] in one palette.
#[derive(Clone, Copy, Debug)]
pub struct StateColors {
    pub idle: Hsla,
    pub queued: Hsla,
    pub active: Hsla,
    pub success: Hsla,
    pub failure: Hsla,
    pub cancelled: Hsla,
    pub skipped: Hsla,
}

impl StateColors {
    /// State colors derived from a palette's existing tokens: the accent for active work, the
    /// positive and negative accents for the outcomes, and the text tones for everything at rest.
    pub fn derive(theme: &Theme) -> Self {
        Self {
            idle: theme.text_tertiary,
            queued: theme.text_secondary,
            active: theme.line_color,
            success: theme.control_active,
            failure: theme.error_accent,
            cancelled: theme.text_secondary,
            skipped: theme.text_tertiary,
        }
    }

    /// The color of `tone`.
    pub fn color(&self, tone: StateTone) -> Hsla {
        match tone {
            StateTone::Idle => self.idle,
            StateTone::Queued => self.queued,
            StateTone::Active => self.active,
            StateTone::Success => self.success,
            StateTone::Failure => self.failure,
            StateTone::Cancelled => self.cancelled,
            StateTone::Skipped => self.skipped,
        }
    }
}

impl Theme {
    /// How much of a sidebar's tint covers the native blurred backdrop behind it.
    pub const BACKDROP_TINT: f32 = 0.7;

    /// The side tab bar's tint over the native blurred backdrop.
    pub fn tab_sidebar_bg(&self) -> Hsla {
        Self::dim(self.bg_secondary, Self::BACKDROP_TINT)
    }

    /// `bg_elevated` for floating surfaces. On macOS, where gpui blurs what lies behind, it lets
    /// the blurred backdrop through; elsewhere nothing blurs it, and translucency would only make
    /// rows harder to read.
    pub fn surface_bg(&self) -> Hsla {
        if cfg!(target_os = "macos") {
            Self::dim(self.bg_elevated, 0.75)
        } else {
            self.bg_elevated
        }
    }

    /// The dimming laid over a page behind a modal dialog.
    pub fn scrim(&self) -> Hsla {
        Self::dim(self.bg_primary, 0.6)
    }

    /// Track behind a checked switch knob: `control_active` with less presence.
    pub fn control_active_track(&self) -> Hsla {
        Self::dim(self.control_active, 0.7)
    }

    /// The colors of the lifecycle states.
    pub fn state_colors(&self) -> StateColors {
        self.state.unwrap_or_else(|| StateColors::derive(self))
    }

    /// The color of a lifecycle state.
    pub fn state_color(&self, tone: StateTone) -> Hsla {
        self.state_colors().color(tone)
    }

    /// A theme token at reduced alpha, for tints and dimmed borders derived from accent colors
    /// without introducing new literals at call sites.
    pub fn dim(color: Hsla, alpha: f32) -> Hsla {
        Hsla { a: alpha, ..color }
    }
}

/// Component-wise RGB blend of `a` into `b` by `t`, keeping `a`'s alpha.
pub fn lerp_color(a: Hsla, b: Hsla, t: f32) -> Hsla {
    let x = gpui::Rgba::from(a);
    let y = gpui::Rgba::from(b);
    let lerp = |p: f32, q: f32| p + (q - p) * t;
    gpui::Rgba {
        r: lerp(x.r, y.r),
        g: lerp(x.g, y.g),
        b: lerp(x.b, y.b),
        a: x.a,
    }
    .into()
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
    // Views read the theme as a global, which no cached view tracks: redraw
    // everything once, bypassing the caches.
    cx.refresh_windows();
}

/// Every theme compiled into the binary, in the order the theme picker
/// displays them. [`ThemeRegistry`] starts from this list.
pub fn builtin_themes() -> &'static [&'static Theme] {
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

/// The themes the picker offers: the built-ins in display order, then
/// consumer themes in registration order.
///
/// Names are identities, so registering a theme whose name is already
/// present replaces that entry in place — a consumer can restyle "Dark"
/// without the picker growing a second one or the order shifting.
pub struct ThemeRegistry {
    themes: Vec<Arc<Theme>>,
}

impl Global for ThemeRegistry {}

impl Default for ThemeRegistry {
    fn default() -> Self {
        Self {
            themes: builtin_themes()
                .iter()
                .map(|theme| Arc::new((*theme).clone()))
                .collect(),
        }
    }
}

impl ThemeRegistry {
    pub fn register(&mut self, theme: Theme) {
        let theme = Arc::new(theme);
        match self.themes.iter_mut().find(|t| t.name == theme.name) {
            Some(slot) => *slot = theme,
            None => self.themes.push(theme),
        }
    }

    pub fn themes(&self) -> &[Arc<Theme>] {
        &self.themes
    }

    pub fn find(&self, name: &str) -> Option<Arc<Theme>> {
        self.themes.iter().find(|t| t.name == name).cloned()
    }

    /// The theme a session starts in: `name` if registered, otherwise the
    /// registry's first entry — "Dark", or whatever a consumer replaced it
    /// with — so a misspelt default still boots into a coherent palette.
    pub fn startup_theme(&self, name: Option<&str>) -> Arc<Theme> {
        if let Some(name) = name {
            match self.find(name) {
                Some(theme) => return theme,
                None => tracing::warn!(name, "unknown default theme; falling back"),
            }
        }
        self.themes[0].clone()
    }
}

/// Add `theme` to the picker, replacing a registered theme of the same name.
/// When the replaced theme is the active one, the replacement takes effect
/// immediately.
pub fn register_theme(cx: &mut App, theme: Theme) {
    let active = cx
        .try_global::<ActiveTheme>()
        .is_some_and(|active| active.0.name == theme.name);
    let name = theme.name.clone();
    let registry = cx.default_global::<ThemeRegistry>();
    registry.register(theme);
    if active && let Some(theme) = registry.find(&name) {
        set_theme(cx, theme);
    }
}

/// Every theme the picker offers. Falls back to the built-ins when no
/// registry is installed (tests that set a theme directly).
pub fn themes(cx: &App) -> Vec<Arc<Theme>> {
    match cx.try_global::<ThemeRegistry>() {
        Some(registry) => registry.themes().to_vec(),
        None => ThemeRegistry::default().themes,
    }
}

/// Seed the registry with the built-ins plus `extra`, then activate the startup theme, `default`
/// or the first registered. Runs once at boot, before any window opens.
pub fn install_themes(cx: &mut App, extra: Vec<Theme>, default: Option<&str>) {
    let mut registry = ThemeRegistry::default();
    for theme in extra {
        registry.register(theme);
    }
    set_theme(cx, registry.startup_theme(default));
    cx.set_global(registry);
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
    name: SharedString::new_static("Dark"),

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
    horizon_sky: hex(0x4aa0e0, 1.0),
    horizon_ground: hex(0xff7c1f, 1.0),
    grid_color: hex(0x2e2b28, 1.0),
    axis_color: hex(0x4d4843, 1.0),
    zero_line_color: hex(0x6b6560, 1.0),
    density_stops: [
        hex(0x201e1c, 1.0),
        hex(0x175684, 1.0),
        hex(0x882ccd, 1.0),
        hex(0xff9e59, 1.0),
        hex(0xebdecc, 1.0),
    ],

    control_active: hex(0x40b060, 1.0),
    error_accent: hex(0xe04040, 1.0),
    state: None,
};

pub static CATPPUCCIN_MOCHA: Theme = Theme {
    name: SharedString::new_static("Catppuccin Mocha"),

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
    horizon_sky: hex(0x89b4fa, 1.0),
    horizon_ground: hex(0xfab387, 1.0),
    grid_color: hex(0x313244, 1.0),
    axis_color: hex(0x45475a, 1.0),
    zero_line_color: hex(0x585b70, 1.0),
    density_stops: [
        hex(0x181828, 1.0),
        hex(0x063e98, 1.0),
        hex(0x7515e7, 1.0),
        hex(0xf89b61, 1.0),
        hex(0xc4cff2, 1.0),
    ],

    control_active: hex(0xa6e3a1, 1.0),
    error_accent: hex(0xf38ba8, 1.0),
    state: None,
};

pub static CATPPUCCIN_MACCHIATO: Theme = Theme {
    name: SharedString::new_static("Catppuccin Macchiato"),

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
    horizon_sky: hex(0x8aadf4, 1.0),
    horizon_ground: hex(0xf5a97f, 1.0),
    grid_color: hex(0x363a4f, 1.0),
    axis_color: hex(0x494d64, 1.0),
    zero_line_color: hex(0x5b6078, 1.0),
    density_stops: [
        hex(0x25283e, 1.0),
        hex(0x1042a8, 1.0),
        hex(0x7b22ea, 1.0),
        hex(0xf49e6e, 1.0),
        hex(0xc3cdf4, 1.0),
    ],

    control_active: hex(0xa6da95, 1.0),
    error_accent: hex(0xed8796, 1.0),
    state: None,
};

pub static CATPPUCCIN_LATTE: Theme = Theme {
    name: SharedString::new_static("Catppuccin Latte"),

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
    horizon_sky: hex(0x1e66f5, 1.0),
    horizon_ground: hex(0xfe640b, 1.0),
    grid_color: hex(0xdce0e8, 1.0),
    axis_color: hex(0xccd0da, 1.0),
    zero_line_color: hex(0xbcc0cc, 1.0),
    density_stops: [
        hex(0xd6dbe6, 1.0),
        hex(0xfea46f, 1.0),
        hex(0x8432ee, 1.0),
        hex(0x0949c9, 1.0),
        hex(0x383a4d, 1.0),
    ],

    control_active: hex(0x40a02b, 1.0),
    error_accent: hex(0xd20f39, 1.0),
    state: None,
};

pub static AYU_DARK: Theme = Theme {
    name: SharedString::new_static("Ayu Dark"),

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
    horizon_sky: hex(0x59c2ff, 1.0),
    horizon_ground: hex(0xe6b450, 1.0),
    grid_color: hex(0x11151c, 1.0),
    axis_color: hex(0x1a1e28, 1.0),
    zero_line_color: hex(0x2a2e38, 1.0),
    density_stops: [
        hex(0x161c28, 1.0),
        hex(0x00629c, 1.0),
        hex(0x7c00fa, 1.0),
        hex(0xeac16e, 1.0),
        hex(0xdeddd9, 1.0),
    ],

    control_active: hex(0x7fd962, 1.0),
    error_accent: hex(0xff3333, 1.0),
    state: None,
};

pub static EVERFOREST_DARK: Theme = Theme {
    name: SharedString::new_static("Everforest Dark"),

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
    horizon_sky: hex(0x7fbbb3, 1.0),
    horizon_ground: hex(0xe69875, 1.0),
    grid_color: hex(0x3d484d, 1.0),
    axis_color: hex(0x475258, 1.0),
    zero_line_color: hex(0x56635f, 1.0),
    density_stops: [
        hex(0x2b353b, 1.0),
        hex(0x407972, 1.0),
        hex(0xba5484, 1.0),
        hex(0xe79d7b, 1.0),
        hex(0xe7dfd0, 1.0),
    ],

    control_active: hex(0xa7c080, 1.0),
    error_accent: hex(0xe67e80, 1.0),
    state: None,
};

pub static EVERFOREST_LIGHT: Theme = Theme {
    name: SharedString::new_static("Everforest Light"),

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
    horizon_sky: hex(0x3a94c5, 1.0),
    horizon_ground: hex(0xf57d26, 1.0),
    grid_color: hex(0xefebd4, 1.0),
    axis_color: hex(0xe6e2cc, 1.0),
    zero_line_color: hex(0xbdc3af, 1.0),
    density_stops: [
        hex(0xefe9c4, 1.0),
        hex(0xf8a96f, 1.0),
        hex(0xd745a9, 1.0),
        hex(0x2f79a1, 1.0),
        hex(0x3b4449, 1.0),
    ],

    control_active: hex(0x8da101, 1.0),
    error_accent: hex(0xf85552, 1.0),
    state: None,
};

pub static ROSE_PINE: Theme = Theme {
    name: SharedString::new_static("Rosé Pine"),

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
    horizon_sky: hex(0x31748f, 1.0),
    horizon_ground: hex(0xf6c177, 1.0),
    grid_color: hex(0x21202e, 1.0),
    axis_color: hex(0x403d52, 1.0),
    zero_line_color: hex(0x524f67, 1.0),
    density_stops: [
        hex(0x211e31, 1.0),
        hex(0x2b667e, 1.0),
        hex(0x7b3ac9, 1.0),
        hex(0xf5ba68, 1.0),
        hex(0xcdc9ed, 1.0),
    ],

    control_active: hex(0x9ccfd8, 1.0),
    error_accent: hex(0xeb6f92, 1.0),
    state: None,
};

pub static ROSE_PINE_MOON: Theme = Theme {
    name: SharedString::new_static("Rosé Pine Moon"),

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
    horizon_sky: hex(0x3e8fb0, 1.0),
    horizon_ground: hex(0xf6c177, 1.0),
    grid_color: hex(0x2a283e, 1.0),
    axis_color: hex(0x44415a, 1.0),
    zero_line_color: hex(0x56526e, 1.0),
    density_stops: [
        hex(0x2a2844, 1.0),
        hex(0x32728d, 1.0),
        hex(0x8245cc, 1.0),
        hex(0xf5bd6e, 1.0),
        hex(0xcdc9ed, 1.0),
    ],

    control_active: hex(0x9ccfd8, 1.0),
    error_accent: hex(0xeb6f92, 1.0),
    state: None,
};

pub static ROSE_PINE_DAWN: Theme = Theme {
    name: SharedString::new_static("Rosé Pine Dawn"),

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
    horizon_sky: hex(0x286983, 1.0),
    horizon_ground: hex(0xea9d34, 1.0),
    grid_color: hex(0xf4ede8, 1.0),
    axis_color: hex(0xdfdad9, 1.0),
    zero_line_color: hex(0xcecacd, 1.0),
    density_stops: [
        hex(0xf5e8d9, 1.0),
        hex(0xf3c689, 1.0),
        hex(0x937eab, 1.0),
        hex(0x3284a5, 1.0),
        hex(0x39364f, 1.0),
    ],

    control_active: hex(0x56949f, 1.0),
    error_accent: hex(0xb4637a, 1.0),
    state: None,
};

pub static MAKING_SOFTWARE: Theme = Theme {
    name: SharedString::new_static("Making Software"),

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
    horizon_sky: hex(0x1342ff, 1.0),
    horizon_ground: hex(0xe35d3a, 1.0),
    grid_color: hex(0xe6ebff, 1.0),
    axis_color: hex(0xb9c7fd, 1.0),
    zero_line_color: hex(0x97acff, 1.0),
    density_stops: [
        hex(0xdbdfed, 1.0),
        hex(0xffdd78, 1.0),
        hex(0x8651d5, 1.0),
        hex(0x002ad5, 1.0),
        hex(0x363c4e, 1.0),
    ],

    control_active: hex(0x1342ff, 1.0),
    error_accent: hex(0xc02434, 1.0),
    state: None,
};

pub static DEPARTURE: Theme = Theme {
    name: SharedString::new_static("Departure"),

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
    horizon_sky: hex(0x4ad0d0, 1.0),
    horizon_ground: hex(0xf0a040, 1.0),
    grid_color: hex(0x2a2018, 1.0),
    axis_color: hex(0x3a2a18, 1.0),
    zero_line_color: hex(0x5a3e20, 1.0),
    density_stops: [
        hex(0x1f1d19, 1.0),
        hex(0x1f7878, 1.0),
        hex(0xc2358c, 1.0),
        hex(0xf3b264, 1.0),
        hex(0xfbe6bb, 1.0),
    ],

    control_active: hex(0xf0a040, 1.0),
    error_accent: hex(0xe05050, 1.0),
    state: None,
};

/// Port of the "Kintsugi" VS Code theme (github.com/ahatem/vscode-kintsugi),
/// **Dark Flared** variant: the same warm charcoal surfaces as the standard
/// dark theme, but with the Flared variant's hotter, earth-toned accents —
/// terracotta, burnt orange, and golden amber — like fresh gold seams in the
/// pottery repair it's named for. The categorical line palette leads with those
/// warm tones, keeping a few cool anchors so series stay distinguishable.
pub static KINTSUGI: Theme = Theme {
    name: SharedString::new_static("Kintsugi Flared"),

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
    horizon_sky: hex(0x6c7a8a, 1.0),
    horizon_ground: hex(0xdbad49, 1.0),
    grid_color: hex(0x1d1d1c, 1.0),
    axis_color: hex(0x444444, 1.0),
    zero_line_color: hex(0x5c584f, 1.0),
    density_stops: [
        hex(0x1c1c1f, 1.0),
        hex(0x444c57, 1.0),
        hex(0x6e51a8, 1.0),
        hex(0xe4c074, 1.0),
        hex(0xdbdbdb, 1.0),
    ],

    control_active: hex(0xdbad49, 1.0),
    error_accent: hex(0xd66848, 1.0),
    state: None,
};

/// The rem gpui starts every window at. Outside a zoomed view one rem is always this many
/// pixels.
const BASE_REM: f32 = 16.0;

/// A length designed in pixels that follows the rem instead of pinning it.
///
/// Identical to `px(n)` at the base rem, which is everywhere but inside a zoomed view, where the
/// zoom scales the rem and so everything sized through here. Views size their text, spacing and
/// rows with it; hairline borders stay in `px` so they never vanish when zoomed out.
pub fn ui_px(n: f32) -> Rems {
    rems(n / BASE_REM)
}

/// How far the current rem is from the base: the factor text and strokes a view shapes and
/// paints itself multiply by to follow [`ui_px`] layout.
pub fn ui_scale(window: &Window) -> f32 {
    window.rem_size() / px(BASE_REM)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: Hsla, b: Hsla) -> bool {
        (a.h - b.h).abs() < 1e-5
            && (a.s - b.s).abs() < 1e-5
            && (a.l - b.l).abs() < 1e-5
            && (a.a - b.a).abs() < 1e-5
    }

    fn named(name: &'static str) -> Theme {
        Theme {
            name: name.into(),
            bg_primary: hex(0x101418, 1.0),
            ..DARK.clone()
        }
    }

    fn names(registry: &ThemeRegistry) -> Vec<&str> {
        registry.themes().iter().map(|t| t.name.as_ref()).collect()
    }

    /// Consumer themes follow the built-ins, and re-registering a name
    /// restyles that entry where it stands instead of listing it twice.
    #[test]
    fn registry_appends_and_replaces_by_name() {
        let mut registry = ThemeRegistry::default();
        let builtins: Vec<&str> = builtin_themes().iter().map(|t| t.name.as_ref()).collect();
        assert_eq!(names(&registry), builtins);

        registry.register(named("Mission"));
        registry.register(named("Night Ops"));
        let mut expected = builtins.clone();
        expected.extend(["Mission", "Night Ops"]);
        assert_eq!(names(&registry), expected);

        registry.register(named("Dark"));
        registry.register(Theme {
            bg_primary: hex(0x202020, 1.0),
            ..named("Mission")
        });
        assert_eq!(names(&registry), expected);
        assert!(approx(registry.themes()[0].bg_primary, hex(0x101418, 1.0)));
        let mission = registry.find("Mission").unwrap();
        assert!(approx(mission.bg_primary, hex(0x202020, 1.0)));
    }

    #[test]
    fn startup_theme_falls_back_to_dark() {
        let mut registry = ThemeRegistry::default();
        registry.register(named("Mission"));
        assert_eq!(registry.startup_theme(None).name, "Dark");
        assert_eq!(registry.startup_theme(Some("Nope")).name, "Dark");
        assert_eq!(registry.startup_theme(Some("Mission")).name, "Mission");
        assert_eq!(
            registry.startup_theme(Some("Rosé Pine")).name,
            ROSE_PINE.name
        );
    }

    #[gpui::test]
    fn install_activates_the_default_and_registering_the_active_name_repaints(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            install_themes(cx, vec![named("Mission")], Some("Mission"));
            assert_eq!(theme(cx).name, "Mission");
            assert_eq!(themes(cx).last().unwrap().name, "Mission");

            register_theme(
                cx,
                Theme {
                    bg_primary: hex(0x202020, 1.0),
                    ..named("Mission")
                },
            );
            assert!(approx(theme(cx).bg_primary, hex(0x202020, 1.0)));
        });
    }

    #[test]
    fn every_palette_derives_its_state_colors() {
        for theme in builtin_themes() {
            assert!(approx(
                theme.state_color(StateTone::Success),
                theme.control_active
            ));
            assert!(approx(
                theme.state_color(StateTone::Failure),
                theme.error_accent
            ));
            assert!(approx(
                theme.state_color(StateTone::Active),
                theme.line_color
            ));
        }
    }
}
