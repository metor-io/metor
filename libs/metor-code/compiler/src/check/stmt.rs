//! Checking statements.

use crate::ast::{self, ExprKind, StmtKind};
use crate::diag::{Diagnostic, Span};
use crate::hir::{self, Base, Place};
use crate::types::{ConstVal, Prim};

use super::Checker;
use super::expr::PlaceRes;
use super::scope::{Ctx, Loc, Scope};

impl Checker {
    pub fn stmts(&mut self, sc: &mut Scope, stmts: &[ast::Stmt]) -> Vec<hir::Stmt> {
        let mut out = Vec::new();
        for stmt in stmts {
            self.stmt(sc, stmt, &mut out);
        }
        out
    }

    fn stmt(&mut self, sc: &mut Scope, stmt: &ast::Stmt, out: &mut Vec<hir::Stmt>) {
        match &stmt.kind {
            StmtKind::Assign {
                target,
                op: None,
                value,
            } => self.assign(sc, target, value, out),
            StmtKind::Assign {
                target,
                op: Some(op),
                value,
            } => {
                let value = ast::Expr {
                    kind: ExprKind::Binary(*op, Box::new(target.clone()), Box::new(value.clone())),
                    span: stmt.span,
                };
                if let ExprKind::Name(name) = &target.kind
                    && sc.var(name).is_none()
                    && sc.param(name).is_none()
                {
                    return self.error(target.span, format!("`{name}` is not defined"));
                }
                self.assign(sc, target, &value, out);
            }
            StmtKind::Declare { name, ty, value } => self.declare(sc, name, ty, value, out),
            StmtKind::If { branches, orelse } => {
                if let Some(stmt) = self.if_chain(sc, branches, orelse.as_deref()) {
                    out.push(stmt);
                }
            }
            StmtKind::For { var, count, body } => self.for_loop(sc, var, count, body, out),
            StmtKind::Return(value) => self.ret(sc, value, stmt.span, out),
            StmtKind::Pass | StmtKind::Error => {}
            StmtKind::Wait(duration) => self.wait(sc, duration, stmt.span, out),
            StmtKind::WaitUntil { cond, timeout } => {
                self.wait_until(sc, cond, timeout.as_ref(), stmt.span, out)
            }
            StmtKind::Fail(message) => {
                if self.in_steps(sc, stmt.span, "`fail`") {
                    out.push(hir::Stmt::Fail(sc.prog.fail(message)));
                }
            }
        }
    }

    /// Whether the statement is in a sequence's steps, reporting it if not.
    fn in_steps(&mut self, sc: &Scope, span: Span, what: &str) -> bool {
        if sc.ctx == Ctx::SeqBody {
            return true;
        }
        self.error(span, format!("{what} belongs in a sequence's steps"));
        false
    }

    fn assign(
        &mut self,
        sc: &mut Scope,
        target: &ast::Expr,
        value: &ast::Expr,
        out: &mut Vec<hir::Stmt>,
    ) {
        let ExprKind::Name(name) = &target.kind else {
            if let Some(place) = self.place(sc, target) {
                self.store(sc, place, value, target.span, out);
            }
            return;
        };
        if sc.port(name).is_some() {
            if let Some(place) = self.place(sc, target) {
                self.frame_write(sc, place, value, target.span, out);
            }
            return;
        }
        if let Some(var) = sc.var(name).cloned() {
            let value = self.assigned(sc, value, var.prim);
            out.push(match var.loc {
                Loc::Local(id) => hir::Stmt::SetLocal(id, value),
                Loc::State(offset) => store(Place::at(Base::State, offset, var.prim), value, None),
            });
            return;
        }
        if sc.param(name).is_some() || self.consts.contains_key(name) {
            return self.error(target.span, format!("`{name}` is read-only"));
        }
        if matches!(name.as_str(), "now" | "dt") {
            return self.error(target.span, format!("`{name}` is read-only"));
        }
        let value = self.expr(sc, value, None);
        out.push(match sc.new_var(name, value.prim) {
            Loc::Local(id) => hir::Stmt::SetLocal(id, value),
            Loc::State(offset) => store(Place::at(Base::State, offset, value.prim), value, None),
        });
    }

    /// Checks a value stored into a `prim`. A `bool` stores into an integer
    /// as 0 or 1.
    fn assigned(&mut self, sc: &mut Scope, value: &ast::Expr, prim: Prim) -> hir::Expr {
        let integer = prim != Prim::Bool && !prim.is_float();
        if integer
            && self.is_flex(value)
            && let Some(ConstVal::Bool(bit)) = self.const_eval(value)
        {
            return hir::Expr::constant(ConstVal::Int(i128::from(bit)), prim);
        }
        let checked = self.expr(sc, value, Some(prim));
        match (checked.prim, prim) {
            (got, want) if got == want => checked,
            (Prim::Bool, want) if !want.is_float() => checked.cast(want),
            (got, want) => {
                self.mismatch(value.span, want, got);
                hir::Expr::constant(ConstVal::Int(0), want)
            }
        }
    }

