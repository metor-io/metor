//! Checking expressions and the places they read.

use crate::ast::{self, ExprKind, UnOp};
use crate::diag::{Diagnostic, Span};
use crate::hir::{self, Base, Builtin, Place};
use crate::types::{ConstVal, Prim, Ty, prim_name, prim_named};

use super::scope::{Ctx, Loc, Scope};
use super::Checker;

/// A resolved place, which may still be an aggregate.
pub struct PlaceRes {
    pub base: Base,
    pub offset: u32,
    pub index: Vec<hir::Index>,
    pub ty: Ty,
    /// The port the place is in: `(true, i)` for input `i`.
    pub port: Option<(bool, usize)>,
}

impl PlaceRes {
    /// The scalar place, if this is one.
    pub fn scalar(self) -> Option<Place> {
        let prim = self.ty.prim()?;
        Some(Place {
            base: self.base,
            offset: self.offset,
            index: self.index,
            prim,
        })
    }
}

/// A stand-in for an expression that failed to check.
fn dummy(want: Option<Prim>) -> hir::Expr {
    let prim = want.unwrap_or(Prim::F64);
    let value = match prim {
        Prim::Bool => ConstVal::Bool(false),
        p if p.is_float() => ConstVal::Float(0.0),
        _ => ConstVal::Int(0),
    };
    hir::Expr::constant(value, prim)
}

fn is_numeric(prim: Prim) -> bool {
    prim != Prim::Bool
}

impl Checker {
    /// Checks `e`, giving untyped constants the type `want`.
    ///
    /// The result may have another type; [`Checker::expect`] checks it.
    pub fn expr(&mut self, sc: &mut Scope, e: &ast::Expr, want: Option<Prim>) -> hir::Expr {
        if self.is_flex(e)
            && let Some(value) = self.const_eval(e)
        {
            return match self.coerce_const(value, want, e.span) {
                Some((value, prim)) => hir::Expr::constant(value, prim),
                None => dummy(want),
            };
        }
        match &e.kind {
            ExprKind::Name(name) => self.name(sc, name, e.span, want),
            ExprKind::Field(..) | ExprKind::Index(..) => self.read(sc, e, want),
            ExprKind::Call { callee, args } => self.call(sc, callee, args, e.span, want),
            ExprKind::Unary(op, x) => self.unary(sc, *op, x, want),
            ExprKind::Binary(op, a, b) => self.binary(sc, *op, a, b, e.span, want),
            ExprKind::Error => dummy(want),
            _ => {
                self.error(e.span, "this expression is not supported here");
                dummy(want)
            }
        }
    }

    /// Checks `e` as a value of type `prim`.
    pub fn expect(&mut self, sc: &mut Scope, e: &ast::Expr, prim: Prim) -> hir::Expr {
        let value = self.expr(sc, e, Some(prim));
        if value.prim != prim {
            self.mismatch(e.span, prim, value.prim);
            return dummy(Some(prim));
        }
        value
    }

    /// Checks a condition, which must be a `bool`.
    pub fn cond(&mut self, sc: &mut Scope, e: &ast::Expr) -> hir::Expr {
        self.expect(sc, e, Prim::Bool)
    }

    pub fn mismatch(&mut self, span: Span, want: Prim, got: Prim) {
        let diag = Diagnostic::new(span, format!("expected `{}`, found `{}`", prim_name(want), prim_name(got)));
        let diag = match (want.is_float(), got.is_float()) {
            (true, false) | (false, true) => diag.with_note(format!("convert it with `{}(…)`", prim_name(want))),
            _ => diag,
        };
        self.diags.push(diag);
    }

    fn name(&mut self, sc: &mut Scope, name: &str, span: Span, want: Option<Prim>) -> hir::Expr {
        if let Some(var) = sc.var(name) {
            return match var.loc {
                Loc::Local(id) => hir::Expr {
                    kind: hir::ExprKind::Local(id),
                    prim: var.prim,
                },
                Loc::State(offset) => hir::Expr::load(Place::at(Base::State, offset, var.prim)),
            };
        }
        if let Some((prim, offset)) = sc.param(name) {
            return hir::Expr::load(Place::at(Base::Params, offset, prim));
        }
        if let Some(c) = self.consts.get(name).copied() {
            return match self.coerce_const(c.value, c.prim.or(want), span) {
                Some((value, prim)) => hir::Expr::constant(value, prim),
                None => dummy(want),
            };
        }
        let time = match name {
            "now" => Some(hir::ExprKind::Now),
            "dt" => Some(hir::ExprKind::Dt),
            _ => None,
        };
        if let Some(kind) = time.filter(|_| sc.ctx != Ctx::Def) {
            return hir::Expr { kind, prim: Prim::F64 };
        }
        let message = match sc.port(name) {
            Some(_) => format!("`{name}` is a frame; read one of its fields, as in `{name}.field`"),
            None => format!("`{name}` is not defined"),
        };
        self.error(span, message);
        dummy(want)
    }

