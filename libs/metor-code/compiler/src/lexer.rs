//! The hand-written lexer.
//!
//! It turns source text into tokens with byte spans. Indentation becomes
//! `Indent` and `Dedent` tokens, the end of each logical line a `Newline`,
//! and a number followed by a time unit (`500 ms`, `5 min`) one `Duration`
//! literal in seconds. Newlines inside brackets continue the line.

use core::fmt;

use crate::diag::{Diagnostic, Span};

/// One token.
#[derive(Clone, Debug, PartialEq)]
pub enum Token {
    Ident(String),
    Int(u64),
    Float(f64),
    /// A literal with a time unit, in seconds.
    Duration(f64),
    Str(String),
    Kw(Kw),
    Op(Op),
    Newline,
    Indent,
    Dedent,
}

// Floats in tokens are only compared by the parser's `just`, which never
// matches on a literal's value.
impl Eq for Token {}

impl core::hash::Hash for Token {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        core::mem::discriminant(self).hash(state);
    }
}

macro_rules! keywords {
    ($($variant:ident = $text:literal,)*) => {
        /// A reserved word.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum Kw { $($variant,)* }

        impl Kw {
            fn from_word(word: &str) -> Option<Kw> {
                match word { $($text => Some(Kw::$variant),)* _ => None }
            }

            pub fn text(self) -> &'static str {
                match self { $(Kw::$variant => $text,)* }
            }
        }
    };
}

keywords! {
    Use = "use",
    Frames = "frames",
    From = "from",
    Frame = "frame",
    System = "system",
    Sequence = "sequence",
    Machine = "machine",
    Def = "def",
    Const = "const",
    In = "in",
    Out = "out",
    Keep = "keep",
    Loop = "loop",
    If = "if",
    Elif = "elif",
    Else = "else",
    For = "for",
    Return = "return",
    Wait = "wait",
    Until = "until",
    Timeout = "timeout",
    Fail = "fail",
    Redline = "redline",
    On = "on",
    Abort = "abort",
    Alert = "alert",
    And = "and",
    Or = "or",
    Not = "not",
    True = "True",
    False = "False",
    Pass = "pass",
}

macro_rules! operators {
    ($($variant:ident = $text:literal,)*) => {
        /// A punctuation token.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum Op { $($variant,)* }

        impl Op {
            /// Every operator, longest spelling first, so lexing is greedy.
            const ALL: &[(&str, Op)] = &[$(($text, Op::$variant),)*];

            pub fn text(self) -> &'static str {
                match self { $(Op::$variant => $text,)* }
            }
        }
    };
}

operators! {
    Pow = "**",
    Arrow = "->",
    EqEq = "==",
    NotEq = "!=",
    LtEq = "<=",
    GtEq = ">=",
    PlusEq = "+=",
    MinusEq = "-=",
    StarEq = "*=",
    SlashEq = "/=",
    LParen = "(",
    RParen = ")",
    LBracket = "[",
    RBracket = "]",
    Comma = ",",
    Colon = ":",
    Semi = ";",
    Dot = ".",
    Eq = "=",
    Lt = "<",
    Gt = ">",
    Plus = "+",
    Minus = "-",
    Star = "*",
    Slash = "/",
    Percent = "%",
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Token::Ident(name) => write!(f, "{name}"),
            Token::Int(value) => write!(f, "{value}"),
            Token::Float(value) => write!(f, "{value:?}"),
            Token::Duration(secs) => write!(f, "{secs} s"),
            Token::Str(text) => write!(f, "{text:?}"),
            Token::Kw(kw) => write!(f, "{}", kw.text()),
            Token::Op(op) => write!(f, "{}", op.text()),
            Token::Newline => write!(f, "end of line"),
            Token::Indent => write!(f, "indent"),
            Token::Dedent => write!(f, "dedent"),
        }
    }
}

/// Seconds per time unit.
const UNITS: &[(&str, f64)] = &[
    ("us", 1e-6),
    ("ms", 1e-3),
    ("s", 1.0),
    ("sec", 1.0),
    ("min", 60.0),
    ("h", 3600.0),
];

/// Lexes `src` into tokens.
///
/// Every lexing error is collected; the tokens around a bad character are
/// still returned so parsing can report more.
pub fn lex(src: &str) -> (Vec<(Token, Span)>, Vec<Diagnostic>) {
    let mut lexer = Lexer {
        src,
        pos: 0,
        tokens: Vec::new(),
        errors: Vec::new(),
        indents: vec![0],
        depth: 0,
        line_start: true,
    };
    lexer.run();
    (lexer.tokens, lexer.errors)
}

struct Lexer<'a> {
    src: &'a str,
    pos: usize,
    tokens: Vec<(Token, Span)>,
    errors: Vec<Diagnostic>,
    /// The indentation of each open block.
    indents: Vec<usize>,
    /// How many brackets are open.
    depth: usize,
    /// Whether the next token starts a logical line.
    line_start: bool,
}