    fn declare(
        &mut self,
        sc: &mut Scope,
        name: &ast::Ident,
        ty: &ast::TypeExpr,
        value: &ast::Expr,
        out: &mut Vec<hir::Stmt>,
    ) {
        let Some(prim) = self.scalar_type(ty) else {
            return;
        };
        if sc.var(&name.name).is_some() {
            return self.error(name.span, format!("`{}` is already defined", name.name));
        }
        let value = self.assigned(sc, value, prim);
        out.push(match sc.new_var(&name.name, prim) {
            Loc::Local(id) => hir::Stmt::SetLocal(id, value),
            Loc::State(offset) => store(Place::at(Base::State, offset, prim), value, None),
        });
    }

    /// Stores into a scalar place.
    fn store(
        &mut self,
        sc: &mut Scope,
        place: PlaceRes,
        value: &ast::Expr,
        span: Span,
        out: &mut Vec<hir::Stmt>,
    ) {
        let port = place.port;
        let Some(place) = place.scalar() else {
            return self.error(span, "only a scalar can be assigned; assign each element");
        };
        let mark = match (place.base, port) {
            (Base::Input, _) => return self.error(span, "inputs are read-only"),
            (Base::Params, _) => return self.error(span, "params are read-only"),
            (Base::Output, Some((false, index))) => Some(index as u32),
            _ => None,
        };
        let value = self.assigned(sc, value, place.prim);
        if let Some(index) = mark {
            sc.writes.insert(index);
        }
        out.push(store(place, value, mark));
    }

    /// Writes a whole output frame: `out = Frame(field = v, …)` or
    /// `out = other_port`.
    fn frame_write(
        &mut self,
        sc: &mut Scope,
        place: PlaceRes,
        value: &ast::Expr,
        span: Span,
        out: &mut Vec<hir::Stmt>,
    ) {
        let Some((false, index)) = place.port else {
            return self.error(span, "inputs are read-only");
        };
        let frame = sc.prog.outputs[index].frame;
        let (offset, size) = (place.offset, self.frames[frame].size);
        sc.writes.insert(index as u32);
        let mark = Some(index as u32);
        match &value.kind {
            ExprKind::Call { callee, args } if self.frame_named(&callee.name) == Some(frame) => {
                out.push(hir::Stmt::Zero {
                    base: Base::Output,
                    offset,
                    len: size,
                });
                for arg in args {
                    self.field_arg(sc, frame, offset, index, arg, out);
                }
                out.push(hir::Stmt::Mark(index as u32));
            }
            ExprKind::Name(name) if sc.port(name).is_some() => {
                let Some(src) = self.place(sc, value) else {
                    return;
                };
                if src.ty != crate::types::Ty::Frame(frame) {
                    return self.error(value.span, "this port carries another frame");
                }
                out.push(hir::Stmt::Copy {
                    dst: (Base::Output, offset),
                    src: (src.base, src.offset),
                    len: size,
                    mark,
                });
            }
            _ => {
                let name = &self.frames[frame].name;
                let diag = Diagnostic::new(
                    value.span,
                    format!("an output is written as a whole frame, as in `{name}(field = …)`"),
                )
                .with_note("or write one field, as in `out.field = …`");
                self.diags.push(diag);
            }
        }
    }

    /// Stores one `field = value` of a frame being built.
    fn field_arg(
        &mut self,
        sc: &mut Scope,
        frame: usize,
        offset: u32,
        port: usize,
        arg: &ast::Arg,
        out: &mut Vec<hir::Stmt>,
    ) {
        let Some(name) = &arg.name else {
            return self.error(
                arg.value.span,
                "name each field, as in `Frame(field = value)`",
            );
        };
        let Some(field) = self.frames[frame].field(&name.name).cloned() else {
            let frame = &self.frames[frame].name;
            return self.error(
                name.span,
                format!("frame `{frame}` has no field `{}`", name.name),
            );
        };
        let Some(prim) = field.ty.prim() else {
            return self.error(
                name.span,
                "only scalar fields can be set here; set elements after",
            );
        };
        let value = self.assigned(sc, &arg.value, prim);
        let place = Place::at(Base::Output, offset + field.offset, prim);
        out.push(store(place, value, Some(port as u32)));
    }