    /// Reads a scalar field or element.
    fn read(&mut self, sc: &mut Scope, e: &ast::Expr, want: Option<Prim>) -> hir::Expr {
        let Some(place) = self.place(sc, e) else {
            return dummy(want);
        };
        match place.scalar() {
            Some(place) => hir::Expr::load(place),
            None => {
                self.error(e.span, "this is an array or frame; read one of its scalars");
                dummy(want)
            }
        }
    }

    /// Resolves a place: a port, a field or element inside one, a state
    /// variable or a param. Reports an error and returns `None` otherwise.
    pub fn place(&mut self, sc: &mut Scope, e: &ast::Expr) -> Option<PlaceRes> {
        match &e.kind {
            ExprKind::Name(name) => self.name_place(sc, name, e.span),
            ExprKind::Field(base, field) => {
                let base = self.place(sc, base)?;
                self.field_place(base, field)
            }
            ExprKind::Index(base, index) => {
                let base = self.place(sc, base)?;
                self.index_place(sc, base, index)
            }
            _ => {
                self.error(e.span, "expected a port, a field or a variable");
                None
            }
        }
    }

    fn name_place(&mut self, sc: &Scope, name: &str, span: Span) -> Option<PlaceRes> {
        let plain = |base, offset, ty| PlaceRes {
            base,
            offset,
            index: Vec::new(),
            ty,
            port: None,
        };
        if let Some((input, index)) = sc.port(name) {
            let port = match input {
                true => &sc.prog.inputs[index],
                false => &sc.prog.outputs[index],
            };
            let base = if input { Base::Input } else { Base::Output };
            return Some(PlaceRes {
                port: Some((input, index)),
                ..plain(base, port.offset, Ty::Frame(port.frame))
            });
        }
        if let Some(var) = sc.var(name)
            && let Loc::State(offset) = var.loc
        {
            return Some(plain(Base::State, offset, Ty::Prim(var.prim)));
        }
        if let Some((prim, offset)) = sc.param(name) {
            return Some(plain(Base::Params, offset, Ty::Prim(prim)));
        }
        self.error(span, format!("`{name}` is not a port or a variable"));
        None
    }

    fn field_place(&mut self, base: PlaceRes, field: &ast::Ident) -> Option<PlaceRes> {
        let Ty::Frame(frame) = base.ty else {
            self.error(field.span, format!("`.{}` reads a field, but this is not a frame", field.name));
            return None;
        };
        let frame = &self.frames[frame];
        let Some(found) = frame.field(&field.name) else {
            let names: Vec<&str> = frame.fields.iter().map(|f| f.name.as_str()).collect();
            let diag = Diagnostic::new(field.span, format!("frame `{}` has no field `{}`", frame.name, field.name))
                .with_note(format!("its fields are: {}", names.join(", ")));
            self.diags.push(diag);
            return None;
        };
        Some(PlaceRes {
            offset: base.offset + found.offset,
            ty: found.ty.clone(),
            ..base
        })
    }

    fn index_place(&mut self, sc: &mut Scope, mut base: PlaceRes, index: &ast::Expr) -> Option<PlaceRes> {
        let Ty::Array(elem, len) = base.ty.clone() else {
            self.error(index.span, "only an array can be indexed");
            return None;
        };
        let (stride, _) = elem.layout(&self.frames);
        match self.const_eval(index) {
            Some(ConstVal::Int(i)) if (0..i128::from(len)).contains(&i) => base.offset += i as u32 * stride,
            Some(ConstVal::Int(i)) => {
                self.error(index.span, format!("index {i} is out of range for an array of {len}"));
                return None;
            }
            Some(_) => {
                self.error(index.span, "an index must be an integer");
                return None;
            }
            None => {
                let value = self.expr(sc, index, Some(Prim::I64));
                if value.prim.is_float() || value.prim == Prim::Bool {
                    self.error(index.span, "an index must be an integer");
                    return None;
                }
                base.index.push(hir::Index { value, stride, len });
            }
        }
        base.ty = *elem;
        Some(base)
    }

