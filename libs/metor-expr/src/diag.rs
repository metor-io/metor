//! What the compiler says when it refuses.
//!
//! [`compile`](crate::compile) has one hard obligation: no input makes it
//! panic. A half-typed line in a panel field is the *normal* case, not an
//! error case, so every rejection — a parse failure, an unknown name, a
//! construct outside the subset — comes back as a [`Diagnostic`] carrying the
//! byte range that provoked it.

use rustpython_parser::text_size::TextRange;

/// A byte range in the source.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub(crate) fn new(start: u32, end: u32) -> Self {
        Span { start, end }
    }
}

impl From<TextRange> for Span {
    fn from(range: TextRange) -> Self {
        Span::new(u32::from(range.start()), u32::from(range.end()))
    }
}

/// One reason the source was refused.
#[derive(Clone, Debug)]
pub struct Diagnostic {
    pub span: Span,
    pub message: String,
}

/// Every reason the source was refused, in source order.
#[derive(Clone, Debug, Default)]
pub struct Diagnostics(Vec<Diagnostic>);

impl Diagnostics {
    pub(crate) fn push(&mut self, span: impl Into<Span>, message: impl Into<String>) {
        self.0.push(Diagnostic {
            span: span.into(),
            message: message.into(),
        });
    }

    /// Whether the source was accepted.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The diagnostics, in source order.
    pub fn iter(&self) -> impl Iterator<Item = &Diagnostic> {
        self.0.iter()
    }

    /// How many reasons there are.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub(crate) fn sorted(mut self) -> Self {
        self.0.sort_by_key(|d| (d.span.start, d.span.end));
        self
    }
}

impl std::fmt::Display for Diagnostics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (i, d) in self.0.iter().enumerate() {
            if i > 0 {
                writeln!(f)?;
            }
            write!(f, "{}..{}: {}", d.span.start, d.span.end, d.message)?;
        }
        Ok(())
    }
}

impl std::error::Error for Diagnostics {}
