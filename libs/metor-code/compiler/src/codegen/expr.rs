//! Lowering expressions and the addresses of places.

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::{InstBuilder, Value, types};
use cranelift_module::Module;
use mc_rt::Fault;
use mc_rt::cx::offsets;

use super::clif;
use super::func::{FLAGS, FnCx};
use crate::hir::{BinOp, Builtin, Expr, ExprKind, Place};
use crate::types::{ConstVal, Prim};

impl<M: Module> FnCx<'_, '_, M> {
    pub fn expr(&mut self, e: &Expr) -> Value {
        match &e.kind {
            ExprKind::Const(value) => self.constant(*value, e.prim),
            ExprKind::Local(id) => self.b.use_var(self.locals[*id as usize]),
            ExprKind::Load(place) => {
                let addr = self.addr(place);
                self.b.ins().load(clif(place.prim), FLAGS, addr, 0)
            }
            ExprKind::Neg(x) => {
                let x = self.expr(x);
                match e.prim.is_float() {
                    true => self.b.ins().fneg(x),
                    false => self.b.ins().ineg(x),
                }
            }
            ExprKind::Not(x) => {
                let x = self.expr(x);
                self.b.ins().icmp_imm_s(IntCC::Equal, x, 0)
            }
            ExprKind::Binary(op, a, b) => {
                let prim = a.prim;
                let (a, b) = (self.expr(a), self.expr(b));
                self.binary(*op, prim, a, b)
            }
            ExprKind::Logic { and, lhs, rhs } => self.logic(*and, lhs, rhs),
            ExprKind::Cast(x) => {
                let from = x.prim;
                let x = self.expr(x);
                self.cast(x, from, e.prim)
            }
            ExprKind::Call(def, args) => {
                let mut values = vec![self.cx];
                values.extend(args.iter().map(|a| self.expr(a)));
                let callee = self.func_ref(self.funcs.defs[*def as usize]);
                let call = self.b.ins().call(callee, &values);
                self.b.inst_results(call)[0]
            }
            ExprKind::Math(symbol, args) => {
                let values: Vec<Value> = args.iter().map(|a| self.expr(a)).collect();
                let id = self.funcs.math[symbol];
                let callee = self.func_ref(id);
                let call = self.b.ins().call(callee, &values);
                self.b.inst_results(call)[0]
            }
            ExprKind::Builtin(builtin, args) => {
                let values: Vec<Value> = args.iter().map(|a| self.expr(a)).collect();
                self.builtin(*builtin, e.prim, &values)
            }
            ExprKind::Stale { port, timestamp, age } => self.stale(*port, *timestamp, age),
            ExprKind::NowMicros => self.b.ins().load(types::I64, FLAGS, self.cx, offsets::NOW),
            ExprKind::Now => {
                let now = self.b.ins().load(types::I64, FLAGS, self.cx, offsets::NOW);
                let now = self.b.ins().fcvt_from_sint(types::F64, now);
                let scale = self.b.ins().f64const(1e-6);
                self.b.ins().fmul(now, scale)
            }
            ExprKind::Dt => self.b.ins().load(types::F64, FLAGS, self.cx, offsets::DT),
        }
    }

    pub fn constant(&mut self, value: ConstVal, prim: Prim) -> Value {
        match (prim, value) {
            (Prim::F32, v) => self.b.ins().f32const(v.as_f64() as f32),
            (Prim::F64, v) => self.b.ins().f64const(v.as_f64()),
            (p, ConstVal::Int(v)) => self.b.ins().iconst(clif(p), v as i64),
            (p, ConstVal::Bool(v)) => self.b.ins().iconst(clif(p), i64::from(v)),
            (p, ConstVal::Float(v)) => self.b.ins().iconst(clif(p), v as i64),
        }
    }

