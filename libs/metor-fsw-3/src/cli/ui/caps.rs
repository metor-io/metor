//! What the terminal on stderr can show.
//!
//! [`Probe::detect`] reads the process environment once. [`Caps::resolve`]
//! turns a probe and the command-line flags into capabilities, and touches
//! nothing else, so it is tested without a terminal.

use std::io::IsTerminal;

/// The `--color` flag.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum ColorChoice {
    #[default]
    Auto,
    Always,
    Never,
}

/// How many colours the terminal shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Depth {
    None,
    Ansi16,
    Ansi256,
    TrueColor,
}

/// The terminal's background.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Background {
    #[default]
    Dark,
    Light,
}

/// The raw facts about stderr and the environment.
#[derive(Clone, Debug, Default)]
pub struct Probe {
    pub tty: bool,
    /// The depth the environment reports, or `None` if it reports no colour.
    pub level: Option<Depth>,
    /// `NO_COLOR` is set and not empty.
    pub no_color: bool,
    /// `CLICOLOR_FORCE` or `FORCE_COLOR` is set and not `0`.
    pub force: bool,
    /// `COLORTERM` names truecolor.
    pub truecolor_env: bool,
    pub width: Option<u16>,
    pub background: Option<Background>,
}

impl Probe {
    /// Reads stderr and the environment.
    ///
    /// The background is queried from the terminal only on a TTY, then read
    /// from `COLORFGBG`.
    ///
    /// `query` allows the terminal query, which costs up to 100 ms on a
    /// terminal that does not answer.
    pub fn detect(query: bool) -> Self {
        let stream = supports_color::Stream::Stderr;
        let tty = std::io::stderr().is_terminal();
        let env = |name| std::env::var(name).ok().filter(|value| !value.is_empty());
        let forced = |name| env(name).is_some_and(|value| value != "0");
        Self {
            tty,
            level: supports_color::on(stream).map(|level| match level {
                level if level.has_16m => Depth::TrueColor,
                level if level.has_256 => Depth::Ansi256,
                _ => Depth::Ansi16,
            }),
            no_color: env("NO_COLOR").is_some(),
            force: forced("CLICOLOR_FORCE") || forced("FORCE_COLOR"),
            truecolor_env: env("COLORTERM")
                .is_some_and(|value| value == "truecolor" || value == "24bit"),
            width: terminal_size::terminal_size_of(std::io::stderr()).map(|(w, _)| w.0),
            background: (query && tty && foreground())
                .then(query_background)
                .flatten()
                .or_else(|| env("COLORFGBG").and_then(|value| from_colorfgbg(&value))),
        }
    }
}

/// Whether this process owns the terminal, so querying it neither stops a
/// background job nor reads another's input.
fn foreground() -> bool {
    // SAFETY: both calls only read the process's and the terminal's group
    // ids.
    unsafe { libc::tcgetpgrp(libc::STDERR_FILENO) == libc::getpgrp() }
}

fn query_background() -> Option<Background> {
    use terminal_colorsaurus::{QueryOptions, ThemeMode, theme_mode};
    let mut options = QueryOptions::default();
    options.timeout = std::time::Duration::from_millis(100);
    match theme_mode(options).ok()? {
        ThemeMode::Dark => Some(Background::Dark),
        ThemeMode::Light => Some(Background::Light),
    }
}

/// Reads the background from `COLORFGBG`, which is `fg;bg` in ANSI colour
/// numbers.
pub fn from_colorfgbg(value: &str) -> Option<Background> {
    let bg: u8 = value.rsplit(';').next()?.parse().ok()?;
    Some(match bg {
        7 | 9..=15 => Background::Light,
        _ => Background::Dark,
    })
}

/// What the renderers may use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caps {
    /// Whether lines may be redrawn in place.
    pub live: bool,
    pub depth: Depth,
    pub width: u16,
    pub background: Background,
}

impl Caps {
    /// The width used when the terminal does not report one.
    pub const DEFAULT_WIDTH: u16 = 100;

    /// Applies the flags to `probe`.
    ///
    /// `--color` wins over the environment, `NO_COLOR` wins over a TTY, and
    /// `CLICOLOR_FORCE` wins over a pipe.
    pub fn resolve(probe: &Probe, color: ColorChoice) -> Self {
        let colored = match color {
            ColorChoice::Always => true,
            ColorChoice::Never => false,
            ColorChoice::Auto => !probe.no_color && (probe.tty || probe.force),
        };
        let depth = match (colored, probe.level) {
            (false, _) => Depth::None,
            (true, Some(level)) => level,
            (true, None) if probe.truecolor_env => Depth::TrueColor,
            (true, None) => Depth::Ansi256,
        };
        Self {
            live: probe.tty,
            depth,
            width: probe.width.unwrap_or(Self::DEFAULT_WIDTH),
            background: probe.background.unwrap_or_default(),
        }
    }

    /// Plain output: no colour and no redraws.
    pub fn plain() -> Self {
        Self {
            live: false,
            depth: Depth::None,
            width: Self::DEFAULT_WIDTH,
            background: Background::Dark,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tty() -> Probe {
        Probe {
            tty: true,
            level: Some(Depth::TrueColor),
            width: Some(80),
            ..Probe::default()
        }
    }

    #[test]
    fn test_tty_gets_its_reported_depth() {
        let caps = Caps::resolve(&tty(), ColorChoice::Auto);
        assert_eq!(caps.depth, Depth::TrueColor);
        assert!(caps.live);
        assert_eq!(caps.width, 80);
    }

    #[test]
    fn test_no_color_wins_over_a_tty() {
        let probe = Probe {
            no_color: true,
            ..tty()
        };
        assert_eq!(Caps::resolve(&probe, ColorChoice::Auto).depth, Depth::None);
    }

    #[test]
    fn test_always_wins_over_no_color_and_a_pipe() {
        let probe = Probe {
            no_color: true,
            ..Probe::default()
        };
        let caps = Caps::resolve(&probe, ColorChoice::Always);
        assert_eq!(caps.depth, Depth::Ansi256);
        assert!(!caps.live);
    }

    #[test]
    fn test_force_colors_a_pipe_at_colorterm_depth() {
        let probe = Probe {
            force: true,
            truecolor_env: true,
            ..Probe::default()
        };
        assert_eq!(
            Caps::resolve(&probe, ColorChoice::Auto).depth,
            Depth::TrueColor
        );
    }

    #[test]
    fn test_pipe_is_plain() {
        let caps = Caps::resolve(&Probe::default(), ColorChoice::Auto);
        assert_eq!(caps.depth, Depth::None);
        assert!(!caps.live);
        assert_eq!(caps.width, Caps::DEFAULT_WIDTH);
    }

    #[test]
    fn test_never() {
        let caps = Caps::resolve(&tty(), ColorChoice::Never);
        assert_eq!(caps.depth, Depth::None);
    }

    #[test]
    fn test_colorfgbg() {
        assert_eq!(from_colorfgbg("15;0"), Some(Background::Dark));
        assert_eq!(from_colorfgbg("0;15"), Some(Background::Light));
        assert_eq!(from_colorfgbg("0;default;7"), Some(Background::Light));
        assert_eq!(from_colorfgbg("garbage"), None);
        assert_eq!(from_colorfgbg(""), None);
    }
}
