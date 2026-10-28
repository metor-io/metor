//! The parser: chumsky over the lexer's tokens.
//!
//! This is the only module that uses chumsky. Its only output is the
//! [`File`] syntax tree and the errors it found. Expressions use chumsky's
//! Pratt parser. A bad statement is skipped to the end of its line, and a
//! bad item to the next top-level keyword, so one compile reports more than
//! one error.

use chumsky::input::{Input as _, ValueInput};
use chumsky::pratt::{infix, left, prefix, right};
use chumsky::prelude::*;

use crate::ast::*;
use crate::diag::{Diagnostic, Span};
use crate::lexer::{Kw, Op, Token};

type E<'t> = extra::Err<Rich<'t, Token, SimpleSpan>>;

/// Parses a token stream into a file.
///
/// `len` is the source length, where the end of input points.
pub fn parse(tokens: &[(Token, Span)], len: usize) -> (Option<File>, Vec<Diagnostic>) {
    let tokens: Vec<(Token, SimpleSpan)> = tokens
        .iter()
        .map(|(token, span)| (token.clone(), SimpleSpan::from(span.range())))
        .collect();
    let input = tokens.as_slice().map(SimpleSpan::from(len..len), |(t, s)| (t, s));
    let (file, errors) = file().parse(input).into_output_errors();
    (file, errors.into_iter().map(diagnostic).collect())
}

/// Turns a chumsky error into a diagnostic.
fn diagnostic(error: Rich<'_, Token, SimpleSpan>) -> Diagnostic {
    let span = sp(*error.span());
    let found = error
        .found()
        .map_or("the end of the file".to_string(), |t| format!("`{t}`"));
    let mut expected: Vec<String> = error.expected().map(pattern).collect();
    expected.sort();
    expected.dedup();
    let mut message = format!("unexpected {found}");
    if !expected.is_empty() {
        message.push_str(&format!(", expected {}", expected.join(" or ")));
    }
    let contexts: Vec<String> = error.contexts().map(|(label, _)| label.to_string()).collect();
    let diag = Diagnostic::new(span, message);
    match contexts.first() {
        Some(context) => diag.with_note(format!("while parsing {context}")),
        None => diag,
    }
}

/// Names one thing the parser expected.
fn pattern(pattern: &chumsky::error::RichPattern<'_, Token>) -> String {
    use chumsky::error::RichPattern;
    match pattern {
        RichPattern::Token(token) => format!("`{}`", **token),
        RichPattern::Label(label) => label.to_string(),
        RichPattern::Identifier(word) => format!("`{word}`"),
        RichPattern::Any => "something".into(),
        RichPattern::SomethingElse => "something else".into(),
        RichPattern::EndOfInput => "the end of the file".into(),
    }
}

fn sp(span: SimpleSpan) -> Span {
    Span::new(span.start, span.end)
}

fn kw<'t, I: ValueInput<'t, Token = Token, Span = SimpleSpan>>(
    kw: Kw,
) -> impl Parser<'t, I, Token, E<'t>> + Clone {
    just(Token::Kw(kw))
}

fn op<'t, I: ValueInput<'t, Token = Token, Span = SimpleSpan>>(
    op: Op,
) -> impl Parser<'t, I, Token, E<'t>> + Clone {
    just(Token::Op(op))
}

fn ident<'t, I: ValueInput<'t, Token = Token, Span = SimpleSpan>>()
-> impl Parser<'t, I, Ident, E<'t>> + Clone {
    select! { Token::Ident(name) => name }
        .map_with(|name, e| Ident {
            name,
            span: sp(e.span()),
        })
        .labelled("a name")
}

fn string<'t, I: ValueInput<'t, Token = Token, Span = SimpleSpan>>()
-> impl Parser<'t, I, String, E<'t>> + Clone {
    select! { Token::Str(text) => text }.labelled("a string")
}

fn newline<'t, I: ValueInput<'t, Token = Token, Span = SimpleSpan>>()
-> impl Parser<'t, I, (), E<'t>> + Clone {
    just(Token::Newline).ignored().labelled("end of line")
}