    /// The address of a place: its block, its offset, and each index
    /// clamped to its array.
    pub fn addr(&mut self, place: &Place) -> Value {
        let mut addr = self.base(place.base);
        if place.offset != 0 {
            addr = self.b.ins().iadd_imm_s(addr, i64::from(place.offset));
        }
        for index in &place.index {
            let prim = index.value.prim;
            let value = self.expr(&index.value);
            let value = self.cast(value, prim, Prim::I64);
            let clamped = self.clamp_index(value, index.len);
            let scaled = self.b.ins().imul_imm_s(clamped, i64::from(index.stride));
            let scaled = match self.ptr == types::I64 {
                true => scaled,
                false => self.b.ins().ireduce(self.ptr, scaled),
            };
            addr = self.b.ins().iadd(addr, scaled);
        }
        addr
    }

    /// Clamps an `i64` index to `0..len`, raising `IndexClamped` if it was
    /// outside.
    fn clamp_index(&mut self, value: Value, len: u32) -> Value {
        let last = i64::from(len) - 1;
        let outside = self.b.ins().icmp_imm_s(IntCC::UnsignedGreaterThan, value, last);
        self.fault_if(outside, Fault::IndexClamped);
        let negative = self.b.ins().icmp_imm_s(IntCC::SignedLessThan, value, 0);
        let zero = self.b.ins().iconst(types::I64, 0);
        let max = self.b.ins().iconst(types::I64, last);
        let high = self.b.ins().select(outside, max, value);
        self.b.ins().select(negative, zero, high)
    }

    fn binary(&mut self, op: BinOp, prim: Prim, a: Value, b: Value) -> Value {
        if let Some(cc) = compare(op) {
            return match prim.is_float() {
                true => self.b.ins().fcmp(float_cc(cc), a, b),
                false => self.b.ins().icmp(int_cc(cc, prim.is_signed()), a, b),
            };
        }
        let ins = self.b.ins();
        match (op, prim.is_float()) {
            (BinOp::Add, true) => ins.fadd(a, b),
            (BinOp::Sub, true) => ins.fsub(a, b),
            (BinOp::Mul, true) => ins.fmul(a, b),
            (BinOp::Div, true) => ins.fdiv(a, b),
            (BinOp::Add, false) => ins.iadd(a, b),
            (BinOp::Sub, false) => ins.isub(a, b),
            (BinOp::Mul, false) => ins.imul(a, b),
            (BinOp::Div | BinOp::Rem, false) => self.int_div(op == BinOp::Div, prim, a, b),
            _ => a,
        }
    }

    /// Integer division or remainder that never traps: by zero it yields 0
    /// and raises `DivByZero`, and `MIN / -1` wraps.
    fn int_div(&mut self, div: bool, prim: Prim, a: Value, b: Value) -> Value {
        let ty = clif(prim);
        let zero = self.b.ins().iconst(ty, 0);
        let one = self.b.ins().iconst(ty, 1);
        let is_zero = self.b.ins().icmp_imm_s(IntCC::Equal, b, 0);
        self.fault_if(is_zero, Fault::DivByZero);
        let mut divisor = self.b.ins().select(is_zero, one, b);
        let mut minus_one = None;
        if prim.is_signed() {
            let is_m1 = self.b.ins().icmp_imm_s(IntCC::Equal, divisor, -1);
            divisor = self.b.ins().select(is_m1, one, divisor);
            minus_one = Some(is_m1);
        }
        let result = match (div, prim.is_signed()) {
            (true, true) => self.b.ins().sdiv(a, divisor),
            (true, false) => self.b.ins().udiv(a, divisor),
            (false, true) => self.b.ins().srem(a, divisor),
            (false, false) => self.b.ins().urem(a, divisor),
        };
        let result = match minus_one {
            Some(is_m1) => {
                let wrapped = match div {
                    true => self.b.ins().ineg(a),
                    false => zero,
                };
                self.b.ins().select(is_m1, wrapped, result)
            }
            None => result,
        };
        self.b.ins().select(is_zero, zero, result)
    }

