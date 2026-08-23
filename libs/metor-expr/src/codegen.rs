//! Typed IR to wasm, appended to the prelude.
//!
//! Scalar arithmetic is native opcodes; the prelude is entered only where wasm
//! has no instruction — the transcendentals, and `fmod_floor` for float `%`.
//! Two operators cost more than one instruction, and both for the same reason:
//! wasm's `rem` truncates where Python floors, so `//` and `%` on integers
//! carry a correction that adjusts the result when the operands' signs differ.
//!
//! Nothing here re-checks anything. The IR arrived from a single validation
//! gate, so a `Call` index is in range, an operand type is already unified, and
//! a condition is already `bool`.

use std::collections::HashMap;

use wasm_encoder::{BlockType, Function, Instruction, ValType};

use crate::ir::{Arith, Cmp, Expr, Func, Intrinsic, Num, Program, Stmt};
use crate::template::Template;
use crate::{Manifest, PRELUDE, Ty};

pub(crate) fn emit(program: &Program, manifest: &Manifest) -> Vec<u8> {
    let kernels = program.kernels();
    let plan = Template::parse(PRELUDE)
        .expect("the checked-in prelude parses")
        .plan(&kernels)
        .expect("the checked-in prelude exports every kernel the IR names");
    let mut splice = plan.splice();

    let kernel_index: HashMap<&'static str, u32> =
        kernels.iter().map(|k| (*k, splice.kernel(k))).collect();
    let base = splice.next_function();

    let types: Vec<u32> = manifest
        .functions
        .iter()
        .map(|sig| {
            let params: Vec<ValType> = sig.params.iter().map(|(_, t)| val_type(t)).collect();
            splice.ty(params, vec![val_type(&sig.ret)])
        })
        .collect();

    for (func, ty) in program.funcs.iter().zip(&types) {
        let body = Emitter::new(func, &kernel_index, base).finish(func);
        let index = splice.function(*ty, body);
        splice.export(func.name.clone(), index);
    }

    splice.finish()
}

pub(crate) fn val_type(ty: &Ty) -> ValType {
    match ty {
        Ty::F64 => ValType::F64,
        Ty::I64 => ValType::I64,
        Ty::Bool => ValType::I32,
        Ty::Tensor { .. } => ValType::I32,
    }
}

struct Emitter<'a> {
    locals: Vec<ValType>,
    param_count: usize,
    free: HashMap<ValType, Vec<u32>>,
    code: Vec<Instruction<'static>>,
    depth: u32,
    loops: Vec<(u32, u32)>,
    kernels: &'a HashMap<&'static str, u32>,
    base: u32,
}

impl<'a> Emitter<'a> {
    fn new(func: &Func, kernels: &'a HashMap<&'static str, u32>, base: u32) -> Self {
        Emitter {
            locals: func.locals.iter().map(val_type).collect(),
            param_count: func.param_count,
            free: HashMap::new(),
            code: Vec::new(),
            depth: 0,
            loops: Vec::new(),
            kernels,
            base,
        }
    }

    fn finish(mut self, func: &Func) -> Function {
        self.block(&func.body);
        self.code.push(Instruction::Unreachable);
        self.code.push(Instruction::End);

        let mut runs: Vec<(u32, ValType)> = Vec::new();
        for ty in &self.locals[self.param_count..] {
            match runs.last_mut() {
                Some((count, last)) if last == ty => *count += 1,
                _ => runs.push((1, *ty)),
            }
        }
        let mut body = Function::new(runs);
        for instruction in &self.code {
            body.instruction(instruction);
        }
        body
    }

    fn acquire(&mut self, ty: ValType) -> u32 {
        match self.free.get_mut(&ty).and_then(Vec::pop) {
            Some(slot) => slot,
            None => {
                self.locals.push(ty);
                (self.locals.len() - 1) as u32
            }
        }
    }

    fn release(&mut self, ty: ValType, slot: u32) {
        self.free.entry(ty).or_default().push(slot);
    }

    fn push(&mut self, instruction: Instruction<'static>) {
        self.code.push(instruction);
    }

    fn block(&mut self, stmts: &[Stmt]) {
        for stmt in stmts {
            self.stmt(stmt);
        }
    }