/// `: NEWLINE INDENT inner+ DEDENT`.
fn block<'t, I, T, P>(inner: P) -> impl Parser<'t, I, Vec<T>, E<'t>> + Clone
where
    I: ValueInput<'t, Token = Token, Span = SimpleSpan>,
    P: Parser<'t, I, T, E<'t>> + Clone,
{
    op(Op::Colon)
        .ignore_then(newline())
        .ignore_then(just(Token::Indent).labelled("an indented block"))
        .ignore_then(inner.repeated().at_least(1).collect())
        .then_ignore(just(Token::Dedent))
}

fn expr<'t, I: ValueInput<'t, Token = Token, Span = SimpleSpan>>()
-> impl Parser<'t, I, Expr, E<'t>> + Clone {
    recursive(|expr| {
        let literal = select! {
            Token::Int(v) => ExprKind::Int(v),
            Token::Float(v) => ExprKind::Float(v),
            Token::Duration(v) => ExprKind::Duration(v),
            Token::Kw(Kw::True) => ExprKind::Bool(true),
            Token::Kw(Kw::False) => ExprKind::Bool(false),
        }
        .labelled("a literal");
        let arg = ident()
            .then_ignore(op(Op::Eq))
            .or_not()
            .then(expr.clone())
            .map(|(name, value)| Arg { name, value });
        let args = arg
            .separated_by(op(Op::Comma))
            .allow_trailing()
            .collect::<Vec<_>>()
            .delimited_by(op(Op::LParen), op(Op::RParen))
            .labelled("call arguments");
        let call = ident()
            .then(args)
            .map(|(callee, args)| ExprKind::Call { callee, args });
        let name = ident().map(|id| ExprKind::Name(id.name));
        let atom = literal
            .or(call)
            .or(name)
            .map_with(|kind, e| Expr {
                kind,
                span: sp(e.span()),
            })
            .or(expr.clone().delimited_by(op(Op::LParen), op(Op::RParen)))
            .recover_with(via_parser(nested_delimiters(
                Token::Op(Op::LParen),
                Token::Op(Op::RParen),
                [(Token::Op(Op::LBracket), Token::Op(Op::RBracket))],
                |span| Expr {
                    kind: ExprKind::Error,
                    span: sp(span),
                },
            )))
            .boxed();

        enum Postfix {
            Field(Ident),
            Index(Expr),
        }
        let postfix = op(Op::Dot)
            .ignore_then(ident())
            .map(Postfix::Field)
            .or(expr
                .clone()
                .delimited_by(op(Op::LBracket), op(Op::RBracket))
                .map(Postfix::Index));
        let place = atom.foldl_with(postfix.repeated(), |base, post, e| {
            let kind = match post {
                Postfix::Field(field) => ExprKind::Field(Box::new(base), field),
                Postfix::Index(index) => ExprKind::Index(Box::new(base), Box::new(index)),
            };
            Expr {
                kind,
                span: sp(e.span()),
            }
        });

        let bin = |o: BinOp| {
            move |l: Expr, _: Token, r: Expr, e: &mut chumsky::input::MapExtra<'t, '_, I, E<'t>>| Expr {
                kind: ExprKind::Binary(o, Box::new(l), Box::new(r)),
                span: sp(e.span()),
            }
        };
        let un = |o: UnOp| {
            move |_: Token, x: Expr, e: &mut chumsky::input::MapExtra<'t, '_, I, E<'t>>| Expr {
                kind: ExprKind::Unary(o, Box::new(x)),
                span: sp(e.span()),
            }
        };
        place
            .pratt((
                infix(right(9), op(Op::Pow), bin(BinOp::Pow)),
                prefix(8, op(Op::Minus), un(UnOp::Neg)),
                infix(left(7), op(Op::Star), bin(BinOp::Mul)),
                infix(left(7), op(Op::Slash), bin(BinOp::Div)),
                infix(left(7), op(Op::Percent), bin(BinOp::Rem)),
                infix(left(6), op(Op::Plus), bin(BinOp::Add)),
                infix(left(6), op(Op::Minus), bin(BinOp::Sub)),
                infix(left(5), op(Op::EqEq), bin(BinOp::Eq)),
                infix(left(5), op(Op::NotEq), bin(BinOp::Ne)),
                infix(left(5), op(Op::Lt), bin(BinOp::Lt)),
                infix(left(5), op(Op::LtEq), bin(BinOp::Le)),
                infix(left(5), op(Op::Gt), bin(BinOp::Gt)),
                infix(left(5), op(Op::GtEq), bin(BinOp::Ge)),
                prefix(4, kw(Kw::Not), un(UnOp::Not)),
                infix(left(3), kw(Kw::And), bin(BinOp::And)),
                infix(left(2), kw(Kw::Or), bin(BinOp::Or)),
            ))
            .labelled("an expression")
            .boxed()
    })
}

