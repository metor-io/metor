//! The styled renderer for a person at a terminal. A log line is time,
//! level, source, message, then fields.
//!
//! With [`Caps::plain`] it is also the renderer for pipes and logs.

use core::fmt;
use core::time::Duration;
use std::io::{self, Write};

use anstyle::Style;
use indicatif::{ProgressBar, ProgressStyle};
use metor_proto_wkt::LogLevel;
use miette::{GraphicalReportHandler, GraphicalTheme};

use super::Reporter;
use super::caps::{Caps, Depth};
use super::event::{Event, LogLine, Phase, PhaseState, Source, Summary};
use super::glyph;
use super::table::{self, Align, group};
use super::theme::Theme;
use super::width;

/// The column phase subjects start at, after the margin, glyph, and verb.
const VERB_WIDTH: usize = 11;

/// The width phase subjects pad to, so durations align.
const SUBJECT_WIDTH: usize = 24;

/// The summary table's columns.
const SUMMARY_COLUMNS: [(&str, Align); 4] = [
    ("TARGET", Align::Left),
    ("CYCLES", Align::Right),
    ("MAX CYCLE", Align::Right),
    ("OVERRUNS", Align::Right),
];

/// How often the spinner turns.
const TICK: Duration = Duration::from_millis(100);

/// The widest a source column is set up front. A longer source still
/// widens it.
const MAX_SOURCE_WIDTH: usize = 28;

/// Writes events as styled lines to `out`.
///
/// Each event renders into a reused buffer and reaches `out` in one write,
/// so other writers to the same stream never split a line.
pub struct Human<W> {
    out: W,
    buf: Vec<u8>,
    theme: Theme,
    caps: Caps,
    /// The phase in progress, spinning on stderr while it runs.
    spinner: Option<ProgressBar>,
    /// The widest log source so far. Sources pad to it.
    source_width: usize,
}

impl<W: Write> Human<W> {
    pub fn new(out: W, caps: Caps) -> Self {
        Self {
            out,
            buf: Vec::with_capacity(512),
            theme: Theme::new(&caps),
            caps,
            spinner: None,
            source_width: 0,
        }
    }

    fn phase(&mut self, phase: &Phase<'_>) -> io::Result<()> {
        match phase.state {
            PhaseState::Started => {
                self.stop_spinner();
                self.spinner = self.caps.live.then(|| self.spin(phase));
                Ok(())
            }
            PhaseState::Progress { done, current } => {
                let muted = self.theme.muted;
                if let Some(spinner) = &self.spinner {
                    let detail = format!("{muted}{done} {} {current}{muted:#}", glyph::DOT);
                    spinner.set_message(format!("{}  {detail}", phase.subject));
                }
                Ok(())
            }
            PhaseState::Done(took) => {
                self.stop_spinner();
                let took = (took.as_millis() >= 10).then_some(took);
                self.phase_line(
                    glyph::OK,
                    self.theme.ok,
                    phase.verb.done,
                    phase.subject,
                    took,
                )
            }
            PhaseState::Failed => {
                self.stop_spinner();
                let verb = phase.verb.doing;
                self.phase_line(glyph::FAIL, self.theme.error, verb, phase.subject, None)
            }
        }
    }

    /// A spinner on stderr for `phase`.
    fn spin(&self, phase: &Phase<'_>) -> ProgressBar {
        let accent = self.theme.accent;
        let frames: Vec<String> = glyph::SPINNER
            .iter()
            .chain([&" "])
            .map(|frame| format!("{accent}{frame}{accent:#}"))
            .collect();
        let frames: Vec<&str> = frames.iter().map(String::as_str).collect();
        let style = ProgressStyle::with_template("  {spinner} {prefix:11} {wide_msg}")
            .unwrap_or_else(|_| ProgressStyle::default_spinner())
            .tick_strings(&frames);
        let spinner = ProgressBar::new_spinner()
            .with_style(style)
            .with_prefix(phase.verb.doing)
            .with_message(phase.subject.to_string());
        spinner.enable_steady_tick(TICK);
        spinner
    }

    fn stop_spinner(&mut self) {
        if let Some(spinner) = self.spinner.take() {
            spinner.finish_and_clear();
        }
    }

    /// Writes a finished phase's line.
    fn phase_line(
        &mut self,
        glyph: &str,
        style: Style,
        verb: &str,
        subject: &str,
        took: Option<Duration>,
    ) -> io::Result<()> {
        let muted = self.theme.muted;
        write!(self.buf, "  {style}{glyph}{style:#} {verb:<VERB_WIDTH$} ")?;
        let Some(took) = took else {
            return writeln!(self.buf, "{subject}");
        };
        write!(self.buf, "{subject:<SUBJECT_WIDTH$}  {muted}")?;
        match took.as_millis() {
            ms @ 0..1000 => write!(self.buf, "{ms:>4}ms")?,
            _ => write!(self.buf, "{:>5.1}s", took.as_secs_f64())?,
        }
        writeln!(self.buf, "{muted:#}")
    }

