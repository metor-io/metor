//! metor-code: a small total language for flight software systems and
//! sequences, compiled with Cranelift.

pub mod aot;
pub mod ast;
pub mod check;
pub mod cli;
pub mod codegen;
pub mod diag;
pub mod hir;
pub mod jit;
pub mod lexer;
pub mod packs;
pub mod parser;
pub mod runtime;
pub mod types;

use check::Imports;
use diag::{Diagnostic, Diagnostics};

/// An error compiling a file.
#[derive(Debug, thiserror::Error)]
pub enum CompileError {
    #[error("{0}")]
    Source(Diagnostics),
    #[error(transparent)]
    Codegen(#[from] codegen::CodegenError),
    #[error(transparent)]
    Program(#[from] mc_rt::ProgramError),
}

impl CompileError {
    /// Renders the error, pointing into `src` when it has a span.
    pub fn render(&self, file: &str, src: &str) -> String {
        match self {
            CompileError::Source(diags) => diags.render(file, src),
            other => format!("{other}\n"),
        }
    }

    /// The source errors, if this is one.
    pub fn diagnostics(&self) -> Option<&Diagnostics> {
        match self {
            CompileError::Source(diags) => Some(diags),
            _ => None,
        }
    }
}

/// Lexes, parses and checks `src`.
pub fn front(src: &str, imports: &dyn Imports) -> Result<hir::Program, CompileError> {
    let (tokens, mut errors) = lexer::lex(src);
    let (file, parse_errors) = parser::parse(&tokens, src.len());
    errors.extend(parse_errors);
    let file = match (file, errors.is_empty()) {
        (Some(file), true) => file,
        _ => return Err(source_error(errors)),
    };
    check::check(&file, src, imports).map_err(source_error)
}

fn source_error(mut errors: Vec<Diagnostic>) -> CompileError {
    errors.sort_by_key(|d| d.span.start);
    if errors.is_empty() {
        errors.push(Diagnostic::new(diag::Span::default(), "the file did not parse"));
    }
    CompileError::Source(Diagnostics(errors))
}
