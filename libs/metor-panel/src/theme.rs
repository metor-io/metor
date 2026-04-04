use gpui::Hsla;

pub struct Theme {
    pub bg_primary: Hsla,
    pub bg_secondary: Hsla,

    pub text_primary: Hsla,
    pub text_secondary: Hsla,
    pub text_tertiary: Hsla,

    pub border_primary: Hsla,

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

    // Pumpkin orange line
    line_color: Hsla {
        h: 0.069,
        s: 1.0,
        l: 0.56,
        a: 1.0,
    },
    line_colors: [
        // orange (same as line_color)
        Hsla { h: 0.069, s: 1.0, l: 0.56, a: 1.0 },
        // blue
        Hsla { h: 0.583, s: 0.8, l: 0.56, a: 1.0 },
        // green
        Hsla { h: 0.333, s: 0.7, l: 0.50, a: 1.0 },
        // red
        Hsla { h: 0.0, s: 0.8, l: 0.55, a: 1.0 },
        // purple
        Hsla { h: 0.75, s: 0.7, l: 0.60, a: 1.0 },
        // cyan
        Hsla { h: 0.5, s: 0.7, l: 0.55, a: 1.0 },
        // yellow
        Hsla { h: 0.15, s: 0.9, l: 0.55, a: 1.0 },
        // pink
        Hsla { h: 0.917, s: 0.7, l: 0.60, a: 1.0 },
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
