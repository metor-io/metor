use gpui::Hsla;

pub struct Theme {
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
    bg_primary: Hsla {
        h: 0.083,
        s: 0.08,
        l: 0.12,
        a: 1.0,
    },
    bg_secondary: Hsla {
        h: 0.083,
        s: 0.08,
        l: 0.08,
        a: 1.0,
    },
    bg_elevated: Hsla {
        h: 0.083,
        s: 0.08,
        l: 0.14,
        a: 1.0,
    },

    text_primary: Hsla {
        h: 0.1,
        s: 0.6,
        l: 0.95,
        a: 1.0,
    },
    text_secondary: Hsla {
        h: 0.083,
        s: 0.05,
        l: 0.50,
        a: 1.0,
    },
    text_tertiary: Hsla {
        h: 0.083,
        s: 0.05,
        l: 0.40,
        a: 1.0,
    },

    border_primary: Hsla {
        h: 0.083,
        s: 0.06,
        l: 0.20,
        a: 1.0,
    },

    selection_bg: Hsla {
        h: 0.083,
        s: 0.10,
        l: 0.20,
        a: 1.0,
    },
    text_selection: Hsla {
        h: 0.583,
        s: 0.5,
        l: 0.35,
        a: 0.6,
    },
    drop_target: Hsla {
        h: 0.069,
        s: 1.0,
        l: 0.56,
        a: 0.15,
    },

    pill_bg: Hsla {
        h: 0.0,
        s: 0.0,
        l: 0.25,
        a: 1.0,
    },
    pill_border: Hsla {
        h: 0.0,
        s: 0.0,
        l: 0.35,
        a: 1.0,
    },

    line_color: Hsla {
        h: 0.069,
        s: 1.0,
        l: 0.56,
        a: 1.0,
    },
    line_colors: [
        Hsla { h: 0.069, s: 1.0, l: 0.56, a: 1.0 }, // orange
        Hsla { h: 0.583, s: 0.8, l: 0.56, a: 1.0 }, // blue
        Hsla { h: 0.333, s: 0.7, l: 0.50, a: 1.0 }, // green
        Hsla { h: 0.0,   s: 0.8, l: 0.55, a: 1.0 }, // red
        Hsla { h: 0.75,  s: 0.7, l: 0.60, a: 1.0 }, // purple
        Hsla { h: 0.5,   s: 0.7, l: 0.55, a: 1.0 }, // cyan
        Hsla { h: 0.15,  s: 0.9, l: 0.55, a: 1.0 }, // yellow
        Hsla { h: 0.917, s: 0.7, l: 0.60, a: 1.0 }, // pink
    ],
    grid_color: Hsla {
        h: 0.083,
        s: 0.06,
        l: 0.18,
        a: 1.0,
    },
    axis_color: Hsla {
        h: 0.083,
        s: 0.08,
        l: 0.30,
        a: 1.0,
    },
    zero_line_color: Hsla {
        h: 0.083,
        s: 0.10,
        l: 0.40,
        a: 1.0,
    },
};