fn type_expr<'t, I: ValueInput<'t, Token = Token, Span = SimpleSpan>>()
-> impl Parser<'t, I, TypeExpr, E<'t>> + Clone {
    recursive(|ty| {
        let array = ty
            .then_ignore(op(Op::Semi))
            .then(expr())
            .delimited_by(op(Op::LBracket), op(Op::RBracket))
            .map_with(|(elem, len), e| TypeExpr::Array {
                elem: Box::new(elem),
                len: Box::new(len),
                span: sp(e.span()),
            });
        ident().map(TypeExpr::Named).or(array).labelled("a type")
    })
}

fn stmt<'t, I: ValueInput<'t, Token = Token, Span = SimpleSpan>>()
-> impl Parser<'t, I, Stmt, E<'t>> + Clone {
    recursive(|stmt| {
        let body = block(stmt.clone());
        let simple = simple_stmt().then_ignore(newline());
        let if_ = kw(Kw::If)
            .ignore_then(expr())
            .then(body.clone())
            .then(kw(Kw::Elif).ignore_then(expr()).then(body.clone()).repeated().collect::<Vec<_>>())
            .then(kw(Kw::Else).ignore_then(body.clone()).or_not())
            .map(|((first, elifs), orelse)| {
                let branches = core::iter::once(first).chain(elifs).collect();
                StmtKind::If { branches, orelse }
            });
        let range = just(Token::Ident("range".into()))
            .ignore_then(expr().delimited_by(op(Op::LParen), op(Op::RParen)))
            .labelled("`range(N)`");
        let for_ = kw(Kw::For)
            .ignore_then(ident())
            .then_ignore(kw(Kw::In))
            .then(range)
            .then(body)
            .map(|((var, count), body)| StmtKind::For { var, count, body });
        if_.or(for_)
            .or(simple)
            .map_with(|kind, e| Stmt {
                kind,
                span: sp(e.span()),
            })
            .labelled("a statement")
            .recover_with(skip_until(
                none_of([Token::Newline, Token::Dedent]).ignored(),
                newline(),
                || Stmt {
                    kind: StmtKind::Error,
                    span: Span::default(),
                },
            ))
            .boxed()
    })
}

