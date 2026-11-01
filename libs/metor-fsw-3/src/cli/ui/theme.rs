//! Colours. Each role has an RGB colour per background, converted down to
//! what the terminal shows.

use anstyle::{Color, RgbColor, Style};
use anstyle_lossy::{palette::VGA, rgb_to_ansi, rgb_to_xterm};

use terminal_colorsaurus::ThemeMode;

use super::caps::{Caps, Depth};

/// The number of distinct target hues.
pub const HUES: usize = 8;

/// One background's colours: muted, faint, accent, ok, warn, error, then the
/// target hues.
type Palette = ([u32; 6], [u32; HUES]);

const DARK: Palette = (
    [0x8a8f98, 0x4e535c, 0xb48eff, 0x7fd88f, 0xf5c26b, 0xff6b6b],
    [
        0x56b4e9, 0xe69f00, 0x3cc49a, 0xcc79a7, 0x6fd6e0, 0xf07a4a, 0x9aa8ff, 0xd8d25e,
    ],
);

const LIGHT: Palette = (
    [0x6b7078, 0xb8bcc4, 0x6e40c9, 0x1a7f37, 0x9a6700, 0xc9252c],
    [
        0x0072b2, 0xb06a00, 0x00875f, 0xa83c7c, 0x007f8c, 0xc04a10, 0x4a5cd0, 0x7a7000,
    ],
);

/// The styles for one terminal. Primary text keeps the terminal's own
/// foreground, so it has no style.
#[derive(Clone, Copy, Debug)]
pub struct Theme {
    /// Labels, field names, timing.
    pub muted: Style,
    /// Rules and anything that should recede.
    pub faint: Style,
    /// The one highlight.
    pub accent: Style,
    pub ok: Style,
    pub warn: Style,
    pub error: Style,
    hues: [Style; HUES],
    effects: bool,
}

impl Theme {
    pub fn new(caps: &Caps) -> Self {
        let (tokens, hues) = match caps.background {
            ThemeMode::Dark => DARK,
            ThemeMode::Light => LIGHT,
        };
        let [muted, faint, accent, ok, warn, error] = tokens.map(|rgb| paint(rgb, caps.depth));
        Self {
            muted,
            faint,
            accent,
            ok,
            warn,
            error,
            hues: hues.map(|rgb| paint(rgb, caps.depth)),
            effects: caps.depth != Depth::None,
        }
    }

    /// `style` in bold, unless the terminal takes no styles.
    pub fn bold(&self, style: Style) -> Style {
        if self.effects { style.bold() } else { style }
    }

    /// The hue for the `index`th target. Hues repeat after [`HUES`].
    pub fn hue(&self, index: usize) -> Style {
        self.hues[index % HUES]
    }
}

/// `rgb` as a foreground at `depth`.
fn paint(rgb: u32, depth: Depth) -> Style {
    let [_, r, g, b] = rgb.to_be_bytes();
    let rgb = RgbColor(r, g, b);
    let color = match depth {
        Depth::None => return Style::new(),
        Depth::Ansi16 => Color::Ansi(rgb_to_ansi(rgb, VGA)),
        Depth::Ansi256 => Color::Ansi256(rgb_to_xterm(rgb)),
        Depth::TrueColor => Color::Rgb(rgb),
    };
    Style::new().fg_color(Some(color))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme(depth: Depth, background: ThemeMode) -> Theme {
        Theme::new(&Caps {
            depth,
            background,
            ..Caps::plain()
        })
    }

    fn styles(theme: &Theme) -> impl Iterator<Item = Style> + '_ {
        let tokens = [
            theme.muted,
            theme.faint,
            theme.accent,
            theme.ok,
            theme.warn,
            theme.error,
        ];
        tokens.into_iter().chain((0..HUES).map(|i| theme.hue(i)))
    }

    #[test]
    fn test_each_depth_stays_in_its_colour_space() {
        for background in [ThemeMode::Dark, ThemeMode::Light] {
            for depth in [Depth::None, Depth::Ansi16, Depth::Ansi256, Depth::TrueColor] {
                for style in styles(&theme(depth, background)) {
                    let fits = matches!(
                        (style.get_fg_color(), depth),
                        (None, Depth::None)
                            | (Some(Color::Ansi(_)), Depth::Ansi16)
                            | (Some(Color::Ansi256(_)), Depth::Ansi256)
                            | (Some(Color::Rgb(_)), Depth::TrueColor)
                    );
                    assert!(fits, "{style:?} at {depth:?}");
                }
            }
        }
    }

    #[test]
    fn test_no_depth_has_no_styles_or_bold() {
        let theme = theme(Depth::None, ThemeMode::Dark);
        assert!(styles(&theme).all(|style| style == Style::new()));
        assert_eq!(theme.bold(theme.accent), Style::new());
    }

    #[test]
    fn test_hues_are_distinct_and_wrap() {
        for depth in [Depth::Ansi256, Depth::TrueColor] {
            let theme = theme(depth, ThemeMode::Dark);
            for i in 0..HUES {
                for j in i + 1..HUES {
                    assert_ne!(theme.hue(i), theme.hue(j), "{depth:?}");
                }
            }
            assert_eq!(theme.hue(HUES + 2), theme.hue(2));
        }
    }
}
