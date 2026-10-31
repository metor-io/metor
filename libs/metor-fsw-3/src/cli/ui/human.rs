//! The styled renderer for a person at a terminal.
//!
//! With [`Caps::plain`] it is also the renderer for pipes and logs.

use std::io::Write;

use super::Reporter;
use super::caps::Caps;
use super::event::{Event, Phase, PhaseState};
use super::theme::{Theme, Token};

/// The column phase subjects start at, after the margin, glyph, and verb.
const VERB_WIDTH: usize = 11;

/// Writes events as styled lines to `out`.
pub struct Human<W> {
    out: W,
    theme: Theme,
    caps: Caps,
    /// Whether the last line written is a live phase line that the next
    /// phase line replaces.
    live_line: bool,
}

impl<W: Write> Human<W> {
    pub fn new(out: W, caps: Caps) -> Self {
        Self {
            out,
            theme: Theme::new(&caps),
            caps,
            live_line: false,
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
            self.out,
            "  {glyph_style}{glyph}{glyph_style:#} {verb:<VERB_WIDTH$} {}",
            phase.subject
        )?;
        match phase.state {
            PhaseState::Progress { done, current } => {
                write!(
                    self.out,
                    "  {muted}{done} {} {current}{muted:#}",
                    glyphs.dot
                )?;
            }
            PhaseState::Done(took) if took.as_millis() >= 10 => {
                write!(self.out, "  {muted}{:.1}s{muted:#}", took.as_secs_f64())?;
            }
            _ => {}
        }
        self.live_line = matches!(
            phase.state,
            PhaseState::Started | PhaseState::Progress { .. }
        );
        if !self.live_line {
            writeln!(self.out)?;
        }
        Ok(())
    }

    fn running(&mut self, namespaces: &[&str]) -> std::io::Result<()> {
        self.clear_live()?;
        let accent = self.theme.style(Token::Accent);
        let muted = self.theme.style(Token::Muted);
        write!(
            self.out,
            "  {accent}{}{accent:#} {:<VERB_WIDTH$}",
            self.theme.glyphs.running, "Running"
        )?;
        for (i, namespace) in namespaces.iter().enumerate() {
            let hue = self.theme.bold(self.theme.hue(i));
            let gap = if i == 0 { " " } else { "  " };
            write!(self.out, "{gap}{hue}{namespace}{hue:#}")?;
        }
        if self.caps.live {
            write!(self.out, "    {muted}ctrl-c to stop{muted:#}")?;
        }
        writeln!(self.out)
    }

    fn diagnostic(&mut self, diagnostic: &dyn miette::Diagnostic) -> std::io::Result<()> {
        self.clear_live()?;
        let error = self.theme.bold(self.theme.style(Token::Error));
        let muted = self.theme.style(Token::Muted);
        write!(self.out, "  {error}{} error", self.theme.glyphs.fail)?;
        if let Some(code) = diagnostic.code() {
            write!(self.out, "[{code}]")?;
        }
        writeln!(self.out, "{error:#}  {diagnostic}")?;
        if let Some(help) = diagnostic.help() {
            writeln!(self.out, "    {muted}help:{muted:#} {help}")?;
        }
        Ok(())
    }

    /// Ends a live phase line so the next write starts on a clean line.
    fn clear_live(&mut self) -> std::io::Result<()> {
        if std::mem::take(&mut self.live_line) {
            write!(self.out, "\r\x1b[2K")?;
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
        };
        let _ = written.and_then(|()| self.out.flush());
    }
}
