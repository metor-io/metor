//! The typed IR — the only representation of a program this crate keeps.
//!
//! Everything the checker decides is already decided here: which numeric type
//! an operator works in, where a promotion happens, which local a name means,
//! which prelude kernel a call lands on. Codegen reads this and emits; it does
//! not re-derive types, and it does not re-check anything the checker checked.
//!
//! nox's `noxpr` graph layer is deliberately absent. nox enters this crate as
//! a kernel library and as the differential oracle, never as an IR.

use crate::Ty;

pub(crate) struct Program {
    pub funcs: Vec<Func>,
    /// Parameter types per function, so codegen knows how a buffer-ABI
    /// callee's arguments cross without consulting the manifest.
    pub param_types: Vec<Vec<Ty>>,
    /// Every statically placed block of linear memory the program needs, in
    /// bytes. Codegen turns these into addresses above `__heap_base`.
    pub buffers: Vec<u32>,
}

/// A statically placed block of linear memory: a tensor parameter, return
/// value, named variable, or intermediate.
pub(crate) type BufId = u32;

pub(crate) struct Func {
    pub name: String,
    pub param_count: usize,
    /// Slot types for the whole frame; parameters occupy the first
    /// `param_count` entries. A tensor parameter has no slot — it lives in a
    /// buffer — so this is empty for buffer-ABI functions.
    pub locals: Vec<Ty>,
    pub body: Vec<Stmt>,
    /// Set when the signature mentions a tensor. Such a function takes no
    /// wasm parameters: everything crosses through [`Func::arg_buffers`] and
    /// [`Func::ret_buffer`].
    pub buffer_abi: bool,
    pub arg_buffers: Vec<BufId>,
    pub ret_buffer: Option<BufId>,
    /// Scalar parameters of a buffer-ABI function, loaded out of their
    /// buffers into slots on entry.
    pub prologue: Vec<(BufId, u32, Ty)>,
}

/// Shapes for one elementwise call site, already right-aligned to a common
/// rank with leading `1`s, which is the form the kernel reads.
#[derive(Clone)]
pub(crate) struct Desc {
    pub rank: u32,
    pub lhs: Vec<u32>,
    pub rhs: Vec<u32>,
    pub out: Vec<u32>,
}

/// The type an arithmetic or comparison operator works in, after promotion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Num {
    F64,
    I64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Arith {
    Add,
    Sub,
    Mul,
    /// True division: always `f64`, so `ty` is always [`Num::F64`].
    Div,
    /// Floor division, Python's `//` — sign follows the divisor.
    FloorDiv,
    /// Floored remainder, Python's `%` — sign follows the divisor.
    Rem,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cmp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// Operations that compile to native wasm opcodes rather than kernel calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Intrinsic {
    NegF64,
    NegI64,
    AbsF64,
    AbsI64,
    SqrtF64,
    FloorF64,
    CeilF64,
    TruncF64,
    NearestF64,
    MinF64,
    MaxF64,
    MinI64,
    MaxI64,
    /// `i64` widened to `f64` — the only implicit conversion in the language.
    IntToFloat,
    /// `f64` truncated toward zero; traps outside `i64`'s range.
    FloatToInt,
}