/// The statements that fit on one line.
fn simple_stmt<'t, I: ValueInput<'t, Token = Token, Span = SimpleSpan>>()
-> impl Parser<'t, I, StmtKind, E<'t>> + Clone {
    let wait_until = kw(Kw::Until)
        .ignore_then(expr())
        .then(kw(Kw::Timeout).ignore_then(expr()).or_not())
        .map(|(cond, timeout)| StmtKind::WaitUntil { cond, timeout })
        .labelled("`wait until … timeout`");
    let wait = kw(Kw::Wait).ignore_then(wait_until.or(expr().map(StmtKind::Wait)));
    let compound = select! {
        Token::Op(Op::Eq) => None,
        Token::Op(Op::PlusEq) => Some(BinOp::Add),
        Token::Op(Op::MinusEq) => Some(BinOp::Sub),
        Token::Op(Op::StarEq) => Some(BinOp::Mul),
        Token::Op(Op::SlashEq) => Some(BinOp::Div),
    };
    let declare = ident()
        .then_ignore(op(Op::Colon))
        .then(type_expr())
        .then_ignore(op(Op::Eq))
        .then(expr())
        .map(|((name, ty), value)| StmtKind::Declare { name, ty, value });
    let assign = expr()
        .then(compound)
        .then(expr())
        .map(|((target, op), value)| StmtKind::Assign { target, op, value });
    choice((
        kw(Kw::Pass).to(StmtKind::Pass),
        kw(Kw::Return).ignore_then(expr()).map(StmtKind::Return),
        kw(Kw::Fail).ignore_then(string()).map(StmtKind::Fail),
        wait,
        declare,
        assign,
    ))
}

fn member<'t, I: ValueInput<'t, Token = Token, Span = SimpleSpan>>()
-> impl Parser<'t, I, Member, E<'t>> + Clone {
    let bound = ident()
        .then_ignore(op(Op::Dot))
        .then(ident())
        .map(|(program, port)| PortSource::Bound { program, port });
    let source = op(Op::Colon)
        .ignore_then(type_expr())
        .map(PortSource::Typed)
        .or(op(Op::Eq).ignore_then(bound));
    let input = kw(Kw::In)
        .ignore_then(ident())
        .then(source)
        .then_ignore(newline())
        .map_with(|(name, source), e| Member::In {
            name,
            source,
            span: sp(e.span()),
        })
        .labelled("an `in` port");
    let output = kw(Kw::Out)
        .ignore_then(ident())
        .then_ignore(op(Op::Colon))
        .then(type_expr())
        .then(kw(Kw::Keep).or_not().map(|k| k.is_some()))
        .then_ignore(newline())
        .map_with(|((name, ty), keep), e| Member::Out {
            name,
            ty,
            keep,
            span: sp(e.span()),
        })
        .labelled("an `out` port");
    let alert = kw(Kw::Alert)
        .ignore_then(ident())
        .then(ident().labelled("a severity"))
        .then_ignore(op(Op::Colon))
        .then(expr())
        .then(kw(Kw::For).ignore_then(expr()).or_not())
        .then_ignore(newline())
        .map_with(|(((name, severity), cond), hold), e| Member::Alert {
            name,
            severity,
            cond,
            hold,
            span: sp(e.span()),
        })
        .labelled("an alert");
    let redline = kw(Kw::Redline)
        .ignore_then(string())
        .then_ignore(op(Op::Colon))
        .then(expr())
        .then_ignore(newline())
        .map_with(|(name, cond), e| Member::Redline {
            name,
            cond,
            span: sp(e.span()),
        })
        .labelled("a redline");
    let on_abort = kw(Kw::On)
        .ignore_then(kw(Kw::Abort))
        .ignore_then(block(stmt()))
        .map_with(|body, e| Member::OnAbort {
            body,
            span: sp(e.span()),
        })
        .labelled("an `on abort:` block");
    let loop_ = kw(Kw::Loop)
        .ignore_then(block(stmt()))
        .map_with(|body, e| Member::Loop {
            body,
            span: sp(e.span()),
        })
        .labelled("a `loop:` block");
    let doc = string().then_ignore(newline()).map(Member::Doc);
    choice((input, output, alert, redline, on_abort, loop_, doc, stmt().map(Member::Stmt))).boxed()
}

fn params<'t, I: ValueInput<'t, Token = Token, Span = SimpleSpan>>()
-> impl Parser<'t, I, Vec<ParamDecl>, E<'t>> + Clone {
    ident()
        .then(op(Op::Colon).ignore_then(type_expr()).or_not())
        .then(op(Op::Eq).ignore_then(expr()).or_not())
        .map(|((name, ty), default)| ParamDecl { name, ty, default })
        .separated_by(op(Op::Comma))
        .allow_trailing()
        .collect()
        .delimited_by(op(Op::LParen), op(Op::RParen))
        .labelled("params")
}

