//! The styled renderer for a person at a terminal.
//!
//! With [`Caps::plain`] it is also the renderer for pipes and logs.

use core::fmt::Write as _;
use core::time::Duration;
use std::io::{self, Write};
use std::time::Instant;

use super::Reporter;
use super::caps::Caps;
use super::event::{Event, LogLine, Phase, PhaseState};
use super::logline;
use super::theme::{Theme, Token};

/// The column phase subjects start at, after the margin, glyph, and verb.
const VERB_WIDTH: usize = 11;

/// The width phase subjects pad to, so durations align.
const SUBJECT_WIDTH: usize = 24;

/// How long a phase runs before its live line appears.
const REVEAL_AFTER: Duration = Duration::from_millis(200);

/// Synchronized output: the terminal shows what is between these at once.
const SYNC_BEGIN: &str = "\x1b[?2026h";
const SYNC_END: &str = "\x1b[?2026l";
const CLEAR_LINE: &str = "\x1b[2K";

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
    /// The phase in progress, redrawn in place on a live terminal.
    live: Option<Live>,
    /// How long a phase runs before its live line appears.
    reveal_after: Duration,
    /// The widest log source so far. Sources pad to it.
    source_width: usize,
}

/// A phase line that redraws in place until its phase ends.
struct Live {
    verb: &'static str,
    subject: String,
    detail: String,
    since: Instant,
    frame: usize,
    /// Whether the line is on screen now.
    shown: bool,
}

impl<W: Write> Human<W> {
    pub fn new(out: W, caps: Caps) -> Self {
        Self {
            out,
            buf: Vec::with_capacity(512),
            theme: Theme::new(&caps),
            caps,
            live: None,
            reveal_after: REVEAL_AFTER,
            source_width: 0,
        }
    }

    /// Shows live lines as soon as their phase starts.
    #[cfg(test)]
    pub(crate) fn reveal_at_once(mut self) -> Self {
        self.reveal_after = Duration::ZERO;
        self
    }

    fn phase(&mut self, phase: &Phase<'_>) -> io::Result<()> {
        let glyphs = self.theme.glyphs;
        match phase.state {
            PhaseState::Started => {
                self.clear_live()?;
                self.live = Some(Live {
                    verb: phase.verb.doing(),
                    subject: phase.subject.to_string(),
                    detail: String::new(),
                    since: Instant::now(),
                    frame: 0,
                    shown: false,
                });
                self.draw_live()
            }
            PhaseState::Progress { done, current } => {
                if let Some(live) = &mut self.live {
                    live.detail.clear();
                    let _ = write!(live.detail, "{done} {} {current}", glyphs.dot);
                }
                self.draw_live()
            }
            PhaseState::Done(took) => {
                self.end_live()?;
                let took = (took.as_millis() >= 10).then_some(took);
                self.phase_line(glyphs.ok, Token::Ok, phase.verb.done(), phase.subject, took)
            }
            PhaseState::Failed => {
                self.end_live()?;
                self.phase_line(
                    glyphs.fail,
                    Token::Error,
                    phase.verb.doing(),
                    phase.subject,
                    None,
                )
            }
        }
    }

