//! Source spans and the diagnostics every pass reports, rendered with
//! `ariadne`.

use core::fmt;
use core::ops::Range;

/// A byte range in the source.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub const fn new(start: usize, end: usize) -> Span {
        Span { start, end }
    }

    /// The smallest span covering both.
    pub fn to(self, other: Span) -> Span {
        Span::new(self.start.min(other.start), self.end.max(other.end))
    }

    pub fn range(self) -> Range<usize> {
        self.start..self.end
    }
}

/// One error, with the span it points at and optional notes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub span: Span,
    pub message: String,
    /// A label on the span, if it says more than the message.
    pub label: Option<String>,
    pub notes: Vec<String>,
}

impl Diagnostic {
    pub fn new(span: Span, message: impl Into<String>) -> Diagnostic {
        Diagnostic {
            span,
            message: message.into(),
            label: None,
            notes: Vec::new(),
        }
    }

    pub fn with_label(mut self, label: impl Into<String>) -> Diagnostic {
        self.label = Some(label.into());
        self
    }

    pub fn with_note(mut self, note: impl Into<String>) -> Diagnostic {
        self.notes.push(note.into());
        self
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

/// Every error a compile found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostics(pub Vec<Diagnostic>);

impl Diagnostics {
    /// Renders every error with `ariadne`, for a terminal or a test's output.
    pub fn render(&self, file: &str, src: &str) -> String {
        let mut out = Vec::new();
        for diag in &self.0 {
            let _ = report(diag, file).write((file, ariadne::Source::from(src)), &mut out);
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    /// Whether some error's message contains `text`.
    pub fn mentions(&self, text: &str) -> bool {
        self.0.iter().any(|d| d.message.contains(text))
    }
}

impl fmt::Display for Diagnostics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for diag in &self.0 {
            writeln!(f, "{}..{}: {}", diag.span.start, diag.span.end, diag.message)?;
        }
        Ok(())
    }
}

impl std::error::Error for Diagnostics {}

/// Builds the `ariadne` report of one diagnostic.
fn report<'a>(diag: &Diagnostic, file: &'a str) -> ariadne::Report<'static, (&'a str, Range<usize>)> {
    let span = (file, diag.span.range());
    let label = ariadne::Label::new(span.clone())
        .with_message(diag.label.clone().unwrap_or_else(|| diag.message.clone()));
    let mut builder = ariadne::Report::build(ariadne::ReportKind::Error, span)
        .with_message(&diag.message)
        .with_label(label)
        .with_config(ariadne::Config::default().with_color(false));
    for note in &diag.notes {
        builder = builder.with_note(note);
    }
    builder.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_points_at_span() {
        let src = "system a:\n    x = y\n";
        let diags = Diagnostics(vec![
            Diagnostic::new(Span::new(18, 19), "`y` is not defined").with_note("declare it first"),
        ]);
        let text = diags.render("a.mc", src);
        assert!(text.contains("`y` is not defined"), "{text}");
        assert!(text.contains("a.mc:2"), "{text}");
        assert!(text.contains("declare it first"), "{text}");
        assert!(diags.mentions("not defined"));
    }

    #[test]
    fn test_span_union() {
        assert_eq!(Span::new(4, 6).to(Span::new(1, 5)), Span::new(1, 6));
    }
}
