//! The checked, typed program the code generator lowers.
//!
//! Every name is resolved to a local, a block offset, a constant or a
//! function. Every memory access is a [`Place`] with an offset into one of
//! the blocks an `McCx` points to, and sequences carry their resume points.

use crate::types::{ConstVal, FrameTy, Prim};

/// A whole checked file.
#[derive(Clone, Debug)]
pub struct Program {
    pub frames: Vec<FrameTy>,
    pub defs: Vec<Def>,
    pub types: Vec<Type>,
}

/// A helper `def`, compiled as a function of scalars.
#[derive(Clone, Debug)]
pub struct Def {
    pub name: String,
    pub params: Vec<Prim>,
    pub ret: Prim,
    pub body: Body,
}

/// One `system` or `sequence`.
#[derive(Clone, Debug)]
pub struct Type {
    pub desc: mc_rt::desc::TypeDesc,
    pub init: Body,
    /// Statements that run first on every `step` or `resume`: the alerts.
    /// They use no locals.
    pub prelude: Vec<Stmt>,
    /// A system's `step`, or a sequence's `resume`.
    pub step: Body,
    pub redlines: Vec<Body>,
    pub on_abort: Option<Body>,
    /// The number of resume points, for a sequence.
    pub points: u32,
    /// The state offset of a sequence's `u32` resume point.
    pub resume_slot: u32,
}

/// A function body: its locals and statements. Local `i` of a function
/// with params is param `i`.
#[derive(Clone, Debug, Default)]
pub struct Body {
    pub locals: Vec<Prim>,
    pub stmts: Vec<Stmt>,
}

/// The block a place lives in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Base {
    Input,
    Output,
    State,
    Params,
}

/// A scalar in memory: a static offset plus clamped dynamic indices.
#[derive(Clone, Debug)]
pub struct Place {
    pub base: Base,
    pub offset: u32,
    pub index: Vec<Index>,
    pub prim: Prim,
}

/// One dynamic array index, clamped to `0..len`.
#[derive(Clone, Debug)]
pub struct Index {
    pub value: Expr,
    pub stride: u32,
    pub len: u32,
}

#[derive(Clone, Debug)]
pub struct Expr {
    pub kind: ExprKind,
    pub prim: Prim,
}

#[derive(Clone, Debug)]
pub enum ExprKind {
    Const(ConstVal),
    Local(u32),
    Load(Box<Place>),
    Neg(Box<Expr>),
    Not(Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    /// Short-circuit `and` / `or`.
    Logic {
        and: bool,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    /// Converts the operand to this expression's type.
    Cast(Box<Expr>),
    Call(u32, Vec<Expr>),
    /// A `libm` function, by symbol.
    Math(&'static str, Vec<Expr>),
    Builtin(Builtin, Vec<Expr>),
    /// Whether input `port` has no record, or one older than `age` seconds.
    Stale {
        port: u32,
        timestamp: u32,
        age: Box<Expr>,
    },
    /// The cycle time in microseconds, as `i64`.
    NowMicros,
    /// The cycle time in seconds.
    Now,
    Dt,
}

/// A binary operator on two operands of the same type. Comparisons give
/// `bool`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Builtin {
    Abs,
    Min,
    Max,
    Sqrt,
    Floor,
    Ceil,
    Trunc,
    Round,
}

#[derive(Clone, Debug)]
pub enum Stmt {
    SetLocal(u32, Expr),
    /// Stores a scalar. A float stored with `nan_guard` keeps the old value
    /// when it is NaN. `mark` is the output the store writes.
    Store {
        place: Place,
        value: Expr,
        nan_guard: bool,
        mark: Option<u32>,
    },
    /// Zeroes `len` bytes of a block, such as a frame being rebuilt.
    Zero {
        base: Base,
        offset: u32,
        len: u32,
    },
    /// Copies a whole frame between blocks.
    Copy {
        dst: (Base, u32),
        src: (Base, u32),
        len: u32,
        mark: Option<u32>,
    },
    /// Marks output `index` written.
    Mark(u32),
    If {
        cond: Expr,
        then: Vec<Stmt>,
        orelse: Vec<Stmt>,
    },
    /// Runs `body` with `var` from 0 to `count - 1`.
    For {
        var: Var,
        count: u32,
        body: Vec<Stmt>,
    },
    Return(Expr),
    /// Resume point `point`: waits until `deadline` (a state `i64`) passes.
    Wait {
        point: u32,
        deadline: u32,
        micros: Expr,
    },
    /// Resume point `point`: waits until `cond` holds, or fails with
    /// `fail` once a timeout's deadline passes.
    WaitUntil {
        point: u32,
        cond: Expr,
        timeout: Option<Timeout>,
    },
    /// Ends the sequence `Failed` with message `index`.
    Fail(u32),
}

#[derive(Clone, Debug)]
pub struct Timeout {
    pub deadline: u32,
    pub micros: Expr,
    pub fail: u32,
}

/// A loop variable: a local, or an `i64` state slot in a sequence.
#[derive(Clone, Copy, Debug)]
pub enum Var {
    Local(u32),
    State(u32),
}

impl Expr {
    pub fn constant(value: ConstVal, prim: Prim) -> Expr {
        Expr {
            kind: ExprKind::Const(value),
            prim,
        }
    }

    pub fn load(place: Place) -> Expr {
        Expr {
            prim: place.prim,
            kind: ExprKind::Load(Box::new(place)),
        }
    }

    pub fn cast(self, prim: Prim) -> Expr {
        match self.prim == prim {
            true => self,
            false => Expr {
                kind: ExprKind::Cast(Box::new(self)),
                prim,
            },
        }
    }
}

impl Place {
    pub fn at(base: Base, offset: u32, prim: Prim) -> Place {
        Place {
            base,
            offset,
            index: Vec::new(),
            prim,
        }
    }
}