/// An indented block of any tokens, for an item this compiler skips.
fn skipped_block<'t, I: ValueInput<'t, Token = Token, Span = SimpleSpan>>()
-> impl Parser<'t, I, (), E<'t>> + Clone {
    recursive(|nested| {
        let token = none_of([Token::Indent, Token::Dedent]).ignored();
        just(Token::Indent)
            .ignore_then(nested.or(token).repeated())
            .then_ignore(just(Token::Dedent))
    })
}

fn item<'t, I: ValueInput<'t, Token = Token, Span = SimpleSpan>>()
-> impl Parser<'t, I, Item, E<'t>> + Clone {
    let use_ = kw(Kw::Use)
        .ignore_then(kw(Kw::Frames))
        .ignore_then(kw(Kw::From))
        .ignore_then(string())
        .then_ignore(newline())
        .map_with(|pack, e| Item::Use {
            pack,
            span: sp(e.span()),
        });
    let field = ident()
        .then_ignore(op(Op::Colon))
        .then(type_expr())
        .then_ignore(newline())
        .map(|(name, ty)| FieldDecl { name, ty })
        .labelled("a frame field");
    let frame = kw(Kw::Frame)
        .ignore_then(ident())
        .then(block(field))
        .map_with(|(name, fields), e| {
            Item::Frame(FrameDecl {
                name,
                fields,
                span: sp(e.span()),
            })
        });
    let const_ = kw(Kw::Const)
        .ignore_then(ident())
        .then(op(Op::Colon).ignore_then(type_expr()).or_not())
        .then_ignore(op(Op::Eq))
        .then(expr())
        .then_ignore(newline())
        .map_with(|((name, ty), value), e| {
            Item::Const(ConstDecl {
                name,
                ty,
                value,
                span: sp(e.span()),
            })
        });
    let def_param = ident().then_ignore(op(Op::Colon)).then(type_expr());
    let def = kw(Kw::Def)
        .ignore_then(ident())
        .then(
            def_param
                .separated_by(op(Op::Comma))
                .allow_trailing()
                .collect()
                .delimited_by(op(Op::LParen), op(Op::RParen)),
        )
        .then_ignore(op(Op::Arrow))
        .then(type_expr())
        .then(block(stmt()))
        .map_with(|(((name, params), ret), body), e| {
            Item::Def(DefDecl {
                name,
                params,
                ret,
                body,
                span: sp(e.span()),
            })
        });
    let kind = kw(Kw::System)
        .to(ProgramKind::System)
        .or(kw(Kw::Sequence).to(ProgramKind::Sequence));
    let program = kind
        .then(ident())
        .then(params().or_not())
        .then(block(member()))
        .map_with(|(((kind, name), params), members), e| {
            Item::Program(ProgramDecl {
                kind,
                name,
                params: params.unwrap_or_default(),
                members,
                span: sp(e.span()),
            })
        });
    let machine = kw(Kw::Machine)
        .ignore_then(ident())
        .then_ignore(op(Op::Colon))
        .then_ignore(newline())
        .then_ignore(skipped_block())
        .map_with(|name, e| Item::Machine {
            name,
            span: sp(e.span()),
        });
    let top = [Kw::Use, Kw::Frame, Kw::Const, Kw::Def, Kw::System, Kw::Sequence, Kw::Machine]
        .map(Token::Kw);
    let skip = any()
        .ignore_then(none_of(top).repeated())
        .to(Item::Error);
    choice((use_, frame, const_, def, program, machine))
        .recover_with(via_parser(skip))
        .boxed()
}

fn file<'t, I: ValueInput<'t, Token = Token, Span = SimpleSpan>>()
-> impl Parser<'t, I, File, E<'t>> {
    item()
        .repeated()
        .collect()
        .then_ignore(end())
        .map(|items| File { items })
}

#[cfg(test)]
mod tests;
