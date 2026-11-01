//! What the terminal on stderr can show.

use std::io::IsTerminal;

use clap::ColorChoice;
use supports_color::Stream;
use terminal_colorsaurus::ThemeMode;

/// How many colours the terminal shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Depth {
    None,
    Ansi16,
    Ansi256,
    TrueColor,
}

/// What the renderers may use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caps {
    /// Whether lines may be redrawn in place.
    pub live: bool,
    pub depth: Depth,
    pub width: u16,
    /// The terminal's background.
    pub background: ThemeMode,
}

impl Caps {
    /// The width used when the terminal does not report one.
    pub const DEFAULT_WIDTH: u16 = 100;

    /// Reads stderr and the environment. `supports-color` honours
    /// `NO_COLOR`, `FORCE_COLOR`, and `CLICOLOR_FORCE`; `--color` wins over
    /// all of them. `query` allows asking the terminal for its background,
    /// which costs up to 100 ms on one that does not answer.
    pub fn detect(color: ColorChoice, query: bool) -> Self {
        let tty = std::io::stderr().is_terminal();
        let width = terminal_size::terminal_size_of(std::io::stderr()).map(|(w, _)| w.0);
        Self {
            live: tty,
            depth: depth(
                color,
                supports_color::on(Stream::Stderr).map(|l| level(l.has_256, l.has_16m)),
            ),
            width: width.unwrap_or(Self::DEFAULT_WIDTH),
            background: background(query && tty && foreground()),
        }
    }

    /// What stdout shows under `color`, with `stderr`'s width and
    /// background.
    pub fn stdout(color: ColorChoice, stderr: Caps) -> Self {
        let tty = std::io::stdout().is_terminal();
        Self {
            live: tty && stderr.live,
            depth: depth(
                color,
                supports_color::on(Stream::Stdout).map(|l| level(l.has_256, l.has_16m)),
            ),
            ..stderr
        }
    }

    /// Plain output: no colour and no redraws.
    pub fn plain() -> Self {
        Self {
            live: false,
            depth: Depth::None,
            width: Self::DEFAULT_WIDTH,
            background: ThemeMode::Dark,
        }
    }
}

/// The depth `color` allows, given what the environment reports.
fn depth(color: ColorChoice, reported: Option<Depth>) -> Depth {
    match color {
        ColorChoice::Never => Depth::None,
        ColorChoice::Auto => reported.unwrap_or(Depth::None),
        ColorChoice::Always => reported.unwrap_or(Depth::Ansi256),
    }
}

fn level(has_256: bool, has_16m: bool) -> Depth {
    match (has_256, has_16m) {
        (_, true) => Depth::TrueColor,
        (true, false) => Depth::Ansi256,
        (false, false) => Depth::Ansi16,
    }
}

/// Whether this process owns the terminal, so querying it neither stops a
/// background job nor reads another's input.
fn foreground() -> bool {
    // SAFETY: both calls only read the process's and the terminal's group
    // ids.
    unsafe { libc::tcgetpgrp(libc::STDERR_FILENO) == libc::getpgrp() }
}

/// The background the terminal reports when `query`, else `COLORFGBG`'s,
/// else dark.
fn background(query: bool) -> ThemeMode {
    let mut options = terminal_colorsaurus::QueryOptions::default();
    options.timeout = std::time::Duration::from_millis(100);
    let asked = query.then(|| terminal_colorsaurus::theme_mode(options).ok());
    let env = || from_colorfgbg(&std::env::var("COLORFGBG").ok()?);
    asked.flatten().or_else(env).unwrap_or(ThemeMode::Dark)
}

/// Reads the background from `COLORFGBG`, which is `fg;bg` in ANSI colour
/// numbers.
fn from_colorfgbg(value: &str) -> Option<ThemeMode> {
    let bg: u8 = value.rsplit(';').next()?.parse().ok()?;
    Some(match bg {
        7 | 9..=15 => ThemeMode::Light,
        _ => ThemeMode::Dark,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_depth_follows_the_flag() {
        let truecolor = Some(Depth::TrueColor);
        assert_eq!(depth(ColorChoice::Auto, truecolor), Depth::TrueColor);
        assert_eq!(depth(ColorChoice::Never, truecolor), Depth::None);
        assert_eq!(depth(ColorChoice::Auto, None), Depth::None, "a pipe");
        assert_eq!(depth(ColorChoice::Always, None), Depth::Ansi256);
    }

    #[test]
    fn test_level() {
        assert_eq!(level(true, true), Depth::TrueColor);
        assert_eq!(level(true, false), Depth::Ansi256);
        assert_eq!(level(false, false), Depth::Ansi16);
    }

    #[test]
    fn test_colorfgbg() {
        assert_eq!(from_colorfgbg("15;0"), Some(ThemeMode::Dark));
        assert_eq!(from_colorfgbg("0;15"), Some(ThemeMode::Light));
        assert_eq!(from_colorfgbg("0;default;7"), Some(ThemeMode::Light));
        assert_eq!(from_colorfgbg("garbage"), None);
        assert_eq!(from_colorfgbg(""), None);
    }
}