    /// Short-circuit `and` / `or`.
    fn logic(&mut self, and: bool, lhs: &Expr, rhs: &Expr) -> Value {
        let lhs = self.expr(lhs);
        let rhs_block = self.b.create_block();
        let done = self.b.create_block();
        let result = self.b.append_block_param(done, types::I8);
        let short = self.b.ins().iconst(types::I8, i64::from(!and));
        let short = cranelift_codegen::ir::BlockArg::Value(short);
        match and {
            true => self.b.ins().brif(lhs, rhs_block, &[], done, &[short]),
            false => self.b.ins().brif(lhs, done, &[short], rhs_block, &[]),
        };
        self.b.switch_to_block(rhs_block);
        let rhs = self.expr(rhs);
        self.b.ins().jump(done, &[cranelift_codegen::ir::BlockArg::Value(rhs)]);
        self.b.switch_to_block(done);
        result
    }

    /// Converts between scalar types. Float to integer saturates.
    pub fn cast(&mut self, x: Value, from: Prim, to: Prim) -> Value {
        let (ft, tt) = (clif(from), clif(to));
        let ins = self.b.ins();
        match (from.is_float(), to.is_float()) {
            _ if from == to => x,
            (true, true) if tt.bits() > ft.bits() => ins.fpromote(tt, x),
            (true, true) => ins.fdemote(tt, x),
            (false, true) => {
                let x = match ft.bits() < 32 {
                    true => self.cast(x, from, if from.is_signed() { Prim::I32 } else { Prim::U32 }),
                    false => x,
                };
                match from.is_signed() {
                    true => self.b.ins().fcvt_from_sint(tt, x),
                    false => self.b.ins().fcvt_from_uint(tt, x),
                }
            }
            (true, false) if to == Prim::Bool => {
                let zero = self.zero(from);
                self.b.ins().fcmp(FloatCC::NotEqual, x, zero)
            }
            (true, false) if tt.bits() < 32 => {
                let wide = match to.is_signed() {
                    true => ins.fcvt_to_sint_sat(types::I32, x),
                    false => ins.fcvt_to_uint_sat(types::I32, x),
                };
                self.saturate(wide, to)
            }
            (true, false) if to.is_signed() => ins.fcvt_to_sint_sat(tt, x),
            (true, false) => ins.fcvt_to_uint_sat(tt, x),
            (false, false) if tt.bits() < ft.bits() => ins.ireduce(tt, x),
            (false, false) if tt.bits() == ft.bits() => x,
            (false, false) if from.is_signed() => ins.sextend(tt, x),
            (false, false) => ins.uextend(tt, x),
        }
    }

    /// Narrows an `i32` from a float conversion to an 8- or 16-bit integer,
    /// saturating.
    fn saturate(&mut self, wide: Value, to: Prim) -> Value {
        let bits = clif(to).bits();
        let (lo, hi) = match to.is_signed() {
            true => (-(1i64 << (bits - 1)), (1i64 << (bits - 1)) - 1),
            false => (0, (1i64 << bits) - 1),
        };
        let lo = self.b.ins().iconst(types::I32, lo);
        let hi = self.b.ins().iconst(types::I32, hi);
        let cc = if to.is_signed() { IntCC::SignedLessThan } else { IntCC::UnsignedLessThan };
        let below = self.b.ins().icmp(cc, wide, lo);
        let raised = self.b.ins().select(below, lo, wide);
        let cc = if to.is_signed() { IntCC::SignedGreaterThan } else { IntCC::UnsignedGreaterThan };
        let above = self.b.ins().icmp(cc, raised, hi);
        let clamped = self.b.ins().select(above, hi, raised);
        self.b.ins().ireduce(clif(to), clamped)
    }