impl Lexer<'_> {
    fn run(&mut self) {
        while self.pos < self.src.len() {
            if self.line_start && self.depth == 0 {
                self.indentation();
                continue;
            }
            self.token();
        }
        self.finish();
    }

    fn peek(&self) -> Option<char> {
        self.src[self.pos..].chars().next()
    }

    fn peek_at(&self, ahead: usize) -> Option<char> {
        self.src[self.pos..].chars().nth(ahead)
    }

    fn push(&mut self, token: Token, start: usize) {
        self.tokens.push((token, Span::new(start, self.pos)));
    }

    fn error(&mut self, start: usize, end: usize, message: impl Into<String>) {
        self.errors.push(Diagnostic::new(Span::new(start, end), message));
    }

    /// Measures a line's indentation and emits the blocks it opens or
    /// closes. Blank and comment-only lines are skipped.
    fn indentation(&mut self) {
        let start = self.pos;
        let mut width = 0;
        while let Some(c) = self.peek() {
            match c {
                ' ' => width += 1,
                '\t' => {
                    self.error(self.pos, self.pos + 1, "indent with spaces, not tabs");
                    width += 4;
                }
                _ => break,
            }
            self.pos += 1;
        }
        match self.peek() {
            None => return,
            Some('\n') | Some('\r') => {
                self.pos += 1;
                return;
            }
            Some('#') => {
                self.skip_comment();
                return;
            }
            Some(_) => {}
        }
        self.line_start = false;
        self.set_indent(width, start);
    }

    /// Emits `Indent` or `Dedent`s to move from the open block to `width`.
    fn set_indent(&mut self, width: usize, start: usize) {
        let current = self.indents.last().copied().unwrap_or(0);
        if width > current {
            self.indents.push(width);
            self.tokens.push((Token::Indent, Span::new(start, self.pos)));
            return;
        }
        while width < self.indents.last().copied().unwrap_or(0) {
            self.indents.pop();
            self.tokens.push((Token::Dedent, Span::new(self.pos, self.pos)));
        }
        if width != self.indents.last().copied().unwrap_or(0) {
            self.error(start, self.pos, "this line's indentation matches no open block");
        }
    }

    fn skip_comment(&mut self) {
        while let Some(c) = self.peek() {
            if c == '\n' {
                break;
            }
            self.pos += c.len_utf8();
        }
    }

    fn token(&mut self) {
        let start = self.pos;
        let Some(c) = self.peek() else { return };
        match c {
            ' ' | '\t' | '\r' => self.pos += 1,
            '\\' if self.peek_at(1) == Some('\n') => self.pos += 2,
            '\n' => {
                self.pos += 1;
                if self.depth == 0 {
                    self.end_line(start);
                }
            }
            '#' => self.skip_comment(),
            '"' => self.string(),
            c if c.is_ascii_digit() => self.number(),
            c if c == '_' || c.is_alphabetic() => self.word(),
            _ => self.operator(start),
        }
    }

    /// Ends a logical line, once.
    fn end_line(&mut self, start: usize) {
        let ends_line = !matches!(
            self.tokens.last(),
            None | Some((Token::Newline | Token::Indent | Token::Dedent, _))
        );
        if ends_line {
            self.tokens.push((Token::Newline, Span::new(start, start + 1)));
        }
        self.line_start = true;
    }

    fn word(&mut self) {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c != '_' && !c.is_alphanumeric() {
                break;
            }
            self.pos += c.len_utf8();
        }
        let word = &self.src[start..self.pos];
        let token = match Kw::from_word(word) {
            Some(kw) => Token::Kw(kw),
            None => Token::Ident(word.to_string()),
        };
        self.push(token, start);
    }

    fn number(&mut self) {
        let start = self.pos;
        let mut float = false;
        while let Some(c) = self.peek() {
            let exponent_sign = matches!(c, '+' | '-')
                && matches!(self.src[..self.pos].chars().last(), Some('e' | 'E'));
            match c {
                '0'..='9' | '_' => {}
                '.' if !float && self.peek_at(1).is_some_and(|d| d.is_ascii_digit()) => float = true,
                'e' | 'E' => float = true,
                _ if exponent_sign => {}
                _ => break,
            }
            self.pos += 1;
        }
        let text: String = self.src[start..self.pos].chars().filter(|c| *c != '_').collect();
        let value = match float {
            true => text.parse::<f64>().ok(),
            false => text.parse::<u64>().ok().map(|v| v as f64),
        };
        let Some(value) = value else {
            self.error(start, self.pos, format!("`{text}` is not a number"));
            return;
        };
        if let Some(scale) = self.unit() {
            self.push(Token::Duration(value * scale), start);
            return;
        }
        let token = match float {
            true => Token::Float(value),
            false => Token::Int(text.parse().unwrap_or(0)),
        };
        self.push(token, start);
    }

    /// Consumes a time unit after a number, if one follows on the same line.
    fn unit(&mut self) -> Option<f64> {
        let rest = &self.src[self.pos..];
        let trimmed = rest.trim_start_matches([' ', '\t']);
        let word_len = trimmed
            .find(|c: char| c != '_' && !c.is_alphanumeric())
            .unwrap_or(trimmed.len());
        let word = &trimmed[..word_len];
        let (_, scale) = UNITS.iter().find(|(unit, _)| *unit == word)?;
        self.pos += rest.len() - trimmed.len() + word_len;
        Some(*scale)
    }

    fn string(&mut self) {
        let start = self.pos;
        self.pos += 1;
        let mut text = String::new();
        loop {
            match self.peek() {
                None | Some('\n') => {
                    self.error(start, self.pos, "this string is never closed");
                    return;
                }
                Some('"') => {
                    self.pos += 1;
                    break;
                }
                Some('\\') => {
                    self.pos += 1;
                    let escaped = self.peek().unwrap_or('\\');
                    text.push(match escaped {
                        'n' => '\n',
                        't' => '\t',
                        other => other,
                    });
                    self.pos += escaped.len_utf8();
                }
                Some(c) => {
                    text.push(c);
                    self.pos += c.len_utf8();
                }
            }
        }
        self.push(Token::Str(text), start);
    }

    fn operator(&mut self, start: usize) {
        let rest = &self.src[self.pos..];
        let Some((text, op)) = Op::ALL.iter().find(|(text, _)| rest.starts_with(text)) else {
            let c = self.peek().unwrap_or(' ');
            self.pos += c.len_utf8();
            self.error(start, self.pos, format!("unexpected character `{c}`"));
            return;
        };
        self.pos += text.len();
        match op {
            Op::LParen | Op::LBracket => self.depth += 1,
            Op::RParen | Op::RBracket => self.depth = self.depth.saturating_sub(1),
            _ => {}
        }
        self.push(Token::Op(*op), start);
    }

    /// Ends the last line and closes every open block.
    fn finish(&mut self) {
        let end = self.src.len();
        self.end_line(end);
        while self.indents.len() > 1 {
            self.indents.pop();
            self.tokens.push((Token::Dedent, Span::new(end, end)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<Token> {
        let (tokens, errors) = lex(src);
        assert!(errors.is_empty(), "{errors:?}");
        tokens.into_iter().map(|(t, _)| t).collect()
    }

    fn ident(name: &str) -> Token {
        Token::Ident(name.into())
    }

    #[test]
    fn test_blocks_and_lines() {
        let src = "system a:\n    x = 1\n\n    # note\n    loop:\n        x = 2\ny\n";
        let tokens = kinds(src);
        let expected = vec![
            Token::Kw(Kw::System),
            ident("a"),
            Token::Op(Op::Colon),
            Token::Newline,
            Token::Indent,
            ident("x"),
            Token::Op(Op::Eq),
            Token::Int(1),
            Token::Newline,
            Token::Kw(Kw::Loop),
            Token::Op(Op::Colon),
            Token::Newline,
            Token::Indent,
            ident("x"),
            Token::Op(Op::Eq),
            Token::Int(2),
            Token::Newline,
            Token::Dedent,
            Token::Dedent,
            ident("y"),
            Token::Newline,
        ];
        assert_eq!(tokens, expected);
    }

    #[test]
    fn test_unit_literals() {
        assert_eq!(kinds("wait 500 ms"), [Token::Kw(Kw::Wait), Token::Duration(0.5), Token::Newline]);
        assert_eq!(kinds("5 min")[0], Token::Duration(300.0));
        assert_eq!(kinds("1.5s")[0], Token::Duration(1.5));
        assert_eq!(kinds("2.0")[0], Token::Float(2.0));
        assert_eq!(kinds("1e-3")[0], Token::Float(1e-3));
        assert_eq!(kinds("10_000")[0], Token::Int(10_000));
    }

    #[test]
    fn test_brackets_continue_lines() {
        let tokens = kinds("f(a,\n  b)\n");
        assert!(!tokens[..tokens.len() - 1].contains(&Token::Newline));
        assert_eq!(kinds("a ** b")[1], Token::Op(Op::Pow));
        assert_eq!(kinds("a <= b")[1], Token::Op(Op::LtEq));
    }

    #[test]
    fn test_unclosed_eof_blocks_close() {
        let tokens = kinds("a:\n  b:\n    c");
        let dedents = tokens.iter().filter(|t| **t == Token::Dedent).count();
        assert_eq!(dedents, 2);
        assert_eq!(kinds("\"a\\\"b\"")[0], Token::Str("a\"b".into()));
    }

    #[test]
    fn test_lex_errors() {
        let (_, errors) = lex("a:\n    b\n  c\n");
        assert_eq!(errors.len(), 1);
        let (_, errors) = lex("x = $\n\"open");
        assert_eq!(errors.len(), 2);
        let (_, errors) = lex("a:\n\tb\n");
        assert_eq!(errors.len(), 1);
    }
}
