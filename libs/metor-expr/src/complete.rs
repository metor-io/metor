//! What could legally stand at the cursor.
//!
//! The engine leans on the parser's error recovery instead of repairing text:
//! `parse_unchecked` always hands back a tree, with a zero-width identifier
//! synthesized exactly where a name is missing, so half-typed source needs no
//! marker insertion and no second grammar. Context is read the way ty reads
//! it — the token suffix before the cursor says what is being typed, the
//! covering node says where — and the candidate set is collected *broadly*:
//! everything legal at the position, unfiltered. Ranking against what was
//! typed is the host's job, with the host's matcher; [`Completions::prefix`]
//! is the pattern to rank by and [`Completions::replace`] is the range an
//! accepted item replaces.
//!
//! Two facts about the language shape everything here. A dotted component
//! path is *one flat name* — `adcs.omega_b` is a single candidate, not a
//! namespace walk — so the replace range spans the whole dotted chain and a
//! `.` never re-scopes. The one true member access is a frame parameter
//! inside a `@system`, and only there does a dot narrow the offer to that
//! record's fields.

use ruff_python_ast as ast;
use ruff_python_ast::find_node::covering_node;
use ruff_python_ast::token::{Token, TokenKind};
use ruff_text_size::{Ranged, TextRange, TextSize};

use crate::builtins::{Avail, builtins};
use crate::diag::Span;
use crate::{Manifest, Resolver, System};

/// What sort of thing a candidate is, for the host's icons and ranking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompletionKind {
    /// A component path, or a system/stage another declaration can read.
    Component,
    Builtin,
    /// A `def` in the same module.
    Function,
    /// A frame field, offered after `.` on a frame parameter.
    Field,
    /// A parameter in scope inside a body.
    Local,
    Keyword,
}

/// One thing that could stand at the cursor.
#[derive(Clone, Debug)]
pub struct CompletionItem {
    pub label: String,
    /// A type or signature to dim beside the label. Empty when there is
    /// nothing useful to say.
    pub detail: String,
    pub kind: CompletionKind,
    /// What accepting the item writes over [`Completions::replace`]. Differs
    /// from the label for callables, which bring their parentheses.
    pub insert: String,
    /// Where the caret lands inside `insert`, when not at its end — between
    /// the parentheses of a call that still needs arguments.
    pub caret: Option<u32>,
}

/// Everything legal at one cursor position.
#[derive(Debug, Default)]
pub struct Completions {
    /// The byte range of the source an accepted item replaces. Always
    /// contains the cursor.
    pub replace: Span,
    /// What the operator has typed inside that range — the pattern the host
    /// ranks candidates against.
    pub prefix: String,
    pub items: Vec<CompletionItem>,
}

/// Which tier the source is, mirroring [`compile_expr`](crate::compile_expr)
/// vs [`compile_module`](crate::compile_module): a bare expression is one
/// anonymous system's body, a module is declarations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Expression,
    Module,
}

