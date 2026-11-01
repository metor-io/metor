//! The styled renderer for a person at a terminal.
//!
//! With [`Caps::plain`] it is also the renderer for pipes and logs.

use core::time::Duration;
use std::io::{self, Write};

use anstyle::Style;
use indicatif::{ProgressBar, ProgressStyle};
use miette::{GraphicalReportHandler, GraphicalTheme};

use super::Reporter;
use super::caps::{Caps, Depth};
use super::event::{Event, LogLine, Phase, PhaseState, Summary};
use super::glyph;
use super::logline;
use super::table::{Align, Table, group};
use super::theme::Theme;

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
                    phase.verb.done(),
                    phase.subject,
                    took,
                )
            }
            PhaseState::Failed => {
                self.stop_spinner();
                let verb = phase.verb.doing();
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
            .with_prefix(phase.verb.doing())
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

    /// Renders `diagnostic` with miette, ending in where to read more.
    fn diagnostic(&mut self, diagnostic: &dyn miette::Diagnostic) -> io::Result<()> {
        let theme = match self.caps.depth {
            Depth::None => GraphicalTheme::unicode_nocolor(),
            _ => GraphicalTheme::unicode(),
        };
        let mut handler = GraphicalReportHandler::new_themed(theme)
            .with_width(usize::from(self.caps.width))
            .with_links(false);
        if let Some(code) = diagnostic.code() {
            handler = handler.with_footer(format!("more: metor explain {code}"));
        }
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
        let mut table = Table::new(&SUMMARY_COLUMNS);
        for target in &summary.targets {
            let overruns = match target.overruns {
                0 => String::new(),
                n => group(n),
            };
            table.row(vec![
                target.name.clone(),
                group(target.cycles),
                format!("{:.2} ms", target.max_cycle.as_secs_f64() * 1e3),
                overruns,
            ]);
        }
        writeln!(self.buf)?;
        table.write(&mut self.buf, &self.theme, "    ")
    }

    fn log(&mut self, line: &LogLine<'_>) -> io::Result<()> {
        self.source_width = self.source_width.max(logline::source_len(&line.source));
        logline::write(&mut self.buf, line, &self.theme, self.source_width)
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
