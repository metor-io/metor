//! Colours and glyphs.
//!
//! Renderers style text through a [`Theme`] by [`Token`], never by colour.
//! Each token has a designed swatch per background and colour depth.

use anstyle::{Ansi256Color, AnsiColor, Color, RgbColor, Style};

use super::caps::{Background, Caps, Depth};

/// A role a piece of text plays.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Token {
    /// Primary content, in the terminal's own foreground.
    Text,
    /// Labels, field names, timing.
    Muted,
    /// Rules and anything that should recede.
    Faint,
    /// The one highlight.
    Accent,
    Ok,
    Warn,
    Error,
}

/// One colour at each depth.
#[derive(Clone, Copy, Debug)]
struct Swatch {
    rgb: [u8; 3],
    ansi256: u8,
    ansi16: AnsiColor,
}

const fn swatch(rgb: [u8; 3], ansi256: u8, ansi16: AnsiColor) -> Swatch {
    Swatch {
        rgb,
        ansi256,
        ansi16,
    }
}

/// The swatches for one background: muted, faint, accent, ok, warn, error,
/// then the target hues.
struct Palette {
    tokens: [Swatch; 6],
    hues: [Swatch; HUES],
}

/// The number of distinct target hues.
pub const HUES: usize = 8;

const DARK: Palette = Palette {
    tokens: [
        swatch([0x8a, 0x8f, 0x98], 245, AnsiColor::BrightBlack),
        swatch([0x4e, 0x53, 0x5c], 239, AnsiColor::BrightBlack),
        swatch([0xb4, 0x8e, 0xff], 141, AnsiColor::Magenta),
        swatch([0x7f, 0xd8, 0x8f], 114, AnsiColor::Green),
        swatch([0xf5, 0xc2, 0x6b], 221, AnsiColor::Yellow),
        swatch([0xff, 0x6b, 0x6b], 203, AnsiColor::Red),
    ],
    hues: [
        swatch([0x56, 0xb4, 0xe9], 75, AnsiColor::Blue),
        swatch([0xe6, 0x9f, 0x00], 214, AnsiColor::Yellow),
        swatch([0x3c, 0xc4, 0x9a], 43, AnsiColor::Green),
        swatch([0xcc, 0x79, 0xa7], 175, AnsiColor::Magenta),
        swatch([0x6f, 0xd6, 0xe0], 80, AnsiColor::Cyan),
        swatch([0xf0, 0x7a, 0x4a], 209, AnsiColor::Red),
        swatch([0x9a, 0xa8, 0xff], 111, AnsiColor::BrightBlue),
        swatch([0xd8, 0xd2, 0x5e], 185, AnsiColor::BrightYellow),
    ],
};

const LIGHT: Palette = Palette {
    tokens: [
        swatch([0x6b, 0x70, 0x78], 243, AnsiColor::BrightBlack),
        swatch([0xb8, 0xbc, 0xc4], 250, AnsiColor::BrightBlack),
        swatch([0x6e, 0x40, 0xc9], 98, AnsiColor::Magenta),
        swatch([0x1a, 0x7f, 0x37], 28, AnsiColor::Green),
        swatch([0x9a, 0x67, 0x00], 136, AnsiColor::Yellow),
        swatch([0xc9, 0x25, 0x2c], 160, AnsiColor::Red),
    ],
    hues: [
        swatch([0x00, 0x72, 0xb2], 25, AnsiColor::Blue),
        swatch([0xb0, 0x6a, 0x00], 130, AnsiColor::Yellow),
        swatch([0x00, 0x87, 0x5f], 29, AnsiColor::Green),
        swatch([0xa8, 0x3c, 0x7c], 132, AnsiColor::Magenta),
        swatch([0x00, 0x7f, 0x8c], 30, AnsiColor::Cyan),
        swatch([0xc0, 0x4a, 0x10], 166, AnsiColor::Red),
        swatch([0x4a, 0x5c, 0xd0], 62, AnsiColor::BrightBlue),
        swatch([0x7a, 0x70, 0x00], 100, AnsiColor::BrightYellow),
    ],
};

/// The glyphs the renderers draw with.
#[derive(Debug, PartialEq, Eq)]
pub struct Glyphs {
    pub ok: &'static str,
    pub fail: &'static str,
    pub warn: &'static str,
    pub running: &'static str,
    pub stopped: &'static str,
    pub spinner: &'static [&'static str],
    pub ellipsis: &'static str,
    pub from: &'static str,
    pub link: &'static str,
    pub dot: &'static str,
    pub again: &'static str,
}