    fn running(&mut self, namespaces: &[&str]) -> io::Result<()> {
        let (accent, muted) = (self.theme.accent, self.theme.muted);
        write!(
            self.buf,
            "  {accent}{}{accent:#} {:<VERB_WIDTH$}",
            glyph::RUNNING,
            "Running"
        )?;
        for (i, namespace) in namespaces.iter().enumerate() {
            let hue = self.theme.bold(self.theme.hue(i));
            let gap = if i == 0 { " " } else { "  " };
            write!(self.buf, "{gap}{hue}{namespace}{hue:#}")?;
        }
        if self.caps.live {
            write!(self.buf, "    {muted}ctrl-c to stop{muted:#}")?;
        }
        writeln!(self.buf)
    }

    /// Renders `diagnostic` with miette. On a terminal its code links to the
    /// code's text; elsewhere the link is written out.
    fn diagnostic(&mut self, diagnostic: &dyn miette::Diagnostic) -> io::Result<()> {
        let theme = match self.caps.depth {
            Depth::None => GraphicalTheme::unicode_nocolor(),
            _ => GraphicalTheme::unicode(),
        };
        let handler = GraphicalReportHandler::new_themed(theme)
            .with_width(usize::from(self.caps.width))
            .with_links(self.caps.live);
        let mut text = String::new();
        handler
            .render_report(&mut text, diagnostic)
            .map_err(io::Error::other)?;
        self.buf.write_all(text.as_bytes())
    }

    fn ended(&mut self, summary: &Summary) -> io::Result<()> {
        self.stop_spinner();
        let accent = self.theme.accent;
        let how = if summary.interrupted {
            "Stopped"
        } else {
            "Finished"
        };
        writeln!(
            self.buf,
            "\n  {accent}{}{accent:#} {how} after {:.1} s",
            glyph::STOPPED,
            summary.took.as_secs_f64()
        )?;
        if summary.targets.is_empty() {
            return Ok(());
        }
        let rows = summary.targets.iter().map(|target| {
            let overruns = match target.overruns {
                0 => String::new(),
                n => group(n),
            };
            let max = format!("{:.2} ms", target.max_cycle.as_secs_f64() * 1e3);
            vec![target.name.clone(), group(target.cycles), max, overruns]
        });
        writeln!(self.buf)?;
        table::write(
            &mut self.buf,
            &SUMMARY_COLUMNS,
            rows,
            &self.theme,
            None,
            "    ",
        )
    }

    fn log(&mut self, line: &LogLine<'_>) -> io::Result<()> {
        self.source_width = self.source_width.max(source_len(&line.source));
        write_log(&mut self.buf, line, &self.theme, self.source_width)
    }

    /// Writes the buffer to `out`, with the spinner, if any, off the screen.
    fn flush(&mut self) {
        let Self {
            out, buf, spinner, ..
        } = self;
        let mut write = || out.write_all(buf).and_then(|()| out.flush());
        let _ = match spinner {
            Some(spinner) => spinner.suspend(write),
            None => write(),
        };
        buf.clear();
    }
}

impl<W: Write + Send> Reporter for Human<W> {
    fn event(&mut self, event: &Event<'_>) {
        let written = match event {
            Event::Phase(phase) => self.phase(phase),
            Event::Running(namespaces) => self.running(namespaces),
            Event::Diagnostic(diagnostic) => self.diagnostic(*diagnostic),
            Event::Log(line) => self.log(line),
            Event::SourceWidth(width) => {
                self.source_width = (*width).min(MAX_SOURCE_WIDTH);
                Ok(())
            }
            Event::Output(bytes) => self.buf.write_all(bytes),
            Event::Ended(summary) => self.ended(summary),
            Event::Changed(path) => {
                let accent = self.theme.accent;
                writeln!(
                    self.buf,
                    "\n  {accent}{}{accent:#} {:<VERB_WIDTH$} {path}\n",
                    glyph::AGAIN,
                    "Changed"
                )
            }
            Event::Data(_, value) => writeln!(self.buf, "{value:#}"),
        };
        match written {
            Ok(()) if !self.buf.is_empty() => self.flush(),
            _ => self.buf.clear(),
        }
    }
}

/// The width of the time column, `+00:00.000`.
const TIME_WIDTH: usize = 10;

/// Writes `line` to `out`, padding its source to `source_width`.
fn write_log(
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
fn source_len(source: &Source<'_>) -> usize {
    let target = source.target.map_or(0, |(name, _)| width(name) + 1);
    target + width(source.system)
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
            write_log(&mut out, line, &theme, width).expect("writes to a Vec");
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