    /// Writes a finished phase's line.
    fn phase_line(
        &mut self,
        glyph: &str,
        token: Token,
        verb: &str,
        subject: &str,
        took: Option<Duration>,
    ) -> io::Result<()> {
        let style = self.theme.style(token);
        let muted = self.theme.style(Token::Muted);
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

    /// Advances the spinner and redraws the live line.
    fn tick(&mut self) -> io::Result<()> {
        if let Some(live) = &mut self.live {
            live.frame += 1;
        }
        self.draw_live()
    }

    /// Draws the live line in place, once its phase has run long enough.
    fn draw_live(&mut self) -> io::Result<()> {
        let Some(live) = &mut self.live else {
            return Ok(());
        };
        if !self.caps.live || live.since.elapsed() < self.reveal_after {
            return Ok(());
        }
        let glyphs = self.theme.glyphs;
        let frame = glyphs.spinner[live.frame % glyphs.spinner.len()];
        let (accent, muted) = (
            self.theme.style(Token::Accent),
            self.theme.style(Token::Muted),
        );
        let used = 2 + 2 + VERB_WIDTH + 1 + live.subject.chars().count() + 2;
        let room = usize::from(self.caps.width).saturating_sub(used + 1);
        let detail = truncate(&live.detail, room);
        write!(
            self.buf,
            "{SYNC_BEGIN}\r{CLEAR_LINE}  {accent}{frame}{accent:#} {:<VERB_WIDTH$} {}",
            live.verb, live.subject
        )?;
        if !detail.is_empty() {
            write!(self.buf, "  {muted}{detail}{muted:#}")?;
        }
        write!(self.buf, "{SYNC_END}")?;
        live.shown = true;
        Ok(())
    }

    /// Takes the live line off the screen; the next tick draws it again.
    fn clear_live(&mut self) -> io::Result<()> {
        if let Some(live) = &mut self.live
            && std::mem::take(&mut live.shown)
        {
            write!(self.buf, "\r{CLEAR_LINE}")?;
        }
        Ok(())
    }

    fn end_live(&mut self) -> io::Result<()> {
        self.clear_live()?;
        self.live = None;
        Ok(())
    }

    fn running(&mut self, namespaces: &[&str]) -> io::Result<()> {
        self.clear_live()?;
        let accent = self.theme.style(Token::Accent);
        let muted = self.theme.style(Token::Muted);
        write!(
            self.buf,
            "  {accent}{}{accent:#} {:<VERB_WIDTH$}",
            self.theme.glyphs.running, "Running"
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

    fn diagnostic(&mut self, diagnostic: &dyn miette::Diagnostic) -> io::Result<()> {
        self.clear_live()?;
        let error = self.theme.bold(self.theme.style(Token::Error));
        let muted = self.theme.style(Token::Muted);
        write!(self.buf, "  {error}{} error", self.theme.glyphs.fail)?;
        if let Some(code) = diagnostic.code() {
            write!(self.buf, "[{code}]")?;
        }
        writeln!(self.buf, "{error:#}  {diagnostic}")?;
        self.snippet(diagnostic)?;
        if let Some(help) = diagnostic.help() {
            writeln!(self.buf, "    {muted}help:{muted:#} {help}")?;
        }
        Ok(())
    }

    /// Writes the first labelled line of `diagnostic`'s source, rustc style.
    fn snippet(&mut self, diagnostic: &dyn miette::Diagnostic) -> io::Result<()> {
        let (Some(source), Some(label)) = (
            diagnostic.source_code(),
            diagnostic.labels().and_then(|mut labels| labels.next()),
        ) else {
            return Ok(());
        };
        let Ok(contents) = source.read_span(label.inner(), 0, 0) else {
            return Ok(());
        };
        let line = contents.line() + 1;
        let text = String::from_utf8_lossy(contents.data());
        let gutter = line.to_string().len();
        let (faint, accent) = (
            self.theme.style(Token::Faint),
            self.theme.style(Token::Error),
        );
        let muted = self.theme.style(Token::Muted);
        let name = contents.name().unwrap_or("");
        writeln!(self.buf, "    {muted}-->{muted:#} {name}:{line}")?;
        writeln!(self.buf, "    {faint}{:gutter$} │{faint:#}", "")?;
        writeln!(self.buf, "    {faint}{line} │{faint:#} {text}")?;
        let marks = "^".repeat(text.chars().count().max(1));
        write!(
            self.buf,
            "    {faint}{:gutter$} │{faint:#} {accent}{marks}",
            ""
        )?;
        if let Some(label) = label.label() {
            write!(self.buf, " {label}")?;
        }
        writeln!(self.buf, "{accent:#}")?;
        writeln!(self.buf)
    }

    fn log(&mut self, line: &LogLine<'_>) -> io::Result<()> {
        self.clear_live()?;
        self.source_width = self.source_width.max(logline::source_len(&line.source));
        logline::write(&mut self.buf, line, &self.theme, self.source_width)
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
            Event::Output(bytes) => self.clear_live().and_then(|()| self.buf.write_all(bytes)),
            Event::Tick => self.tick(),
        };
        if written.is_ok() && !self.buf.is_empty() {
            let _ = self
                .out
                .write_all(&self.buf)
                .and_then(|()| self.out.flush());
        }
        self.buf.clear();
    }
}

/// The first `room` characters of `text`.
fn truncate(text: &str, room: usize) -> &str {
    text.char_indices()
        .nth(room)
        .map_or(text, |(end, _)| &text[..end])
}