    fn unary(&mut self, sc: &mut Scope, op: UnOp, x: &ast::Expr, want: Option<Prim>) -> hir::Expr {
        match op {
            UnOp::Not => {
                let x = self.cond(sc, x);
                hir::Expr {
                    kind: hir::ExprKind::Not(Box::new(x)),
                    prim: Prim::Bool,
                }
            }
            UnOp::Neg => {
                let value = self.expr(sc, x, want);
                if !value.prim.is_float() && !value.prim.is_signed() {
                    self.error(x.span, format!("cannot negate a `{}`", prim_name(value.prim)));
                    return dummy(want);
                }
                hir::Expr {
                    prim: value.prim,
                    kind: hir::ExprKind::Neg(Box::new(value)),
                }
            }
        }
    }

    /// Checks two operands that must share a type. An untyped constant
    /// takes the other operand's type.
    pub fn operands(
        &mut self,
        sc: &mut Scope,
        a: &ast::Expr,
        b: &ast::Expr,
        want: Option<Prim>,
    ) -> Option<(hir::Expr, hir::Expr)> {
        let (l, r) = if self.is_flex(a) && !self.is_flex(b) {
            let r = self.expr(sc, b, want);
            (self.expr(sc, a, Some(r.prim)), r)
        } else {
            let l = self.expr(sc, a, want);
            let r = self.expr(sc, b, Some(l.prim));
            (l, r)
        };
        if l.prim != r.prim {
            let diag = Diagnostic::new(a.span.to(b.span), format!("`{}` and `{}` do not mix", prim_name(l.prim), prim_name(r.prim)))
                .with_note("convert one side, as in `f64(n)`");
            self.diags.push(diag);
            return None;
        }
        Some((l, r))
    }