pub(crate) enum Expr {
    F64(f64),
    I64(i64),
    Bool(bool),
    Local(u32),
    Arith {
        op: Arith,
        ty: Num,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    /// Repeated multiplication for `**` with a small non-negative integer
    /// literal exponent.
    PowConst {
        ty: Num,
        base: Box<Expr>,
        exp: u32,
    },
    Cmp {
        op: Cmp,
        ty: Num,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    /// `bool` equality, which needs no numeric type.
    CmpBool {
        eq: bool,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
    Select {
        cond: Box<Expr>,
        then: Box<Expr>,
        els: Box<Expr>,
        ty: Ty,
    },
    Call {
        index: u32,
        args: Vec<Expr>,
    },
    /// A prelude function, named by its export.
    Kernel {
        name: &'static str,
        args: Vec<Expr>,
    },
    Intrinsic {
        op: Intrinsic,
        args: Vec<Expr>,
    },
    /// Evaluate `value` into `local`, then evaluate `then`. Chained
    /// comparisons need it: `a < b < c` must evaluate `b` exactly once.
    Store {
        local: u32,
        value: Box<Expr>,
        then: Box<Expr>,
    },

    /// A tensor already sitting in a buffer. Evaluating it costs nothing —
    /// every tensor address in a compiled program is a compile-time constant.
    Tensor(BufId),
    /// An elementwise kernel writing into `dest`, broadcasting per `desc`.
    Elementwise {
        kernel: &'static str,
        dest: BufId,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
        desc: Desc,
    },
    /// A scalar materialised into a one-element buffer so it can broadcast.
    Splat {
        dest: BufId,
        value: Box<Expr>,
    },
    TensorNeg {
        dest: BufId,
        operand: Box<Expr>,
        elems: u32,
    },
    /// One element of a tensor. A non-constant index is bounds-checked.
    Element {
        source: Box<Expr>,
        index: Box<Expr>,
        len: u32,
    },
    /// Inner product of two rank-1 tensors.
    Dot {
        lhs: Box<Expr>,
        rhs: Box<Expr>,
        len: u32,
    },
    /// Row-major matrix product into `dest`.
    MatMul {
        dest: BufId,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
        m: u32,
        k: u32,
        n: u32,
    },
    Sum {
        source: Box<Expr>,
        elems: u32,
    },
    /// Call a buffer-ABI function: arguments are copied into the callee's own
    /// buffers, and the result is copied out of them into `dest`.
    BufferCall {
        index: u32,
        args: Vec<Expr>,
        dest: Option<BufId>,
        ret: Ty,
    },
}

impl Expr {
    /// Where a tensor-valued expression lands. Every one has a statically
    /// known home, which is what lets kernel descriptors be constants.
    pub fn buffer(&self) -> BufId {
        match self {
            Expr::Tensor(id)
            | Expr::Elementwise { dest: id, .. }
            | Expr::Splat { dest: id, .. }
            | Expr::TensorNeg { dest: id, .. }
            | Expr::MatMul { dest: id, .. } => *id,
            Expr::BufferCall { dest: Some(id), .. } => *id,
            _ => unreachable!("only tensor-valued expressions have a buffer"),
        }
    }
}

pub(crate) enum Stmt {
    Assign {
        local: u32,
        value: Expr,
    },
    If {
        cond: Expr,
        then: Vec<Stmt>,
        els: Vec<Stmt>,
    },
    While {
        cond: Expr,
        body: Vec<Stmt>,
    },
    Break,
    Continue,
    Return(Expr),
    /// A bare expression statement, evaluated for its traps and dropped.
    Drop(Expr),
    /// Evaluate a tensor and copy it into `dest`.
    TensorAssign {
        dest: BufId,
        value: Expr,
        bytes: u32,
    },
    /// `v[i] = x`, bounds-checked when the index is not constant.
    ElementAssign {
        target: Expr,
        index: Expr,
        value: Expr,
        len: u32,
    },
    /// Return through the function's static buffer rather than by value.
    ReturnBuffer {
        dest: BufId,
        value: Expr,
        ty: Ty,
    },
}

impl Program {
    /// Every prelude export the program reaches, deduplicated.
    pub fn kernels(&self) -> Vec<&'static str> {
        let mut found = Vec::new();
        for func in &self.funcs {
            collect_stmts(&func.body, &mut found);
        }
        found.sort_unstable();
        found.dedup();
        found
    }
}

fn collect_stmts(stmts: &[Stmt], found: &mut Vec<&'static str>) {
    for stmt in stmts {
        match stmt {
            Stmt::Assign { value, .. } | Stmt::Return(value) | Stmt::Drop(value) => {
                collect_expr(value, found)
            }
            Stmt::TensorAssign { value, .. } | Stmt::ReturnBuffer { value, .. } => {
                collect_expr(value, found)
            }
            Stmt::ElementAssign {
                target,
                index,
                value,
                ..
            } => {
                collect_expr(target, found);
                collect_expr(index, found);
                collect_expr(value, found);
            }
            Stmt::If { cond, then, els } => {
                collect_expr(cond, found);
                collect_stmts(then, found);
                collect_stmts(els, found);
            }
            Stmt::While { cond, body } => {
                collect_expr(cond, found);
                collect_stmts(body, found);
            }
            Stmt::Break | Stmt::Continue => {}
        }
    }
}

fn collect_expr(expr: &Expr, found: &mut Vec<&'static str>) {
    match expr {
        Expr::Kernel { name, args } => {
            found.push(name);
            for arg in args {
                collect_expr(arg, found);
            }
        }
        Expr::Arith { lhs, rhs, .. } | Expr::Cmp { lhs, rhs, .. } => {
            collect_expr(lhs, found);
            collect_expr(rhs, found);
        }
        Expr::CmpBool { lhs, rhs, .. } | Expr::And(lhs, rhs) | Expr::Or(lhs, rhs) => {
            collect_expr(lhs, found);
            collect_expr(rhs, found);
        }
        Expr::Not(e) => collect_expr(e, found),
        Expr::PowConst { base, .. } => collect_expr(base, found),
        Expr::Select {
            cond, then, els, ..
        } => {
            collect_expr(cond, found);
            collect_expr(then, found);
            collect_expr(els, found);
        }
        Expr::Call { args, .. } | Expr::Intrinsic { args, .. } => {
            for arg in args {
                collect_expr(arg, found);
            }
        }
        Expr::Store { value, then, .. } => {
            collect_expr(value, found);
            collect_expr(then, found);
        }
        Expr::Elementwise { kernel, lhs, rhs, .. } => {
            found.push(kernel);
            collect_expr(lhs, found);
            collect_expr(rhs, found);
        }
        Expr::TensorNeg { operand, .. } => {
            found.push("k_neg");
            collect_expr(operand, found);
        }
        Expr::Dot { lhs, rhs, .. } => {
            found.push("k_dot");
            collect_expr(lhs, found);
            collect_expr(rhs, found);
        }
        Expr::MatMul { lhs, rhs, .. } => {
            found.push("k_matmul");
            collect_expr(lhs, found);
            collect_expr(rhs, found);
        }
        Expr::Sum { source, .. } => {
            found.push("k_sum");
            collect_expr(source, found);
        }
        Expr::Splat { value, .. } => collect_expr(value, found),
        Expr::Element { source, index, .. } => {
            collect_expr(source, found);
            collect_expr(index, found);
        }
        Expr::BufferCall { args, .. } => {
            for arg in args {
                collect_expr(arg, found);
            }
        }
        Expr::F64(_) | Expr::I64(_) | Expr::Bool(_) | Expr::Local(_) | Expr::Tensor(_) => {}
    }
}