pub static UNICODE: Glyphs = Glyphs {
    ok: "✓",
    fail: "✗",
    warn: "▲",
    running: "●",
    stopped: "■",
    spinner: &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"],
    ellipsis: "…",
    from: "←",
    link: "⇄",
    dot: "·",
    again: "↻",
};

pub static ASCII: Glyphs = Glyphs {
    ok: "+",
    fail: "x",
    warn: "!",
    running: "*",
    stopped: "#",
    spinner: &["-", "\\", "|", "/"],
    ellipsis: "~",
    from: "<-",
    link: "<>",
    dot: "-",
    again: "~",
};

/// The styles and glyphs for one terminal.
#[derive(Clone, Copy, Debug)]
pub struct Theme {
    tokens: [Style; 6],
    hues: [Style; HUES],
    effects: bool,
    pub glyphs: &'static Glyphs,
}

impl Theme {
    pub fn new(caps: &Caps) -> Self {
        let palette = match caps.background {
            Background::Dark => &DARK,
            Background::Light => &LIGHT,
        };
        let style = |swatch: &Swatch| paint(swatch, caps.depth);
        Self {
            tokens: palette.tokens.each_ref().map(style),
            hues: palette.hues.each_ref().map(style),
            effects: caps.depth != Depth::None,
            glyphs: if caps.unicode { &UNICODE } else { &ASCII },
        }
    }

    /// The style for `token`.
    pub fn style(&self, token: Token) -> Style {
        match token {
            Token::Text => Style::new(),
            Token::Muted => self.tokens[0],
            Token::Faint => self.tokens[1],
            Token::Accent => self.tokens[2],
            Token::Ok => self.tokens[3],
            Token::Warn => self.tokens[4],
            Token::Error => self.tokens[5],
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

fn paint(swatch: &Swatch, depth: Depth) -> Style {
    let [r, g, b] = swatch.rgb;
    let color = match depth {
        Depth::None => return Style::new(),
        Depth::Ansi16 => Color::Ansi(swatch.ansi16),
        Depth::Ansi256 => Color::Ansi256(Ansi256Color(swatch.ansi256)),
        Depth::TrueColor => Color::Rgb(RgbColor(r, g, b)),
    };
    Style::new().fg_color(Some(color))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKENS: [Token; 7] = [
        Token::Text,
        Token::Muted,
        Token::Faint,
        Token::Accent,
        Token::Ok,
        Token::Warn,
        Token::Error,
    ];

    fn theme(depth: Depth, background: Background, unicode: bool) -> Theme {
        Theme::new(&Caps {
            depth,
            background,
            unicode,
            ..Caps::plain()
        })
    }

    fn styles(theme: &Theme) -> impl Iterator<Item = Style> + '_ {
        TOKENS
            .iter()
            .map(|token| theme.style(*token))
            .chain((0..HUES).map(|i| theme.hue(i)))
    }

    #[test]
    fn test_each_depth_stays_in_its_colour_space() {
        for background in [Background::Dark, Background::Light] {
            for depth in [Depth::None, Depth::Ansi16, Depth::Ansi256, Depth::TrueColor] {
                for style in styles(&theme(depth, background, true)) {
                    let fits = matches!(
                        (style.get_fg_color(), depth),
                        (None, _)
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
    fn test_no_depth_has_no_styles() {
        let theme = theme(Depth::None, Background::Dark, true);
        assert!(styles(&theme).all(|style| style == Style::new()));
    }

    #[test]
    fn test_text_keeps_the_terminal_foreground() {
        let theme = theme(Depth::TrueColor, Background::Light, true);
        assert_eq!(theme.style(Token::Text), Style::new());
    }

    #[test]
    fn test_hues_are_distinct_and_wrap() {
        let theme = theme(Depth::TrueColor, Background::Dark, true);
        for i in 0..HUES {
            for j in i + 1..HUES {
                assert_ne!(theme.hue(i), theme.hue(j));
            }
        }
        assert_eq!(theme.hue(HUES + 2), theme.hue(2));
    }

    #[test]
    fn test_ascii_glyphs_are_ascii() {
        let glyphs = theme(Depth::None, Background::Dark, false).glyphs;
        let all = [
            glyphs.ok,
            glyphs.fail,
            glyphs.warn,
            glyphs.running,
            glyphs.stopped,
            glyphs.ellipsis,
            glyphs.from,
            glyphs.link,
            glyphs.dot,
            glyphs.again,
        ];
        assert!(
            all.iter()
                .chain(glyphs.spinner)
                .all(|glyph| glyph.is_ascii())
        );
    }
}
