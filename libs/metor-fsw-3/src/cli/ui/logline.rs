//! One log line: time, level, source, message, fields.

use core::fmt;
use core::time::Duration;
use std::io::{self, Write};

use anstyle::Style;
use metor_proto_wkt::LogLevel;

use super::event::{LogLine, Source};
use super::glyph;
use super::theme::Theme;

/// The width of the time column, `+00:00.000`.
const TIME_WIDTH: usize = 10;

/// Writes `line` to `out`, padding its source to `source_width`.
pub fn write(
    out: &mut impl Write,
    line: &LogLine<'_>,
    theme: &Theme,
    source_width: usize,
) -> io::Result<()> {
    let muted = theme.muted;
    match line.elapsed {
        Some(elapsed) => write!(out, "  {muted}{}{muted:#}", Elapsed(elapsed))?,
        None => write!(out, "  {:TIME_WIDTH$}", "")?,
    }
    let (glyph, style) = level(line.level, theme);
    write!(out, "  {style}{glyph:<1}{style:#}  ")?;
    write_source(out, &line.source, theme)?;
    let pad = source_width.saturating_sub(source_len(&line.source));
    write!(out, "{:pad$}  {style}{}{style:#}", "", line.message)?;
    for (key, value) in line.fields {
        write!(out, "  {muted}{key}={muted:#}")?;
        if value.is_empty() || value.contains(char::is_whitespace) {
            write!(out, "{value:?}")?;
        } else {
            write!(out, "{value}")?;
        }
    }
    writeln!(out)
}

/// The visible width of `source`.
pub fn source_len(source: &Source<'_>) -> usize {
    let target = source
        .target
        .map_or(0, |(name, _)| name.chars().count() + 1);
    target + source.system.chars().count()
}

fn write_source(out: &mut impl Write, source: &Source<'_>, theme: &Theme) -> io::Result<()> {
    if let Some((name, hue)) = source.target {
        let hue = theme.hue(hue);
        write!(out, "{hue}{name}{hue:#}.")?;
    }
    write!(out, "{}", source.system)
}

/// The glyph for `level`, and the style of it and the message.
fn level(level: LogLevel, theme: &Theme) -> (&'static str, Style) {
    match level {
        LogLevel::Error => (glyph::FAIL, theme.error),
        LogLevel::Warn => (glyph::WARN, theme.warn),
        LogLevel::Info => (" ", Style::new()),
        LogLevel::Debug => (glyph::DOT, theme.muted),
        LogLevel::Trace => (glyph::DOT, theme.faint),
    }
}

/// Formats as `+MM:SS.mmm`, [`TIME_WIDTH`] wide, or `+H:MM:SS.mmm` from an
/// hour on.
struct Elapsed(Duration);

impl fmt::Display for Elapsed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let millis = self.0.as_millis();
        let (hours, rest) = (millis / 3_600_000, millis % 3_600_000);
        let (minutes, rest) = (rest / 60_000, rest % 60_000);
        let (seconds, millis) = (rest / 1000, rest % 1000);
        if hours > 0 {
            write!(f, "+{hours}:{minutes:02}:{seconds:02}.{millis:03}")
        } else {
            write!(f, "+{minutes:02}:{seconds:02}.{millis:03}")
        }
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::*;
    use crate::cli::ui::caps::{Caps, Depth};

    fn line<'a>(
        level: LogLevel,
        target: Option<(&'a str, usize)>,
        system: &'a str,
        message: &'a str,
        fields: &'a [(Cow<'static, str>, Cow<'static, str>)],
    ) -> LogLine<'a> {
        LogLine {
            elapsed: Some(Duration::from_millis(1500)),
            level,
            source: Source { target, system },
            message,
            fields,
        }
    }

    fn render(caps: Caps) -> String {
        let theme = Theme::new(&caps);
        let fields = [
            (Cow::Borrowed("axis"), Cow::Borrowed("z")),
            (Cow::Borrowed("note"), Cow::Borrowed("two words")),
            (Cow::Borrowed("empty"), Cow::Borrowed("")),
        ];
        let lines = [
            line(
                LogLevel::Info,
                Some(("plant", 0)),
                "pub",
                "listening on [::]:2240",
                &[],
            ),
            line(
                LogLevel::Warn,
                Some(("fsw", 1)),
                "ctrl",
                "saturated",
                &fields,
            ),
            line(
                LogLevel::Error,
                Some(("fsw", 1)),
                "commission",
                "latched off",
                &[],
            ),
            line(LogLevel::Debug, None, "nav", "converged", &[]),
            LogLine {
                elapsed: None,
                ..line(LogLevel::Info, None, "metor", "outside the run", &[])
            },
        ];
        let width = lines
            .iter()
            .map(|l| source_len(&l.source))
            .max()
            .unwrap_or(0);
        let mut out = Vec::new();
        for line in &lines {
            write(&mut out, line, &theme, width).expect("writes to a Vec");
        }
        String::from_utf8(out).expect("utf-8")
    }

    #[test]
    fn test_plain_lines() {
        insta::assert_snapshot!(render(Caps::plain()));
    }

    #[test]
    fn test_styled_lines() {
        let caps = Caps {
            depth: Depth::TrueColor,
            ..Caps::plain()
        };
        insta::assert_snapshot!(render(caps));
    }

    #[test]
    fn test_elapsed() {
        let at = |ms| Elapsed(Duration::from_millis(ms)).to_string();
        assert_eq!(at(8), "+00:00.008");
        assert_eq!(at(61_500), "+01:01.500");
        assert_eq!(at(3_723_004), "+1:02:03.004");
    }

    #[test]
    fn test_source_len_counts_the_dot() {
        let source = Source {
            target: Some(("fsw", 0)),
            system: "ctrl",
        };
        assert_eq!(source_len(&source), "fsw.ctrl".len());
    }
}
