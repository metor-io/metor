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
    /// Every statically placed piece of linear memory the program needs.
    /// Codegen turns these into addresses above `__heap_base`.
    pub buffers: Vec<Place>,
}

/// A statically placed value: a tensor parameter, return value, named
/// variable, intermediate, frame, frame field, or state slot.
pub(crate) type BufId = u32;

/// Where a statically placed value lives.
///
/// Frames need both halves. The host addresses a whole input frame through one
/// pointer, so a frame is a [`Place::Block`]; the body addresses each field
/// separately, so every field is a [`Place::Field`] into it. Nothing is
/// relocated at run time — a field's address is its frame's plus a constant.
pub(crate) enum Place {
    Block(u32),
    Field { parent: BufId, offset: u32 },
}

pub(crate) struct Func {
    pub name: String,
    pub param_count: usize,
    /// Slot types for the whole frame; parameters occupy the first
    /// `param_count` entries. A tensor parameter has no slot — it lives in a
    /// buffer — so this is empty for buffer-ABI functions.
    pub locals: Vec<Ty>,
    pub ret: Ty,
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
    /// Set for a `@system`, which is exported as `<name>_eval(now)` and adds
    /// `<name>_state_ptr(i)` to the buffer ABI.
    pub system: Option<SystemAbi>,
}

pub(crate) struct SystemAbi {
    /// One block per state field, addressed individually because snapshot and
    /// restore work field by field, followed by the seed guard — which is a
    /// state slot like any other, so a host that restores a snapshot marks the
    /// instance seeded with the same byte copy it uses for everything else.
    pub state: Vec<BufId>,
    /// What the first evaluation writes into the state slots, for the
    /// defaults a fresh linear memory does not already supply.
    pub seeds: Vec<Seed>,
}

pub(crate) struct Seed {
    pub dest: BufId,
    pub ty: Ty,
    pub value: crate::Init,
}