    fn if_chain(
        &mut self,
        sc: &mut Scope,
        branches: &[(ast::Expr, Vec<ast::Stmt>)],
        orelse: Option<&[ast::Stmt]>,
    ) -> Option<hir::Stmt> {
        let ((cond, body), rest) = branches.split_first()?;
        let cond = self.cond(sc, cond);
        let then = self.stmts(sc, body);
        let orelse = match rest.is_empty() {
            true => orelse.map(|s| self.stmts(sc, s)).unwrap_or_default(),
            false => self.if_chain(sc, rest, orelse).into_iter().collect(),
        };
        Some(hir::Stmt::If { cond, then, orelse })
    }

    fn for_loop(
        &mut self,
        sc: &mut Scope,
        var: &ast::Ident,
        count: &ast::Expr,
        body: &[ast::Stmt],
        out: &mut Vec<hir::Stmt>,
    ) {
        let Some(count) = self.const_count(count, count.span) else {
            return;
        };
        let var = match sc.var(&var.name).cloned() {
            Some(existing) if existing.prim == Prim::I64 => existing.loc,
            Some(_) => {
                return self.error(
                    var.span,
                    format!("`{}` is already a variable of another type", var.name),
                );
            }
            None => sc.new_var(&var.name, Prim::I64),
        };
        let body = self.stmts(sc, body);
        let var = match var {
            Loc::Local(id) => hir::Var::Local(id),
            Loc::State(offset) => hir::Var::State(offset),
        };
        out.push(hir::Stmt::For { var, count, body });
    }

    fn ret(&mut self, sc: &mut Scope, value: &ast::Expr, span: Span, out: &mut Vec<hir::Stmt>) {
        let Some(ret) = sc.ret else {
            return self.error(span, "`return` belongs in a `def`");
        };
        let value = self.expect(sc, value, ret);
        out.push(hir::Stmt::Return(value));
    }

    /// `wait d`: a resume point that waits `d` seconds of cycle time.
    fn wait(&mut self, sc: &mut Scope, duration: &ast::Expr, span: Span, out: &mut Vec<hir::Stmt>) {
        if !self.in_steps(sc, span, "`wait`") {
            return;
        }
        let micros = self.micros(sc, duration);
        let deadline = sc.prog.alloc(8);
        let point = sc.prog.next_point();
        out.push(hir::Stmt::Wait {
            point,
            deadline,
            micros,
        });
    }

    /// `wait until c [timeout d]`: a resume point that waits for `c`.
    fn wait_until(
        &mut self,
        sc: &mut Scope,
        cond: &ast::Expr,
        timeout: Option<&ast::Expr>,
        span: Span,
        out: &mut Vec<hir::Stmt>,
    ) {
        if !self.in_steps(sc, span, "`wait until`") {
            return;
        }
        let checked = self.cond(sc, cond);
        let timeout = timeout.map(|duration| {
            let micros = self.micros(sc, duration);
            let text = self
                .src
                .get(cond.span.range())
                .unwrap_or("its condition")
                .to_string();
            hir::Timeout {
                deadline: sc.prog.alloc(8),
                micros,
                fail: sc.prog.fail(&format!("timed out waiting until {text}")),
            }
        });
        let point = sc.prog.next_point();
        out.push(hir::Stmt::WaitUntil {
            point,
            cond: checked,
            timeout,
        });
    }

    /// Checks a duration in seconds and converts it to whole microseconds.
    fn micros(&mut self, sc: &mut Scope, duration: &ast::Expr) -> hir::Expr {
        if let Some(value) = self.const_eval(duration).filter(|_| self.is_flex(duration)) {
            let micros = (value.as_f64() * 1e6).round();
            if !(0.0..=i64::MAX as f64).contains(&micros) {
                self.error(duration.span, "a duration must be zero or more seconds");
            }
            return hir::Expr::constant(ConstVal::Int(micros as i128), Prim::I64);
        }
        let seconds = self.expect(sc, duration, Prim::F64);
        let scaled = hir::Expr {
            kind: hir::ExprKind::Binary(
                hir::BinOp::Mul,
                Box::new(seconds),
                Box::new(hir::Expr::constant(ConstVal::Float(1e6), Prim::F64)),
            ),
            prim: Prim::F64,
        };
        scaled.cast(Prim::I64)
    }
}

/// A store, guarding floats written to an output against NaN.
fn store(place: Place, value: hir::Expr, mark: Option<u32>) -> hir::Stmt {
    hir::Stmt::Store {
        nan_guard: mark.is_some() && place.prim.is_float(),
        place,
        value,
        mark,
    }
}
