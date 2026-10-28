//! Lowering statements, including a sequence's resume points.

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::{InstBuilder, Value, types};
use cranelift_module::Module;
use mc_rt::cx::offsets;
use mc_rt::{Fault, ResumeWord};

use super::func::{FLAGS, FnCx};
use crate::hir::{Base, Place, Stmt, Timeout, Var};
use crate::types::Prim;

impl<M: Module> FnCx<'_, '_, M> {
    pub fn stmts(&mut self, stmts: &[Stmt]) {
        for stmt in stmts {
            self.stmt(stmt);
        }
    }

    fn stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::SetLocal(id, value) => {
                let value = self.expr(value);
                self.b.def_var(self.locals[*id as usize], value);
            }
            Stmt::Store {
                place,
                value,
                nan_guard,
                mark,
            } => {
                let value = self.expr(value);
                self.store(place, value, *nan_guard);
                if let Some(port) = mark {
                    self.mark(*port);
                }
            }
            Stmt::Zero { base, offset, len } => self.zero_bytes(*base, *offset, *len),
            Stmt::Copy {
                dst,
                src,
                len,
                mark,
            } => {
                self.copy(*dst, *src, *len);
                if let Some(port) = mark {
                    self.mark(*port);
                }
            }
            Stmt::Mark(port) => self.mark(*port),
            Stmt::If { cond, then, orelse } => self.if_else(cond, then, orelse),
            Stmt::For { var, count, body } => self.for_loop(*var, *count, body),
            Stmt::Return(value) => self.ret(value),
            Stmt::Wait {
                point,
                deadline,
                micros,
            } => self.wait(*point, *deadline, micros),
            Stmt::WaitUntil {
                point,
                cond,
                timeout,
            } => self.wait_until(*point, cond, timeout.as_ref()),
            Stmt::Fail(index) => self.fail(*index),
        }
    }

    /// Stores a scalar. With `nan_guard`, a NaN is not stored and raises
    /// `NanWrite`.
    fn store(&mut self, place: &Place, value: Value, nan_guard: bool) {
        let addr = self.addr(place);
        if !nan_guard {
            self.b.ins().store(FLAGS, value, addr, 0);
            return;
        }
        let is_nan = self.b.ins().fcmp(FloatCC::Unordered, value, value);
        self.fault_if(is_nan, Fault::NanWrite);
        let (store, done) = (self.b.create_block(), self.b.create_block());
        self.b.ins().brif(is_nan, done, &[], store, &[]);
        self.b.switch_to_block(store);
        self.b.ins().store(FLAGS, value, addr, 0);
        self.b.ins().jump(done, &[]);
        self.b.switch_to_block(done);
    }

    /// Marks output `port` written this call.
    fn mark(&mut self, port: u32) {
        let one = self.b.ins().iconst(types::I8, 1);
        self.b.ins().store(FLAGS, one, self.written, port as i32);
    }

    /// Zeroes `len` bytes from `offset`, eight at a time where it can.
    fn zero_bytes(&mut self, base: Base, offset: u32, len: u32) {
        let ptr = self.base(base);
        let wide = self.b.ins().iconst(types::I64, 0);
        let narrow = self.b.ins().iconst(types::I8, 0);
        let mut at = offset;
        while at < offset + len {
            let fits = at.is_multiple_of(8) && at + 8 <= offset + len;
            let (value, step) = if fits { (wide, 8) } else { (narrow, 1) };
            self.b.ins().store(FLAGS, value, ptr, at as i32);
            at += step;
        }
    }

    fn copy(&mut self, dst: (Base, u32), src: (Base, u32), len: u32) {
        let (to, from) = (self.base(dst.0), self.base(src.0));
        let mut at = 0;
        while at < len {
            let wide =
                (dst.1 + at).is_multiple_of(8) && (src.1 + at).is_multiple_of(8) && at + 8 <= len;
            let (ty, step) = if wide {
                (types::I64, 8)
            } else {
                (types::I8, 1)
            };
            let value = self.b.ins().load(ty, FLAGS, from, (src.1 + at) as i32);
            self.b.ins().store(FLAGS, value, to, (dst.1 + at) as i32);
            at += step;
        }
    }

    fn if_else(&mut self, cond: &crate::hir::Expr, then: &[Stmt], orelse: &[Stmt]) {
        let cond = self.expr(cond);
        let (then_block, else_block, done) = (
            self.b.create_block(),
            self.b.create_block(),
            self.b.create_block(),
        );
        self.b.ins().brif(cond, then_block, &[], else_block, &[]);
        self.b.switch_to_block(then_block);
        self.stmts(then);
        self.b.ins().jump(done, &[]);
        self.b.switch_to_block(else_block);
        self.stmts(orelse);
        self.b.ins().jump(done, &[]);
        self.b.switch_to_block(done);
    }

    fn for_loop(&mut self, var: Var, count: u32, body: &[Stmt]) {
        let zero = self.b.ins().iconst(types::I64, 0);
        self.set_var(var, zero);
        let (head, body_block, done) = (
            self.b.create_block(),
            self.b.create_block(),
            self.b.create_block(),
        );
        self.b.ins().jump(head, &[]);
        self.b.switch_to_block(head);
        let i = self.get_var(var);
        let more = self
            .b
            .ins()
            .icmp_imm_s(IntCC::SignedLessThan, i, i64::from(count));
        self.b.ins().brif(more, body_block, &[], done, &[]);
        self.b.switch_to_block(body_block);
        self.stmts(body);
        let i = self.get_var(var);
        let next = self.b.ins().iadd_imm_s(i, 1);
        self.set_var(var, next);
        self.b.ins().jump(head, &[]);
        self.b.switch_to_block(done);
    }

    fn get_var(&mut self, var: Var) -> Value {
        match var {
            Var::Local(id) => self.b.use_var(self.locals[id as usize]),
            Var::State(offset) => self
                .b
                .ins()
                .load(types::I64, FLAGS, self.state, offset as i32),
        }
    }

    fn set_var(&mut self, var: Var, value: Value) {
        match var {
            Var::Local(id) => self.b.def_var(self.locals[id as usize], value),
            Var::State(offset) => {
                self.b.ins().store(FLAGS, value, self.state, offset as i32);
            }
        }
    }

    /// A `def` returns its value; an entry point returns it as a word.
    fn ret(&mut self, value: &crate::hir::Expr) {
        let prim = value.prim;
        let value = self.expr(value);
        if self.ret.is_some() {
            self.flush_faults();
            self.b.ins().return_(&[value]);
            self.after_return();
            return;
        }
        let word = self.cast(value, prim, Prim::U32);
        self.ret_word(word);
    }

    /// Saves resume point `point` and yields until the next cycle.
    fn yield_at(&mut self, point: u32) {
        let Some(seq) = &self.seq else { return };
        let slot = seq.resume_slot as i32;
        let point = self.b.ins().iconst(types::I32, i64::from(point));
        self.b.ins().store(FLAGS, point, self.state, slot);
        self.ret_const(ResumeWord::Running as u32);
    }

    fn now(&mut self) -> Value {
        self.b.ins().load(types::I64, FLAGS, self.cx, offsets::NOW)
    }

    /// Stores `now + micros` in a deadline slot.
    fn set_deadline(&mut self, deadline: u32, micros: &crate::hir::Expr) {
        let micros = self.expr(micros);
        let now = self.now();
        let at = self.b.ins().iadd(now, micros);
        self.b.ins().store(FLAGS, at, self.state, deadline as i32);
    }

    /// Whether `now` has reached a deadline slot.
    fn passed(&mut self, deadline: u32) -> Value {
        let at = self
            .b
            .ins()
            .load(types::I64, FLAGS, self.state, deadline as i32);
        let now = self.now();
        self.b.ins().icmp(IntCC::SignedGreaterThanOrEqual, now, at)
    }

    /// `wait d`.
    fn wait(&mut self, point: u32, deadline: u32, micros: &crate::hir::Expr) {
        let Some(resume) = self.seq.as_ref().map(|s| s.points[point as usize]) else {
            return;
        };
        self.set_deadline(deadline, micros);
        self.yield_at(point);
        let done = self.b.create_block();
        self.b.switch_to_block(resume);
        let passed = self.passed(deadline);
        let pending = self.b.create_block();
        self.b.ins().brif(passed, done, &[], pending, &[]);
        self.b.switch_to_block(pending);
        self.ret_const(ResumeWord::Running as u32);
        self.b.switch_to_block(done);
    }

    /// `wait until cond [timeout d]`. The condition is checked on the cycle
    /// the wait starts, and each cycle after.
    fn wait_until(&mut self, point: u32, cond: &crate::hir::Expr, timeout: Option<&Timeout>) {
        let Some(check) = self.seq.as_ref().map(|s| s.points[point as usize]) else {
            return;
        };
        if let Some(timeout) = timeout {
            self.set_deadline(timeout.deadline, &timeout.micros);
        }
        let point_value = self.b.ins().iconst(types::I32, i64::from(point));
        let slot = self.seq.as_ref().map_or(0, |s| s.resume_slot as i32);
        self.b.ins().store(FLAGS, point_value, self.state, slot);
        self.b.ins().jump(check, &[]);
        self.b.switch_to_block(check);
        let holds = self.expr(cond);
        let (done, waiting) = (self.b.create_block(), self.b.create_block());
        self.b.ins().brif(holds, done, &[], waiting, &[]);
        self.b.switch_to_block(waiting);
        if let Some(timeout) = timeout {
            let passed = self.passed(timeout.deadline);
            let (failing, pending) = (self.b.create_block(), self.b.create_block());
            self.b.ins().brif(passed, failing, &[], pending, &[]);
            self.b.switch_to_block(failing);
            self.fail(timeout.fail);
            self.b.switch_to_block(pending);
        }
        self.ret_const(ResumeWord::Running as u32);
        self.b.switch_to_block(done);
    }

    /// `fail "msg"`: records the message and ends the sequence `Failed`.
    fn fail(&mut self, index: u32) {
        let Some(slot) = self.seq.as_ref().map(|s| s.fail_slot as i32) else {
            return;
        };
        let index = self.b.ins().iconst(types::I32, i64::from(index));
        self.b.ins().store(FLAGS, index, self.state, slot);
        self.ret_const(ResumeWord::Failed as u32);
    }
}