/// One matrix product of a batched `@`, as element offsets into the three
/// operands. A rank-2 `@` is the single batch `(0, 0, 0)`.
#[derive(Clone, Copy)]
pub(crate) struct Batch {
    pub lhs: u32,
    pub rhs: u32,
    pub out: u32,
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

/// Selects inline or kernel emission for a tensor operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Emit {
    Open,
    Kernel,
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
    /// A scalar living in a buffer rather than a local: a frame field, a state
    /// slot, or a projected port.
    Load {
        buf: BufId,
        ty: Ty,
    },
    /// A buffer's address, for the one kernel that reads *and writes* its
    /// argument: `random()` advances the state word it returns from.
    Address(BufId),
    /// Elementwise arithmetic writing into `dest`, broadcasting per `desc`.
    Elementwise {
        kernel: &'static str,
        dest: BufId,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
        desc: Desc,
        emit: Emit,
    },
    /// A scalar materialised into a one-element buffer so it can broadcast.
    Splat {
        dest: BufId,
        value: Box<Expr>,
    },
    /// `[a, b, c]`: element expressions written into consecutive slots.
    ///
    /// Rank two is the same node with its elements already in row-major
    /// order, because a literal's shape is static and a tensor is contiguous —
    /// there is nothing left for the emitter to arrange.
    TensorLit {
        dest: BufId,
        elements: Vec<Expr>,
    },
    TensorNeg {
        dest: BufId,
        operand: Box<Expr>,
        elems: u32,
        emit: Emit,
    },
    /// A dynamic tensor index checked against one axis.
    CheckedIndex {
        value: Box<Expr>,
        len: u32,
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
        emit: Emit,
    },
    /// Row-major `(m, k) @ (k, n)` into `dest`, once per entry of `batches`.
    ///
    /// Leading dimensions broadcast, so the checker walks their odometer and
    /// leaves one [`Batch`] per product — the addresses are constants, and a
    /// batch is a stride the emitter no longer has to compute.
    MatMul {
        dest: BufId,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
        m: u32,
        k: u32,
        n: u32,
        batches: Vec<Batch>,
        emit: Emit,
    },
    Sum {
        source: Box<Expr>,
        elems: u32,
        emit: Emit,
    },
    /// Push a sample into a state-slot ring and yield the whole ring.
    ///
    /// The ring *is* the result — the buffer this evaluates to is the state
    /// slot itself, shifted one sample left with the new one written at the
    /// end, so the newest sample is last exactly as the panel's Window node
    /// published it.
    Window {
        state: BufId,
        value: Box<Expr>,
        /// Elements in one sample.
        elems: u32,
        /// Samples the ring holds.
        len: u32,
    },
    /// One-sided magnitude spectrum along the last axis, through `k_fft`.
    Fft {
        dest: BufId,
        source: Box<Expr>,
        /// Scratch the kernel needs, `2 * len` doubles, since the guest has no
        /// allocator to size one at run time.
        scratch: BufId,
        len: u32,
        groups: u32,
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
            | Expr::TensorLit { dest: id, .. }
            | Expr::TensorNeg { dest: id, .. }
            | Expr::MatMul { dest: id, .. }
            | Expr::Fft { dest: id, .. }
            | Expr::Window { state: id, .. } => *id,
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
    /// Write a scalar into a buffer: a state slot, or one field of the output
    /// frame.
    Store {
        dest: BufId,
        value: Expr,
        ty: Ty,
    },
    /// Fill every field of a system's output frame and report success.
    /// Publishing is what returning means.
    ReturnFrame(Vec<FieldWrite>),
}

pub(crate) struct FieldWrite {
    pub dest: BufId,
    pub value: Expr,
    pub ty: Ty,
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
            Stmt::TensorAssign { value, .. }
            | Stmt::ReturnBuffer { value, .. }
            | Stmt::Store { value, .. } => collect_expr(value, found),
            Stmt::ReturnFrame(fields) => {
                for field in fields {
                    collect_expr(&field.value, found);
                }
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
        Expr::Elementwise {
            kernel,
            lhs,
            rhs,
            emit,
            ..
        } => {
            // Open coding still needs the transcendentals, one call per
            // element; `+ - * /` become instructions and reach nothing.
            match (emit, *kernel) {
                (Emit::Kernel, _) => found.push(kernel),
                (Emit::Open, "k_pow") => found.push("pow"),
                (Emit::Open, "k_atan2") => found.push("atan2"),
                (Emit::Open, _) => {}
            }
            collect_expr(lhs, found);
            collect_expr(rhs, found);
        }
        Expr::TensorNeg { operand, emit, .. } => {
            if *emit == Emit::Kernel {
                found.push("k_neg");
            }
            collect_expr(operand, found);
        }
        Expr::Dot { lhs, rhs, emit, .. } => {
            if *emit == Emit::Kernel {
                found.push("k_dot");
            }
            collect_expr(lhs, found);
            collect_expr(rhs, found);
        }
        Expr::MatMul { lhs, rhs, emit, .. } => {
            if *emit == Emit::Kernel {
                found.push("k_matmul");
            }
            collect_expr(lhs, found);
            collect_expr(rhs, found);
        }
        Expr::Sum { source, emit, .. } => {
            if *emit == Emit::Kernel {
                found.push("k_sum");
            }
            collect_expr(source, found);
        }
        Expr::Window { value, .. } => collect_expr(value, found),
        Expr::Fft { source, .. } => {
            found.push("k_fft");
            collect_expr(source, found);
        }
        Expr::Splat { value, .. } => collect_expr(value, found),
        Expr::TensorLit { elements, .. } => {
            for element in elements {
                collect_expr(element, found);
            }
        }
        Expr::CheckedIndex { value, .. } => collect_expr(value, found),
        Expr::Element { source, index, .. } => {
            collect_expr(source, found);
            collect_expr(index, found);
        }
        Expr::BufferCall { args, .. } => {
            for arg in args {
                collect_expr(arg, found);
            }
        }
        Expr::F64(_)
        | Expr::I64(_)
        | Expr::Bool(_)
        | Expr::Local(_)
        | Expr::Tensor(_)
        | Expr::Address(_)
        | Expr::Load { .. } => {}
    }
}