    fn builtin(&mut self, builtin: Builtin, prim: Prim, args: &[Value]) -> Value {
        let float = prim.is_float();
        let ins = self.b.ins();
        match builtin {
            Builtin::Abs if float => ins.fabs(args[0]),
            Builtin::Abs => {
                let neg = ins.ineg(args[0]);
                let below = self.b.ins().icmp_imm_s(IntCC::SignedLessThan, args[0], 0);
                match prim.is_signed() {
                    true => self.b.ins().select(below, neg, args[0]),
                    false => args[0],
                }
            }
            Builtin::Min if float => ins.fmin(args[0], args[1]),
            Builtin::Max if float => ins.fmax(args[0], args[1]),
            Builtin::Min | Builtin::Max => {
                let less = int_cc(Cmp::Lt, prim.is_signed());
                let lt = ins.icmp(less, args[0], args[1]);
                match builtin == Builtin::Min {
                    true => self.b.ins().select(lt, args[0], args[1]),
                    false => self.b.ins().select(lt, args[1], args[0]),
                }
            }
            Builtin::Sqrt => ins.sqrt(args[0]),
            Builtin::Floor => ins.floor(args[0]),
            Builtin::Ceil => ins.ceil(args[0]),
            Builtin::Trunc => ins.trunc(args[0]),
            Builtin::Round => ins.nearest(args[0]),
        }
    }

    /// Whether an input has no record, or one older than `age` seconds.
    fn stale(&mut self, port: u32, timestamp: u32, age: &Expr) -> Value {
        let age = self.expr(age);
        let scale = self.b.ins().f64const(1e6);
        let age = self.b.ins().fmul(age, scale);
        let age = self.b.ins().fcvt_to_sint_sat(types::I64, age);
        let present = self.b.ins().load(types::I8, FLAGS, self.present, port as i32);
        let missing = self.b.ins().icmp_imm_s(IntCC::Equal, present, 0);
        let stamp = self.b.ins().load(types::I64, FLAGS, self.inputs, timestamp as i32);
        let now = self.b.ins().load(types::I64, FLAGS, self.cx, offsets::NOW);
        let elapsed = self.b.ins().isub(now, stamp);
        let old = self.b.ins().icmp(IntCC::SignedGreaterThan, elapsed, age);
        self.b.ins().bor(missing, old)
    }
}

#[derive(Clone, Copy)]
enum Cmp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

fn compare(op: BinOp) -> Option<Cmp> {
    Some(match op {
        BinOp::Eq => Cmp::Eq,
        BinOp::Ne => Cmp::Ne,
        BinOp::Lt => Cmp::Lt,
        BinOp::Le => Cmp::Le,
        BinOp::Gt => Cmp::Gt,
        BinOp::Ge => Cmp::Ge,
        _ => return None,
    })
}

fn int_cc(cmp: Cmp, signed: bool) -> IntCC {
    match (cmp, signed) {
        (Cmp::Eq, _) => IntCC::Equal,
        (Cmp::Ne, _) => IntCC::NotEqual,
        (Cmp::Lt, true) => IntCC::SignedLessThan,
        (Cmp::Le, true) => IntCC::SignedLessThanOrEqual,
        (Cmp::Gt, true) => IntCC::SignedGreaterThan,
        (Cmp::Ge, true) => IntCC::SignedGreaterThanOrEqual,
        (Cmp::Lt, false) => IntCC::UnsignedLessThan,
        (Cmp::Le, false) => IntCC::UnsignedLessThanOrEqual,
        (Cmp::Gt, false) => IntCC::UnsignedGreaterThan,
        (Cmp::Ge, false) => IntCC::UnsignedGreaterThanOrEqual,
    }
}

/// Float comparisons. `!=` holds for NaN, and every other comparison does
/// not.
fn float_cc(cmp: Cmp) -> FloatCC {
    match cmp {
        Cmp::Eq => FloatCC::Equal,
        Cmp::Ne => FloatCC::NotEqual,
        Cmp::Lt => FloatCC::LessThan,
        Cmp::Le => FloatCC::LessThanOrEqual,
        Cmp::Gt => FloatCC::GreaterThan,
        Cmp::Ge => FloatCC::GreaterThanOrEqual,
    }
}