    fn stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Assign { local, value } => {
                self.expr(value);
                self.push(Instruction::LocalSet(*local));
            }
            Stmt::Return(value) => {
                self.expr(value);
                self.push(Instruction::Return);
            }
            Stmt::Drop(value) => {
                self.expr(value);
                self.push(Instruction::Drop);
            }
            Stmt::If { cond, then, els } => {
                self.expr(cond);
                self.push(Instruction::If(BlockType::Empty));
                self.depth += 1;
                self.block(then);
                if !els.is_empty() {
                    self.push(Instruction::Else);
                    self.block(els);
                }
                self.depth -= 1;
                self.push(Instruction::End);
            }
            Stmt::While { cond, body } => {
                self.push(Instruction::Block(BlockType::Empty));
                self.depth += 1;
                let break_frame = self.depth;
                self.push(Instruction::Loop(BlockType::Empty));
                self.depth += 1;
                let continue_frame = self.depth;
                self.loops.push((break_frame, continue_frame));

                self.expr(cond);
                self.push(Instruction::I32Eqz);
                let out = self.depth - break_frame;
                self.push(Instruction::BrIf(out));
                self.block(body);
                let back = self.depth - continue_frame;
                self.push(Instruction::Br(back));

                self.loops.pop();
                self.depth -= 1;
                self.push(Instruction::End);
                self.depth -= 1;
                self.push(Instruction::End);
            }
            Stmt::Break => {
                let (frame, _) = *self.loops.last().expect("the checker rejects a bare break");
                let target = self.depth - frame;
                self.push(Instruction::Br(target));
            }
            Stmt::Continue => {
                let (_, frame) = *self
                    .loops
                    .last()
                    .expect("the checker rejects a bare continue");
                let target = self.depth - frame;
                self.push(Instruction::Br(target));
            }
        }
    }

    fn expr(&mut self, expr: &Expr) {
        match expr {
            Expr::F64(v) => self.push(Instruction::F64Const((*v).into())),
            Expr::I64(v) => self.push(Instruction::I64Const(*v)),
            Expr::Bool(v) => self.push(Instruction::I32Const(*v as i32)),
            Expr::Local(slot) => self.push(Instruction::LocalGet(*slot)),
            Expr::Store { local, value, then } => {
                self.expr(value);
                self.push(Instruction::LocalSet(*local));
                self.expr(then);
            }
            Expr::Arith { op, ty, lhs, rhs } => self.arith(*op, *ty, lhs, rhs),
            Expr::PowConst { ty, base, exp } => self.pow_const(*ty, base, *exp),
            Expr::Cmp { op, ty, lhs, rhs } => {
                self.expr(lhs);
                self.expr(rhs);
                self.push(compare(*op, *ty));
            }
            Expr::CmpBool { eq, lhs, rhs } => {
                self.expr(lhs);
                self.expr(rhs);
                self.push(if *eq {
                    Instruction::I32Eq
                } else {
                    Instruction::I32Ne
                });
            }
            Expr::Not(inner) => {
                self.expr(inner);
                self.push(Instruction::I32Eqz);
            }
            Expr::And(lhs, rhs) => {
                self.expr(lhs);
                self.push(Instruction::If(BlockType::Result(ValType::I32)));
                self.depth += 1;
                self.expr(rhs);
                self.push(Instruction::Else);
                self.push(Instruction::I32Const(0));
                self.depth -= 1;
                self.push(Instruction::End);
            }
            Expr::Or(lhs, rhs) => {
                self.expr(lhs);
                self.push(Instruction::If(BlockType::Result(ValType::I32)));
                self.depth += 1;
                self.push(Instruction::I32Const(1));
                self.push(Instruction::Else);
                self.expr(rhs);
                self.depth -= 1;
                self.push(Instruction::End);
            }
            Expr::Select {
                cond,
                then,
                els,
                ty,
            } => {
                self.expr(cond);
                self.push(Instruction::If(BlockType::Result(val_type(ty))));
                self.depth += 1;
                self.expr(then);
                self.push(Instruction::Else);
                self.expr(els);
                self.depth -= 1;
                self.push(Instruction::End);
            }
            Expr::Call { index, args, .. } => {
                for arg in args {
                    self.expr(arg);
                }
                self.push(Instruction::Call(self.base + index));
            }
            Expr::Kernel { name, args, .. } => {
                for arg in args {
                    self.expr(arg);
                }
                self.push(Instruction::Call(self.kernels[name]));
            }
            Expr::Intrinsic { op, args } => self.intrinsic(*op, args),
        }
    }

    fn arith(&mut self, op: Arith, ty: Num, lhs: &Expr, rhs: &Expr) {
        match (op, ty) {
            (Arith::Add, Num::F64) => self.simple(lhs, rhs, Instruction::F64Add),
            (Arith::Add, Num::I64) => self.simple(lhs, rhs, Instruction::I64Add),
            (Arith::Sub, Num::F64) => self.simple(lhs, rhs, Instruction::F64Sub),
            (Arith::Sub, Num::I64) => self.simple(lhs, rhs, Instruction::I64Sub),
            (Arith::Mul, Num::F64) => self.simple(lhs, rhs, Instruction::F64Mul),
            (Arith::Mul, Num::I64) => self.simple(lhs, rhs, Instruction::I64Mul),
            (Arith::Div, _) => self.simple(lhs, rhs, Instruction::F64Div),
            (Arith::FloorDiv, Num::F64) => {
                self.simple(lhs, rhs, Instruction::F64Div);
                self.push(Instruction::F64Floor);
            }
            (Arith::FloorDiv, Num::I64) => self.floor_div_i64(lhs, rhs),
            (Arith::Rem, Num::I64) => self.floor_rem_i64(lhs, rhs),
            (Arith::Rem, Num::F64) => unreachable!("float `%` lowers to the fmod_floor kernel"),
        }
    }

    fn simple(&mut self, lhs: &Expr, rhs: &Expr, op: Instruction<'static>) {
        self.expr(lhs);
        self.expr(rhs);
        self.push(op);
    }

    /// `a // b` with Python's sign rule: `i64.div_s` truncates, so subtract one
    /// when the remainder is non-zero and the operands disagree in sign.
    fn floor_div_i64(&mut self, lhs: &Expr, rhs: &Expr) {
        let (a, b, r) = (
            self.acquire(ValType::I64),
            self.acquire(ValType::I64),
            self.acquire(ValType::I64),
        );
        self.expr(lhs);
        self.push(Instruction::LocalSet(a));
        self.expr(rhs);
        self.push(Instruction::LocalSet(b));

        self.push(Instruction::LocalGet(a));
        self.push(Instruction::LocalGet(b));
        self.push(Instruction::I64DivS);

        self.push(Instruction::LocalGet(a));
        self.push(Instruction::LocalGet(b));
        self.push(Instruction::I64RemS);
        self.push(Instruction::LocalSet(r));
        self.signs_differ(b, r);
        self.push(Instruction::I64ExtendI32U);
        self.push(Instruction::I64Sub);

        self.release(ValType::I64, a);
        self.release(ValType::I64, b);
        self.release(ValType::I64, r);
    }

    /// `a % b` with Python's sign rule: add the divisor back when the
    /// truncating remainder landed on the wrong side of zero.
    fn floor_rem_i64(&mut self, lhs: &Expr, rhs: &Expr) {
        let (a, b, r) = (
            self.acquire(ValType::I64),
            self.acquire(ValType::I64),
            self.acquire(ValType::I64),
        );
        self.expr(lhs);
        self.push(Instruction::LocalSet(a));
        self.expr(rhs);
        self.push(Instruction::LocalSet(b));

        self.push(Instruction::LocalGet(a));
        self.push(Instruction::LocalGet(b));
        self.push(Instruction::I64RemS);
        self.push(Instruction::LocalTee(r));

        self.push(Instruction::LocalGet(b));
        self.push(Instruction::I64Const(0));
        self.signs_differ(b, r);
        self.push(Instruction::Select);
        self.push(Instruction::I64Add);

        self.release(ValType::I64, a);
        self.release(ValType::I64, b);
        self.release(ValType::I64, r);
    }

    /// Leaves `1` on the stack when the remainder in `r` is non-zero and its
    /// sign disagrees with the divisor in `b`.
    fn signs_differ(&mut self, b: u32, r: u32) {
        self.push(Instruction::LocalGet(r));
        self.push(Instruction::I64Eqz);
        self.push(Instruction::I32Eqz);
        self.push(Instruction::LocalGet(r));
        self.push(Instruction::LocalGet(b));
        self.push(Instruction::I64Xor);
        self.push(Instruction::I64Const(0));
        self.push(Instruction::I64LtS);
        self.push(Instruction::I32And);
    }

    fn pow_const(&mut self, ty: Num, base: &Expr, exp: u32) {
        let one = match ty {
            Num::F64 => Instruction::F64Const(1.0.into()),
            Num::I64 => Instruction::I64Const(1),
        };
        if exp == 0 {
            self.push(one);
            return;
        }
        if exp == 1 {
            self.expr(base);
            return;
        }
        let slot = self.acquire(val_type_of(ty));
        self.expr(base);
        self.push(Instruction::LocalSet(slot));
        self.push(Instruction::LocalGet(slot));
        for _ in 1..exp {
            self.push(Instruction::LocalGet(slot));
            self.push(match ty {
                Num::F64 => Instruction::F64Mul,
                Num::I64 => Instruction::I64Mul,
            });
        }
        self.release(val_type_of(ty), slot);
    }

    fn intrinsic(&mut self, op: Intrinsic, args: &[Expr]) {
        match op {
            Intrinsic::NegF64 => {
                self.expr(&args[0]);
                self.push(Instruction::F64Neg);
            }
            Intrinsic::NegI64 => {
                self.push(Instruction::I64Const(0));
                self.expr(&args[0]);
                self.push(Instruction::I64Sub);
            }
            Intrinsic::AbsF64 => {
                self.expr(&args[0]);
                self.push(Instruction::F64Abs);
            }
            Intrinsic::AbsI64 => {
                let slot = self.acquire(ValType::I64);
                self.expr(&args[0]);
                self.push(Instruction::LocalSet(slot));
                self.push(Instruction::LocalGet(slot));
                self.push(Instruction::I64Const(0));
                self.push(Instruction::LocalGet(slot));
                self.push(Instruction::I64Sub);
                self.push(Instruction::LocalGet(slot));
                self.push(Instruction::I64Const(0));
                self.push(Instruction::I64GeS);
                self.push(Instruction::Select);
                self.release(ValType::I64, slot);
            }
            Intrinsic::SqrtF64 => {
                self.expr(&args[0]);
                self.push(Instruction::F64Sqrt);
            }
            Intrinsic::FloorF64 => {
                self.expr(&args[0]);
                self.push(Instruction::F64Floor);
            }
            Intrinsic::CeilF64 => {
                self.expr(&args[0]);
                self.push(Instruction::F64Ceil);
            }
            Intrinsic::TruncF64 => {
                self.expr(&args[0]);
                self.push(Instruction::F64Trunc);
            }
            Intrinsic::NearestF64 => {
                self.expr(&args[0]);
                self.push(Instruction::F64Nearest);
            }
            Intrinsic::MinF64 => self.simple(&args[0], &args[1], Instruction::F64Min),
            Intrinsic::MaxF64 => self.simple(&args[0], &args[1], Instruction::F64Max),
            Intrinsic::MinI64 | Intrinsic::MaxI64 => {
                let (a, b) = (self.acquire(ValType::I64), self.acquire(ValType::I64));
                self.expr(&args[0]);
                self.push(Instruction::LocalSet(a));
                self.expr(&args[1]);
                self.push(Instruction::LocalSet(b));
                self.push(Instruction::LocalGet(a));
                self.push(Instruction::LocalGet(b));
                self.push(Instruction::LocalGet(a));
                self.push(Instruction::LocalGet(b));
                self.push(if op == Intrinsic::MinI64 {
                    Instruction::I64LtS
                } else {
                    Instruction::I64GtS
                });
                self.push(Instruction::Select);
                self.release(ValType::I64, a);
                self.release(ValType::I64, b);
            }
            Intrinsic::IntToFloat => {
                self.expr(&args[0]);
                self.push(Instruction::F64ConvertI64S);
            }
            Intrinsic::FloatToInt => {
                self.expr(&args[0]);
                self.push(Instruction::I64TruncF64S);
            }
        }
    }
}

fn val_type_of(ty: Num) -> ValType {
    match ty {
        Num::F64 => ValType::F64,
        Num::I64 => ValType::I64,
    }
}

fn compare(op: Cmp, ty: Num) -> Instruction<'static> {
    match (op, ty) {
        (Cmp::Eq, Num::F64) => Instruction::F64Eq,
        (Cmp::Ne, Num::F64) => Instruction::F64Ne,
        (Cmp::Lt, Num::F64) => Instruction::F64Lt,
        (Cmp::Le, Num::F64) => Instruction::F64Le,
        (Cmp::Gt, Num::F64) => Instruction::F64Gt,
        (Cmp::Ge, Num::F64) => Instruction::F64Ge,
        (Cmp::Eq, Num::I64) => Instruction::I64Eq,
        (Cmp::Ne, Num::I64) => Instruction::I64Ne,
        (Cmp::Lt, Num::I64) => Instruction::I64LtS,
        (Cmp::Le, Num::I64) => Instruction::I64LeS,
        (Cmp::Gt, Num::I64) => Instruction::I64GtS,
        (Cmp::Ge, Num::I64) => Instruction::I64GeS,
    }
}