    fn binary(
        &mut self,
        sc: &mut Scope,
        op: ast::BinOp,
        a: &ast::Expr,
        b: &ast::Expr,
        span: Span,
        want: Option<Prim>,
    ) -> hir::Expr {
        use ast::BinOp as B;
        if op.is_logic() {
            let (lhs, rhs) = (self.cond(sc, a), self.cond(sc, b));
            return hir::Expr {
                kind: hir::ExprKind::Logic {
                    and: op == B::And,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
                prim: Prim::Bool,
            };
        }
        let operand_want = if op.is_comparison() { None } else { want };
        let Some((l, r)) = self.operands(sc, a, b, operand_want) else {
            return dummy(want);
        };
        let prim = l.prim;
        if op.is_comparison() {
            if prim == Prim::Bool && !matches!(op, B::Eq | B::Ne) {
                self.error(span, "bools compare only with `==` and `!=`");
            }
            return hir::Expr {
                kind: hir::ExprKind::Binary(hir_op(op), Box::new(l), Box::new(r)),
                prim: Prim::Bool,
            };
        }
        if !is_numeric(prim) {
            self.error(span, "arithmetic needs numbers, not `bool`");
            return dummy(want);
        }
        if matches!(op, B::Div | B::Rem) && self.const_eval(b).is_some_and(ConstVal::is_zero) {
            let diag = Diagnostic::new(b.span, "division by a constant zero")
                .with_note("integer division by zero at run time yields 0 and logs a fault; by a constant it is an error");
            self.diags.push(diag);
            return dummy(want);
        }
        match (op, prim.is_float()) {
            (B::Pow, true) => math(if prim == Prim::F32 { "mc_rt_powf" } else { "mc_rt_pow" }, vec![l, r], prim),
            (B::Pow, false) => {
                self.error(span, "`**` needs floats; convert with `f64(…)`");
                dummy(want)
            }
            (B::Rem, true) => math(if prim == Prim::F32 { "mc_rt_fmodf" } else { "mc_rt_fmod" }, vec![l, r], prim),
            _ => hir::Expr {
                kind: hir::ExprKind::Binary(hir_op(op), Box::new(l), Box::new(r)),
                prim,
            },
        }
    }

    fn call(&mut self, sc: &mut Scope, callee: &ast::Ident, args: &[ast::Arg], span: Span, want: Option<Prim>) -> hir::Expr {
        if let Some(arg) = args.iter().find_map(|a| a.name.as_ref()) {
            self.error(arg.span, "only a frame takes named arguments, when it is written to an output");
            return dummy(want);
        }
        let args: Vec<&ast::Expr> = args.iter().map(|a| &a.value).collect();
        let name = callee.name.as_str();
        if let Some(prim) = prim_named(name) {
            return self.cast(sc, prim, &args, span);
        }
        if name == "clamp" {
            return self.clamp(sc, &args, span, want);
        }
        if let Some(builtin) = builtin(name) {
            return self.builtin(sc, builtin, &args, span, want);
        }
        if let Some(def) = self.def_named(name) {
            return self.def_call(sc, def, &args, span);
        }
        if name == "stale" {
            return self.stale(sc, &args, span);
        }
        if mc_rt::math::find(name, false).is_some() {
            return self.math_call(sc, name, &args, span, want);
        }
        let message = match self.frame_named(name) {
            Some(_) => format!("a frame is built only as a whole output, as in `out = {name}(…)`"),
            None => format!("`{name}` is not a function"),
        };
        self.error(callee.span, message);
        dummy(want)
    }

    fn arity(&mut self, args: &[&ast::Expr], n: usize, span: Span) -> bool {
        if args.len() != n {
            self.error(span, format!("this takes {n} argument{}", if n == 1 { "" } else { "s" }));
            return false;
        }
        true
    }

    fn cast(&mut self, sc: &mut Scope, prim: Prim, args: &[&ast::Expr], span: Span) -> hir::Expr {
        if !self.arity(args, 1, span) {
            return dummy(Some(prim));
        }
        if self.is_flex(args[0])
            && let Some(value) = self.const_eval(args[0])
        {
            let value = match (value, prim.is_float(), prim) {
                (_, _, Prim::Bool) => value,
                (ConstVal::Float(v), false, _) => ConstVal::Int(saturate(v as i128, prim)),
                (ConstVal::Int(v), false, _) => ConstVal::Int(saturate(v, prim)),
                _ => value,
            };
            return match self.coerce_const(value, Some(prim), span) {
                Some((value, prim)) => hir::Expr::constant(value, prim),
                None => dummy(Some(prim)),
            };
        }
        let value = self.expr(sc, args[0], None);
        if prim == Prim::Bool && value.prim != Prim::Bool {
            let from = prim_name(value.prim);
            self.error(span, format!("cannot convert `{from}` to `bool`; compare it instead, as in `x != 0`"));
            return dummy(Some(prim));
        }
        value.cast(prim)
    }

    fn builtin(&mut self, sc: &mut Scope, builtin: Builtin, args: &[&ast::Expr], span: Span, want: Option<Prim>) -> hir::Expr {
        match builtin {
            Builtin::Min | Builtin::Max => {
                if !self.arity(args, 2, span) {
                    return dummy(want);
                }
                let Some((a, b)) = self.operands(sc, args[0], args[1], want) else {
                    return dummy(want);
                };
                self.numeric_builtin(builtin, vec![a, b], span, want)
            }
            Builtin::Abs => {
                if !self.arity(args, 1, span) {
                    return dummy(want);
                }
                let x = self.expr(sc, args[0], want);
                self.numeric_builtin(builtin, vec![x], span, want)
            }
            _ => {
                if !self.arity(args, 1, span) {
                    return dummy(want);
                }
                let x = self.expr(sc, args[0], want.filter(|p| p.is_float()).or(Some(Prim::F64)));
                if !x.prim.is_float() {
                    self.error(span, "this takes a float");
                    return dummy(want);
                }
                hir::Expr {
                    prim: x.prim,
                    kind: hir::ExprKind::Builtin(builtin, vec![x]),
                }
            }
        }
    }

    fn numeric_builtin(&mut self, builtin: Builtin, args: Vec<hir::Expr>, span: Span, want: Option<Prim>) -> hir::Expr {
        let prim = args[0].prim;
        if !is_numeric(prim) {
            self.error(span, "this takes numbers, not `bool`");
            return dummy(want);
        }
        hir::Expr {
            kind: hir::ExprKind::Builtin(builtin, args),
            prim,
        }
    }

    /// `clamp(x, lo, hi)`, as `min(max(x, lo), hi)`.
    pub fn clamp(&mut self, sc: &mut Scope, args: &[&ast::Expr], span: Span, want: Option<Prim>) -> hir::Expr {
        if !self.arity(args, 3, span) {
            return dummy(want);
        }
        let first = args.iter().position(|a| !self.is_flex(a)).unwrap_or(0);
        let anchor = self.expr(sc, args[first], want);
        let prim = anchor.prim;
        let mut values: Vec<hir::Expr> = Vec::new();
        for (i, arg) in args.iter().enumerate() {
            let value = match i == first {
                true => anchor.clone(),
                false => self.expect(sc, arg, prim),
            };
            values.push(value);
        }
        let hi = values.pop().unwrap_or_else(|| dummy(want));
        let max = self.numeric_builtin(Builtin::Max, values, span, want);
        self.numeric_builtin(Builtin::Min, vec![max, hi], span, want)
    }

    fn def_call(&mut self, sc: &mut Scope, def: u32, args: &[&ast::Expr], span: Span) -> hir::Expr {
        let params = self.defs[def as usize].def.params.clone();
        let ret = self.defs[def as usize].def.ret;
        if !self.arity(args, params.len(), span) {
            return dummy(Some(ret));
        }
        let args = args
            .iter()
            .zip(&params)
            .map(|(arg, prim)| self.expect(sc, arg, *prim))
            .collect();
        if !sc.calls.contains(&def) {
            sc.calls.push(def);
        }
        hir::Expr {
            kind: hir::ExprKind::Call(def, args),
            prim: ret,
        }
    }

    fn stale(&mut self, sc: &mut Scope, args: &[&ast::Expr], span: Span) -> hir::Expr {
        if !self.arity(args, 2, span) {
            return dummy(Some(Prim::Bool));
        }
        let port = match &args[0].kind {
            ExprKind::Name(name) => match sc.port(name) {
                Some((true, index)) => Some(index),
                _ => None,
            },
            _ => None,
        };
        let Some(port) = port else {
            self.error(args[0].span, "`stale` takes an input port, as in `stale(temp, 0.5)`");
            return dummy(Some(Prim::Bool));
        };
        let age = self.expect(sc, args[1], Prim::F64);
        let input = &sc.prog.inputs[port];
        let timestamp = input.offset + self.frames[input.frame].timestamp;
        hir::Expr {
            kind: hir::ExprKind::Stale {
                port: port as u32,
                timestamp,
                age: Box::new(age),
            },
            prim: Prim::Bool,
        }
    }

    fn math_call(&mut self, sc: &mut Scope, name: &str, args: &[&ast::Expr], span: Span, want: Option<Prim>) -> hir::Expr {
        let arity = mc_rt::math::find(name, false).map_or(1, |m| m.arity);
        if !self.arity(args, arity, span) {
            return dummy(want);
        }
        let first = self.expr(sc, args[0], want.filter(|p| p.is_float()).or(Some(Prim::F64)));
        if !first.prim.is_float() {
            self.error(span, format!("`{name}` takes a float"));
            return dummy(want);
        }
        let prim = first.prim;
        let mut values = vec![first];
        values.extend(args[1..].iter().map(|a| self.expect(sc, a, prim)));
        let symbol = mc_rt::math::find(name, prim == Prim::F32).map_or("", |m| m.symbol);
        math(symbol, values, prim)
    }
}

/// Clamps an integer constant to `prim`'s range, as a run-time conversion
/// saturates.
fn saturate(v: i128, prim: Prim) -> i128 {
    let bits = prim.size() * 8;
    let (lo, hi) = match prim.is_signed() {
        true => (-(1i128 << (bits - 1)), (1i128 << (bits - 1)) - 1),
        false => (0, (1i128 << bits) - 1),
    };
    v.clamp(lo, hi)
}

fn math(symbol: &'static str, args: Vec<hir::Expr>, prim: Prim) -> hir::Expr {
    hir::Expr {
        kind: hir::ExprKind::Math(symbol, args),
        prim,
    }
}

fn builtin(name: &str) -> Option<Builtin> {
    Some(match name {
        "abs" => Builtin::Abs,
        "min" => Builtin::Min,
        "max" => Builtin::Max,
        "sqrt" => Builtin::Sqrt,
        "floor" => Builtin::Floor,
        "ceil" => Builtin::Ceil,
        "trunc" => Builtin::Trunc,
        "round" => Builtin::Round,
        _ => return None,
    })
}

fn hir_op(op: ast::BinOp) -> hir::BinOp {
    use ast::BinOp as B;
    match op {
        B::Add => hir::BinOp::Add,
        B::Sub => hir::BinOp::Sub,
        B::Mul => hir::BinOp::Mul,
        B::Div => hir::BinOp::Div,
        B::Rem => hir::BinOp::Rem,
        B::Eq => hir::BinOp::Eq,
        B::Ne => hir::BinOp::Ne,
        B::Lt => hir::BinOp::Lt,
        B::Le => hir::BinOp::Le,
        B::Gt => hir::BinOp::Gt,
        B::Ge => hir::BinOp::Ge,
        B::Pow | B::And | B::Or => hir::BinOp::Eq,
    }
}