/// Everything that could legally stand at `cursor`, unranked.
///
/// `manifest` is the last successful compile of this source, when there has
/// been one — it is where module-local names (functions, systems, frame
/// parameters) come from. Its spans may trail the text by a keystroke or
/// two, which costs a stale local at worst, never a wrong edit: the replace
/// range is computed from the live token stream alone.
pub fn complete(
    source: &str,
    cursor: u32,
    scope: Scope,
    resolver: &dyn Resolver,
    manifest: Option<&Manifest>,
) -> Completions {
    let mut at = (cursor as usize).min(source.len());
    while at > 0 && !source.is_char_boundary(at) {
        at -= 1;
    }
    let cursor = TextSize::new(at as u32);

    let parsed = ruff_python_parser::parse_unchecked_source(source, ast::PySourceType::Python);
    let tokens: &[Token] = &parsed.tokens()[..];
    let before = tokens.partition_point(|t| t.range().start() < cursor);

    if let Some(last) = before.checked_sub(1).map(|i| &tokens[i]) {
        let touching = last.range().end() >= cursor;
        let suppressed = match last.kind() {
            // Inside literal text, an identifier is not what is being typed.
            TokenKind::Comment
            | TokenKind::String
            | TokenKind::FStringStart
            | TokenKind::FStringMiddle => touching,
            // `1.` is a float with more digits coming, not an access.
            TokenKind::Int | TokenKind::Float | TokenKind::Complex | TokenKind::Ellipsis => {
                touching
            }
            _ => false,
        };
        if suppressed {
            return Completions::default();
        }
    }

    // The dotted chain being typed: `adcs.om` is Name Dot Name, walked back
    // to its head. `chain` indexes the first token of the chain; equal to
    // `before` when the caret sits in open space.
    let name_like = |t: &Token| t.kind() == TokenKind::Name || t.kind().is_keyword();
    let contiguous = |a: &Token, b: &Token| a.range().end() == b.range().start();
    let mut chain = before;
    if let Some(last) = before.checked_sub(1).map(|i| &tokens[i])
        && last.range().end() >= cursor
        && (name_like(last) || last.kind() == TokenKind::Dot)
    {
        chain = before - 1;
        if last.kind() == TokenKind::Dot {
            if !(chain > 0 && name_like(&tokens[chain - 1]) && contiguous(&tokens[chain - 1], last))
            {
                // A dot with nothing to hang from: `.` after `)` or at the
                // start of a line accesses nothing the language can name.
                return Completions::default();
            }
            chain -= 1;
        }
        while chain >= 2
            && tokens[chain - 1].kind() == TokenKind::Dot
            && name_like(&tokens[chain - 2])
            && contiguous(&tokens[chain - 1], &tokens[chain])
            && contiguous(&tokens[chain - 2], &tokens[chain - 1])
        {
            chain -= 2;
        }
    }
    let chain_start = if chain < before {
        tokens[chain].range().start()
    } else {
        cursor
    };

    // Naming something new is the one place a completion can only mislead.
    if chain
        .checked_sub(1)
        .map(|i| matches!(tokens[i].kind(), TokenKind::Def | TokenKind::Class))
        .unwrap_or(false)
    {
        return Completions::default();
    }
    let module = parsed.syntax();
    let covering = covering_node(module.into(), TextRange::empty(cursor));
    for node in covering.ancestors() {
        match node {
            ast::AnyNodeRef::Parameters(_) => return Completions::default(),
            ast::AnyNodeRef::StmtFor(f) if f.target.range().contains_inclusive(cursor) => {
                return Completions::default();
            }
            _ => {}
        }
    }

    let enclosing_def = covering.ancestors().find_map(|node| match node {
        ast::AnyNodeRef::StmtFunctionDef(def) => Some(def),
        _ => None,
    });
    // The AST's ranges end at the last token, so a cursor in a body's
    // trailing whitespace falls outside its own def. The manifest's spans are
    // the fallback: a keystroke stale at worst, and only ever consulted for
    // *which names are in scope*, never for the replace range.
    let holds = |span: &Span| span.start <= cursor.to_u32() && cursor.to_u32() <= span.end;
    let enclosing_system = manifest.and_then(|m| m.systems.iter().find(|s| holds(&s.source)));
    let enclosing_fn = manifest.and_then(|m| m.functions.iter().find(|f| holds(&f.source)));
    let in_def = enclosing_def.is_some() || enclosing_system.is_some() || enclosing_fn.is_some();
    let in_system_body = match scope {
        // A bare expression is an anonymous system's body.
        Scope::Expression => true,
        Scope::Module => {
            enclosing_def.is_some_and(|def| !def.decorator_list.is_empty())
                || enclosing_system.is_some()
        }
    };

    // `param.` on a frame parameter is the one true member access: the offer
    // narrows to that record's fields and the replace range to the segment
    // after the last dot.
    if chain < before {
        let segments = &tokens[chain..before];
        if segments.len() >= 2
            && let Some(system) = enclosing_system
            && let Some(port) = field_port(
                system,
                &source[TextRange::new(segments[0].range().start(), segments[0].range().end())],
            )
        {
            let last_dot = segments.iter().rev().find(|t| t.kind() == TokenKind::Dot);
            let field_start = last_dot.map(|d| d.range().end()).unwrap_or(chain_start);
            let replace = TextRange::new(field_start, cursor);
            // The sample stamp is not a name a body can write.
            let items = port
                .frame
                .fields
                .iter()
                .enumerate()
                .filter(|(i, _)| Some(*i) != port.frame.timestamp)
                .map(|(_, f)| CompletionItem {
                    label: f.name.clone(),
                    detail: f.ty.to_string(),
                    kind: CompletionKind::Field,
                    insert: f.name.clone(),
                    caret: None,
                })
                .collect();
            return Completions {
                replace: replace.into(),
                prefix: source[replace].to_string(),
                items,
            };
        }
    }

    let replace = TextRange::new(chain_start, cursor);
    let mut out = Completions {
        replace: replace.into(),
        prefix: source[replace].to_string(),
        items: Vec::new(),
    };

    // Whether a callable should bring its parentheses, or the text already
    // has them waiting.
    let call_parens = !source[usize::from(cursor)..].trim_start().starts_with('(');
    let callable = |name: &str, params: usize| -> (String, Option<u32>) {
        if !call_parens {
            return (name.to_string(), None);
        }
        let insert = format!("{name}()");
        let caret = (params > 0).then(|| insert.len() as u32 - 1);
        (insert, caret)
    };

    // Components are free names only where a binding can read them: the
    // one-liner tier, and top-level bindings. A body sees its parameters.
    let top_level = scope == Scope::Expression || !in_def;
    if top_level {
        for path in resolver.paths() {
            let detail = resolver
                .component(&path)
                .map(|s| s.ty.to_string())
                .unwrap_or_default();
            out.items.push(CompletionItem {
                label: path.clone(),
                detail,
                kind: CompletionKind::Component,
                insert: path,
                caret: None,
            });
        }
    }

    if let Some(manifest) = manifest
        && scope == Scope::Module
    {
        if !in_def {
            for system in &manifest.systems {
                out.items.push(CompletionItem {
                    label: system.name.clone(),
                    detail: "system".to_string(),
                    kind: CompletionKind::Component,
                    insert: system.name.clone(),
                    caret: None,
                });
            }
            for stage in &manifest.stages {
                out.items.push(CompletionItem {
                    label: stage.name.clone(),
                    detail: "resample".to_string(),
                    kind: CompletionKind::Component,
                    insert: stage.name.clone(),
                    caret: None,
                });
            }
        }
        for f in &manifest.functions {
            let params = f
                .params
                .iter()
                .map(|(name, ty)| format!("{name}: {ty}"))
                .collect::<Vec<_>>()
                .join(", ");
            let (insert, caret) = callable(&f.name, f.params.len());
            out.items.push(CompletionItem {
                label: f.name.clone(),
                detail: format!("({params}) -> {}", f.ret),
                kind: CompletionKind::Function,
                insert,
                caret,
            });
        }
        // The body's own names: ports for a system, parameters for a helper.
        if let Some(system) = enclosing_system {
            for port in &system.inputs {
                if port.param.contains('.') {
                    continue; // A projected port's name *is* a component path.
                }
                out.items.push(CompletionItem {
                    label: port.param.clone(),
                    detail: port.frame.name.clone(),
                    kind: CompletionKind::Local,
                    insert: port.param.clone(),
                    caret: None,
                });
            }
        } else if let Some(sig) = enclosing_fn {
            for (name, ty) in &sig.params {
                out.items.push(CompletionItem {
                    label: name.clone(),
                    detail: ty.to_string(),
                    kind: CompletionKind::Local,
                    insert: name.clone(),
                    caret: None,
                });
            }
        }
    }

    for b in builtins() {
        let offered = match b.avail {
            Avail::Anywhere => true,
            Avail::System => in_system_body,
            Avail::TopLevel => scope == Scope::Module && !in_def,
        };
        if !offered {
            continue;
        }
        let (insert, caret) = callable(b.name, b.params.len());
        out.items.push(CompletionItem {
            label: b.name.to_string(),
            detail: format!("({}) -> {}", b.params.join(", "), b.ret),
            kind: CompletionKind::Builtin,
            insert,
            caret,
        });
    }

    for word in ["True", "False"] {
        out.items.push(CompletionItem {
            label: word.to_string(),
            detail: "bool".to_string(),
            kind: CompletionKind::Keyword,
            insert: word.to_string(),
            caret: None,
        });
    }
    if scope == Scope::Module && statement_start(tokens, chain) {
        let words: &[&str] = if in_def {
            &[
                "if", "elif", "else", "for", "while", "return", "break", "continue", "pass",
            ]
        } else {
            &["def", "class"]
        };
        for word in words {
            out.items.push(CompletionItem {
                label: word.to_string(),
                detail: String::new(),
                kind: CompletionKind::Keyword,
                insert: word.to_string(),
                caret: None,
            });
        }
    }

    out
}

/// The frame port a body addresses by `name`, when it is a record and not a
/// projection.
fn field_port<'a>(system: &'a System, name: &str) -> Option<&'a crate::Port> {
    system
        .inputs
        .iter()
        .find(|port| port.param == name && !port.frame.fields.is_empty() && !name.contains('.'))
}

/// Whether the chain begins a statement: nothing before it on its line but
/// indentation, or a `:` opening a suite.
fn statement_start(tokens: &[Token], chain: usize) -> bool {
    for token in tokens[..chain].iter().rev() {
        match token.kind() {
            TokenKind::Comment | TokenKind::NonLogicalNewline => continue,
            TokenKind::Newline | TokenKind::Indent | TokenKind::Dedent | TokenKind::Colon => {
                return true;
            }
            _ => return false,
        }
    }
    true
}
