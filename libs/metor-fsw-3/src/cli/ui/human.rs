//! The styled renderer for a person at a terminal.
//!
//! With [`Caps::plain`] it is also the renderer for pipes and logs.

use std::io::Write;

use super::Reporter;
use super::caps::Caps;
use super::event::{Event, LogLine, Phase, PhaseState};
use super::logline;
use super::theme::{Theme, Token};

/// The column phase subjects start at, after the margin, glyph, and verb.
const VERB_WIDTH: usize = 11;

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
    /// Whether the last line written is a live phase line that the next
    /// phase line replaces.
    live_line: bool,
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
            live_line: false,
            source_width: 0,
        }
    }

    fn phase(&mut self, phase: &Phase<'_>) -> std::io::Result<()> {
        let glyphs = self.theme.glyphs;
        let (glyph, token, verb) = match phase.state {
            PhaseState::Started | PhaseState::Progress { .. } if !self.caps.live => {
                return Ok(());
            }
            PhaseState::Started | PhaseState::Progress { .. } => {
                (glyphs.spinner[0], Token::Accent, phase.verb.doing())
            }
            PhaseState::Done(_) => (glyphs.ok, Token::Ok, phase.verb.done()),
            PhaseState::Failed => (glyphs.fail, Token::Error, phase.verb.doing()),
        };
        self.clear_live()?;
        let glyph_style = self.theme.style(token);
        let muted = self.theme.style(Token::Muted);
        write!(
            self.buf,
            "  {glyph_style}{glyph}{glyph_style:#} {verb:<VERB_WIDTH$} {}",
            phase.subject
        )?;
        match phase.state {
            PhaseState::Progress { done, current } => {
                write!(
                    self.buf,
                    "  {muted}{done} {} {current}{muted:#}",
                    glyphs.dot
                )?;
            }
            PhaseState::Done(took) if took.as_millis() >= 10 => {
                write!(self.buf, "  {muted}{:.1}s{muted:#}", took.as_secs_f64())?;
            }
            _ => {}
        }
        self.live_line = matches!(
            phase.state,
            PhaseState::Started | PhaseState::Progress { .. }
        );
        if !self.live_line {
            writeln!(self.buf)?;
        }
        Ok(())
    }

    fn running(&mut self, namespaces: &[&str]) -> std::io::Result<()> {
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

    fn diagnostic(&mut self, diagnostic: &dyn miette::Diagnostic) -> std::io::Result<()> {
        self.clear_live()?;
        let error = self.theme.bold(self.theme.style(Token::Error));
        let muted = self.theme.style(Token::Muted);
        write!(self.buf, "  {error}{} error", self.theme.glyphs.fail)?;
        if let Some(code) = diagnostic.code() {
            write!(self.buf, "[{code}]")?;
        }
        writeln!(self.buf, "{error:#}  {diagnostic}")?;
        if let Some(help) = diagnostic.help() {
            writeln!(self.buf, "    {muted}help:{muted:#} {help}")?;
        }
        Ok(())
    }

    fn log(&mut self, line: &LogLine<'_>) -> std::io::Result<()> {
        self.clear_live()?;
        self.source_width = self.source_width.max(logline::source_len(&line.source));
        logline::write(&mut self.buf, line, &self.theme, self.source_width)
    }

    /// Ends a live phase line so the next write starts on a clean line.
    fn clear_live(&mut self) -> std::io::Result<()> {
        if std::mem::take(&mut self.live_line) {
            write!(self.buf, "\r\x1b[2K")?;
        }
        Ok(())
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
