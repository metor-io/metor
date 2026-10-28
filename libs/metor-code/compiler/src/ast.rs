//! The syntax tree the parser produces. Every node carries its span.

use crate::diag::Span;

#[derive(Clone, Debug, PartialEq)]
pub struct Ident {
    pub name: String,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct File {
    pub items: Vec<Item>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    /// `use frames from "pack"`.
    Use { pack: String, span: Span },
    Frame(FrameDecl),
    Const(ConstDecl),
    Def(DefDecl),
    Program(ProgramDecl),
    /// A `machine`, which this compiler does not implement yet.
    Machine { name: Ident, span: Span },
    /// An item the parser skipped after an error.
    Error,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FrameDecl {
    pub name: Ident,
    pub fields: Vec<FieldDecl>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FieldDecl {
    pub name: Ident,
    pub ty: TypeExpr,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TypeExpr {
    Named(Ident),
    /// `[T; N]`, with `N` a constant expression.
    Array {
        elem: Box<TypeExpr>,
        len: Box<Expr>,
        span: Span,
    },
}

impl TypeExpr {
    pub fn span(&self) -> Span {
        match self {
            TypeExpr::Named(ident) => ident.span,
            TypeExpr::Array { span, .. } => *span,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ConstDecl {
    pub name: Ident,
    pub ty: Option<TypeExpr>,
    pub value: Expr,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DefDecl {
    pub name: Ident,
    pub params: Vec<(Ident, TypeExpr)>,
    pub ret: TypeExpr,
    pub body: Vec<Stmt>,
    pub span: Span,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgramKind {
    System,
    Sequence,
}

/// A `system` or a `sequence`.
#[derive(Clone, Debug, PartialEq)]
pub struct ProgramDecl {
    pub kind: ProgramKind,
    pub name: Ident,
    pub params: Vec<ParamDecl>,
    pub members: Vec<Member>,
    pub span: Span,
}

/// One instance param: `name = default`, `name: T = default` or `name: T`.
#[derive(Clone, Debug, PartialEq)]
pub struct ParamDecl {
    pub name: Ident,
    pub ty: Option<TypeExpr>,
    pub default: Option<Expr>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PortSource {
    /// `in x: T`.
    Typed(TypeExpr),
    /// `in x = prog.port`.
    Bound { program: Ident, port: Ident },
}

/// One line or block at the top level of a program.
#[derive(Clone, Debug, PartialEq)]
pub enum Member {
    Doc(String),
    In {
        name: Ident,
        source: PortSource,
        span: Span,
    },
    Out {
        name: Ident,
        ty: TypeExpr,
        keep: bool,
        span: Span,
    },
    Alert {
        name: Ident,
        severity: Ident,
        cond: Expr,
        hold: Option<Expr>,
        span: Span,
    },
    Redline {
        name: String,
        cond: Expr,
        span: Span,
    },
    OnAbort {
        body: Vec<Stmt>,
        span: Span,
    },
    Loop {
        body: Vec<Stmt>,
        span: Span,
    },
    Stmt(Stmt),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Stmt {
    pub kind: StmtKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum StmtKind {
    /// `place = value`, or `place op= value` when `op` is set.
    Assign {
        target: Expr,
        op: Option<BinOp>,
        value: Expr,
    },
    /// `name: T = value`.
    Declare {
        name: Ident,
        ty: TypeExpr,
        value: Expr,
    },
    If {
        branches: Vec<(Expr, Vec<Stmt>)>,
        orelse: Option<Vec<Stmt>>,
    },
    /// `for var in range(count):`.
    For {
        var: Ident,
        count: Expr,
        body: Vec<Stmt>,
    },
    Return(Expr),
    Pass,
    Wait(Expr),
    WaitUntil {
        cond: Expr,
        timeout: Option<Expr>,
    },
    Fail(String),
    /// A statement the parser skipped after an error.
    Error,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ExprKind {
    Int(u64),
    Float(f64),
    Bool(bool),
    /// A literal with a time unit, in seconds.
    Duration(f64),
    Name(String),
    Field(Box<Expr>, Ident),
    Index(Box<Expr>, Box<Expr>),
    Call { callee: Ident, args: Vec<Arg> },
    Unary(UnOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Error,
}

/// A call argument, positional or `name = value`.
#[derive(Clone, Debug, PartialEq)]
pub struct Arg {
    pub name: Option<Ident>,
    pub value: Expr,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Pow,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

impl BinOp {
    pub fn is_comparison(self) -> bool {
        matches!(
            self,
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge
        )
    }

    pub fn is_logic(self) -> bool {
        matches!(self, BinOp::And | BinOp::Or)
    }
}
