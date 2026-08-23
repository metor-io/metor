//! Python AST in, typed IR out — the compiler's single validation gate.
//!
//! Everything downstream trusts what leaves this module. Codegen does not
//! re-derive a type, re-check an arity, or guard against a name it cannot
//! resolve; if the checker returned a [`Program`], the program is well typed
//! and every index in it is in range.
//!
//! The subset is small deliberately, and the rejections are as much a part of
//! the design as the acceptances. Strings, containers, classes, closures,
//! imports, `try`, `with`, generators, and global mutation are all refused
//! with a span, because a language that can be checked completely is worth
//! more here than a language that can express everything.
//!
//! ## Where this differs from CPython, on purpose
//!
//! The numerics are a tensor library's, not an interpreter's:
//!
//! - Ints are `i64` and **wrap**. There are no bignums.
//! - `/` is always true division and always yields `f64`, `int / int` included.
//! - `//` and `%` floor, so the sign follows the divisor — but wasm's `rem` is
//!   truncating, so the emitter carries the correction.
//! - Integer division or modulo by zero **traps**. A trap is a contained
//!   diagnostic; manufacturing a value would not be.
//! - `bool` is not an int in disguise. Comparisons yield `bool`, `if` and
//!   `while` conditions must *be* `bool`, and `True + 1` does not typecheck.
//! - Mixed arithmetic promotes `i64` to `f64`. Narrowing is explicit, through
//!   `int(x)`, and truncates toward zero.

use std::collections::HashMap;

use num_traits::ToPrimitive;
use rustpython_parser::ast::{self, Ranged};
use rustpython_parser::{Parse, text_size::TextRange};

use crate::diag::{Diagnostics, Span};
use crate::ir::{Arith, Cmp, Expr, Func, Intrinsic, Num, Program, Stmt};
use crate::{FnSig, Manifest, Ty};

/// How deep an expression may nest before the checker refuses it. Bounded so
/// that a pathological input is a diagnostic rather than a blown stack.
const MAX_DEPTH: u32 = 96;

/// The largest source this crate will look at. A panel expression is tens of
/// bytes and a systems module is a few thousand, so the only thing this
/// excludes is an input designed to be expensive.
const MAX_SOURCE_BYTES: usize = 256 * 1024;

/// Stack for the parse. `rustpython-parser` builds and drops its AST
/// recursively, so nesting depth costs stack — measured to abort a 2 MiB
/// thread somewhere between ten and fifty thousand levels. Bounding the source
/// bounds the depth, and this leaves room for the worst case that bound allows.
const PARSE_STACK_BYTES: usize = 64 * 1024 * 1024;

pub(crate) fn check(source: &str) -> Result<(Program, Manifest), Diagnostics> {
    if source.len() > MAX_SOURCE_BYTES {
        let mut diags = Diagnostics::default();
        let at = MAX_SOURCE_BYTES as u32;
        diags.push(
            Span::new(at, source.len() as u32),
            format!("source is larger than the {MAX_SOURCE_BYTES} byte limit"),
        );
        return Err(diags);
    }

    // `rustpython-parser` is foreign code at this crate's edge, and `compile`
    // owes its callers a diagnostic rather than an unwind for any input at
    // all. Both concerns are met by running the parse somewhere with room and
    // treating a panic as one more way the source can be bad.
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .stack_size(PARSE_STACK_BYTES)
            .spawn_scoped(scope, || check_inner(source))
            .expect("spawning the parse thread")
            .join()
            .unwrap_or_else(|_| {
                let mut diags = Diagnostics::default();
                diags.push(Span::new(0, 0), "the parser could not handle this source");
                Err(diags)
            })
    })
}

fn check_inner(source: &str) -> Result<(Program, Manifest), Diagnostics> {
    let mut diags = Diagnostics::default();

    let module = match ast::Suite::parse(source, "<expr>") {
        Ok(module) => module,
        Err(err) => {
            let at = u32::from(err.offset).min(source.len() as u32);
            diags.push(Span::new(at, at), format!("{}", err.error));
            return Err(diags);
        }
    };

    let mut defs = Vec::new();
    for stmt in &module {
        match stmt {
            ast::Stmt::FunctionDef(def) => defs.push(def),
            other => diags.push(
                other.range(),
                "only `def` is allowed at module level in this phase",
            ),
        }
    }

    let mut sigs: Vec<FnSig> = Vec::new();
    let mut by_name: HashMap<String, u32> = HashMap::new();
    for def in &defs {
        if !def.decorator_list.is_empty() {
            diags.push(def.range, "decorators are not supported in this phase");
        }
        let name = def.name.as_str().to_string();
        let mut params = Vec::new();
        let args = &def.args;
        if !args.posonlyargs.is_empty()
            || !args.kwonlyargs.is_empty()
            || args.vararg.is_some()
            || args.kwarg.is_some()
        {
            diags.push(def.range, "only plain positional parameters are supported");
        }
        for arg in &args.args {
            if arg.default.is_some() {
                diags.push(arg.def.range, "default arguments are not supported");
            }
            let ty = match &arg.def.annotation {
                Some(ann) => annotation(ann, &mut diags),
                None => {
                    diags.push(arg.def.range, "parameters must be annotated");
                    Ty::F64
                }
            };
            params.push((arg.def.arg.as_str().to_string(), ty));
        }
        let ret = match &def.returns {
            Some(ann) => annotation(ann, &mut diags),
            None => {
                diags.push(def.range, "a return type annotation is required");
                Ty::F64
            }
        };
        if by_name.insert(name.clone(), sigs.len() as u32).is_some() {
            diags.push(def.range, format!("`{name}` is defined more than once"));
        }
        sigs.push(FnSig { name, params, ret });
    }

    let mut funcs = Vec::new();
    for (def, sig) in defs.iter().zip(&sigs) {
        let before = diags.len();
        let mut body = FnChecker {
            diags: &mut diags,
            sigs: &sigs,
            by_name: &by_name,
            ret: sig.ret.clone(),
            locals: sig.params.iter().map(|(_, t)| t.clone()).collect(),
            names: sig
                .params
                .iter()
                .enumerate()
                .map(|(i, (n, _))| (n.clone(), i as u32))
                .collect(),
            loop_depth: 0,
        };
        let stmts = body.block(&def.body, 0);
        let complete = always_returns(&stmts);
        let locals = body.locals;
        // Only worth saying when the body itself was understood; otherwise it
        // is noise stacked on the real complaint.
        if !complete && diags.len() == before {
            diags.push(def.range, "not every path through this function returns");
        }
        funcs.push(Func {
            name: sig.name.clone(),
            param_count: sig.params.len(),
            locals,
            body: stmts,
        });
    }

    if diags.is_empty() {
        Ok((Program { funcs }, Manifest { functions: sigs }))
    } else {
        Err(diags.sorted())
    }
}

fn annotation(expr: &ast::Expr, diags: &mut Diagnostics) -> Ty {
    match expr {
        ast::Expr::Name(name) => match name.id.as_str() {
            "f64" | "float" => Ty::F64,
            "i64" | "int" => Ty::I64,
            "bool" => Ty::Bool,
            other => {
                diags.push(
                    expr.range(),
                    format!("unknown type `{other}`; expected f64, i64, or bool"),
                );
                Ty::F64
            }
        },
        _ => {
            diags.push(expr.range(), "expected a type name");
            Ty::F64
        }
    }
}

struct FnChecker<'a> {
    diags: &'a mut Diagnostics,
    sigs: &'a [FnSig],
    by_name: &'a HashMap<String, u32>,
    ret: Ty,
    locals: Vec<Ty>,
    names: HashMap<String, u32>,
    loop_depth: u32,
}

impl FnChecker<'_> {
    fn temp(&mut self, ty: Ty) -> u32 {
        self.locals.push(ty);
        (self.locals.len() - 1) as u32
    }

    fn block(&mut self, stmts: &[ast::Stmt], depth: u32) -> Vec<Stmt> {
        if depth > MAX_DEPTH {
            if let Some(first) = stmts.first() {
                self.diags.push(first.range(), "nested too deeply");
            }
            return Vec::new();
        }
        stmts
            .iter()
            .filter_map(|s| self.stmt(s, depth))
            .flatten()
            .collect()
    }

    fn stmt(&mut self, stmt: &ast::Stmt, depth: u32) -> Option<Vec<Stmt>> {
        match stmt {
            ast::Stmt::Return(ret) => {
                let value = ret.value.as_ref()?;
                let (expr, ty) = self.expr(value, depth)?;
                let expr = self.coerce(expr, ty, &self.ret.clone(), value.range())?;
                Some(vec![Stmt::Return(expr)])
            }
            ast::Stmt::AnnAssign(ann) => {
                let ast::Expr::Name(target) = ann.target.as_ref() else {
                    self.diags
                        .push(ann.target.range(), "only plain names can be assigned");
                    return None;
                };
                let ty = annotation(&ann.annotation, self.diags);
                let value = ann.value.as_ref()?;
                let (expr, got) = self.expr(value, depth)?;
                let expr = self.coerce(expr, got, &ty, value.range())?;
                let slot = self.declare(target.id.as_str(), ty, ann.range);
                Some(vec![Stmt::Assign { local: slot, value: expr }])
            }
            ast::Stmt::Assign(assign) => {
                if assign.targets.len() != 1 {
                    self.diags
                        .push(assign.range, "chained assignment is not supported");
                    return None;
                }
                let ast::Expr::Name(target) = &assign.targets[0] else {
                    self.diags
                        .push(assign.targets[0].range(), "only plain names can be assigned");
                    return None;
                };
                let (expr, got) = self.expr(&assign.value, depth)?;
                let name = target.id.as_str();
                match self.names.get(name).copied() {
                    Some(slot) => {
                        let want = self.locals[slot as usize].clone();
                        let expr = self.coerce(expr, got, &want, assign.value.range())?;
                        Some(vec![Stmt::Assign { local: slot, value: expr }])
                    }
                    None => {
                        let slot = self.declare(name, got, assign.range);
                        Some(vec![Stmt::Assign { local: slot, value: expr }])
                    }
                }
            }
            ast::Stmt::AugAssign(aug) => {
                let ast::Expr::Name(target) = aug.target.as_ref() else {
                    self.diags
                        .push(aug.target.range(), "only plain names can be assigned");
                    return None;
                };
                let name = target.id.as_str();
                let Some(slot) = self.names.get(name).copied() else {
                    self.diags
                        .push(aug.target.range(), format!("`{name}` is not defined"));
                    return None;
                };
                let want = self.locals[slot as usize].clone();
                let lhs = (Expr::Local(slot), want.clone());
                let rhs = self.expr(&aug.value, depth)?;
                let (expr, got) = self.binop(aug.op, lhs, rhs, &aug.value, aug.range)?;
                let expr = self.coerce(expr, got, &want, aug.range)?;
                Some(vec![Stmt::Assign { local: slot, value: expr }])
            }
            ast::Stmt::If(branch) => {
                let cond = self.condition(&branch.test, depth)?;
                let then = self.block(&branch.body, depth + 1);
                let els = self.block(&branch.orelse, depth + 1);
                Some(vec![Stmt::If { cond, then, els }])
            }
            ast::Stmt::While(loop_) => {
                if !loop_.orelse.is_empty() {
                    self.diags
                        .push(loop_.range, "`while ... else` is not supported");
                }
                let cond = self.condition(&loop_.test, depth)?;
                self.loop_depth += 1;
                let body = self.block(&loop_.body, depth + 1);
                self.loop_depth -= 1;
                Some(vec![Stmt::While { cond, body }])
            }
            ast::Stmt::Break(b) => {
                if self.loop_depth == 0 {
                    self.diags.push(b.range, "`break` outside a loop");
                    return None;
                }
                Some(vec![Stmt::Break])
            }
            ast::Stmt::Continue(c) => {
                if self.loop_depth == 0 {
                    self.diags.push(c.range, "`continue` outside a loop");
                    return None;
                }
                Some(vec![Stmt::Continue])
            }
            ast::Stmt::Pass(_) => Some(Vec::new()),
            ast::Stmt::Expr(e) => {
                let (expr, _) = self.expr(&e.value, depth)?;
                Some(vec![Stmt::Drop(expr)])
            }
            other => {
                self.diags.push(other.range(), refusal(other));
                None
            }
        }
    }

    fn declare(&mut self, name: &str, ty: Ty, range: TextRange) -> u32 {
        let _ = range;
        let slot = self.temp(ty);
        self.names.insert(name.to_string(), slot);
        slot
    }

    fn condition(&mut self, expr: &ast::Expr, depth: u32) -> Option<Expr> {
        let (lowered, ty) = self.expr(expr, depth)?;
        if ty != Ty::Bool {
            self.diags.push(
                expr.range(),
                format!("a condition must be bool, not {ty}; write an explicit comparison"),
            );
            return None;
        }
        Some(lowered)
    }

    fn coerce(&mut self, expr: Expr, from: Ty, to: &Ty, range: TextRange) -> Option<Expr> {
        if from == *to {
            return Some(expr);
        }
        if from == Ty::I64 && *to == Ty::F64 {
            return Some(Expr::Intrinsic {
                op: Intrinsic::IntToFloat,
                args: vec![expr],
            });
        }
        let hint = if from == Ty::F64 && *to == Ty::I64 {
            "; f64 narrows to i64 only through `int(x)`"
        } else {
            ""
        };
        self.diags
            .push(range, format!("expected {to}, found {from}{hint}"));
        None
    }

    /// Bring two numeric operands to a common type, promoting `i64` to `f64`.
    fn unify(
        &mut self,
        lhs: (Expr, Ty),
        rhs: (Expr, Ty),
        range: TextRange,
    ) -> Option<(Expr, Expr, Num)> {
        match (&lhs.1, &rhs.1) {
            (Ty::I64, Ty::I64) => Some((lhs.0, rhs.0, Num::I64)),
            (Ty::F64, Ty::F64) => Some((lhs.0, rhs.0, Num::F64)),
            (Ty::I64, Ty::F64) => Some((
                Expr::Intrinsic {
                    op: Intrinsic::IntToFloat,
                    args: vec![lhs.0],
                },
                rhs.0,
                Num::F64,
            )),
            (Ty::F64, Ty::I64) => Some((
                lhs.0,
                Expr::Intrinsic {
                    op: Intrinsic::IntToFloat,
                    args: vec![rhs.0],
                },
                Num::F64,
            )),
            _ => {
                self.diags.push(
                    range,
                    format!(
                        "arithmetic needs numbers, found {} and {}",
                        lhs.1, rhs.1
                    ),
                );
                None
            }
        }
    }

    fn as_f64(&mut self, expr: Expr, ty: Ty, range: TextRange) -> Option<Expr> {
        self.coerce(expr, ty, &Ty::F64, range)
    }

    fn binop(
        &mut self,
        op: ast::Operator,
        lhs: (Expr, Ty),
        rhs: (Expr, Ty),
        rhs_ast: &ast::Expr,
        range: TextRange,
    ) -> Option<(Expr, Ty)> {
        let arith = match op {
            ast::Operator::Add => Arith::Add,
            ast::Operator::Sub => Arith::Sub,
            ast::Operator::Mult => Arith::Mul,
            ast::Operator::FloorDiv => Arith::FloorDiv,
            ast::Operator::Mod => {
                let (l, r, ty) = self.unify(lhs, rhs, range)?;
                return Some(match ty {
                    Num::F64 => (
                        Expr::Kernel {
                            name: "fmod_floor",
                            args: vec![l, r],
                        },
                        Ty::F64,
                    ),
                    Num::I64 => (
                        Expr::Arith {
                            op: Arith::Rem,
                            ty,
                            lhs: Box::new(l),
                            rhs: Box::new(r),
                        },
                        Ty::I64,
                    ),
                });
            }
            ast::Operator::Div => {
                let l = self.as_f64(lhs.0, lhs.1, range)?;
                let r = self.as_f64(rhs.0, rhs.1, range)?;
                return Some((
                    Expr::Arith {
                        op: Arith::Div,
                        ty: Num::F64,
                        lhs: Box::new(l),
                        rhs: Box::new(r),
                    },
                    Ty::F64,
                ));
            }
            ast::Operator::Pow => return self.pow(lhs, rhs, rhs_ast, range),
            ast::Operator::MatMult => {
                self.diags.push(range, "`@` is not supported in this phase");
                return None;
            }
            ast::Operator::LShift
            | ast::Operator::RShift
            | ast::Operator::BitOr
            | ast::Operator::BitXor
            | ast::Operator::BitAnd => {
                self.diags
                    .push(range, "bitwise operators are not supported in this phase");
                return None;
            }
        };
        let (l, r, ty) = self.unify(lhs, rhs, range)?;
        Some((
            Expr::Arith {
                op: arith,
                ty,
                lhs: Box::new(l),
                rhs: Box::new(r),
            },
            match ty {
                Num::F64 => Ty::F64,
                Num::I64 => Ty::I64,
            },
        ))
    }

    /// `**` is repeated multiplication when the exponent is a small
    /// non-negative integer literal, and `pow` from the prelude otherwise.
    fn pow(
        &mut self,
        lhs: (Expr, Ty),
        rhs: (Expr, Ty),
        rhs_ast: &ast::Expr,
        range: TextRange,
    ) -> Option<(Expr, Ty)> {
        if let ast::Expr::Constant(c) = rhs_ast
            && let ast::Constant::Int(n) = &c.value
            && let Some(n) = n.to_u32()
            && n <= 16
        {
            let ty = match lhs.1 {
                Ty::F64 => Num::F64,
                Ty::I64 => Num::I64,
                other => {
                    self.diags
                        .push(range, format!("`**` needs a number, found {other}"));
                    return None;
                }
            };
            return Some((
                Expr::PowConst {
                    ty,
                    base: Box::new(lhs.0),
                    exp: n,
                },
                lhs.1,
            ));
        }
        let base = self.as_f64(lhs.0, lhs.1, range)?;
        let exp = self.as_f64(rhs.0, rhs.1, range)?;
        Some((
            Expr::Kernel {
                name: "pow",
                args: vec![base, exp],
            },
            Ty::F64,
        ))
    }

    fn expr(&mut self, expr: &ast::Expr, depth: u32) -> Option<(Expr, Ty)> {
        if depth > MAX_DEPTH {
            self.diags.push(expr.range(), "nested too deeply");
            return None;
        }
        let depth = depth + 1;
        match expr {
            ast::Expr::Constant(c) => match &c.value {
                ast::Constant::Float(f) => Some((Expr::F64(*f), Ty::F64)),
                ast::Constant::Bool(b) => Some((Expr::Bool(*b), Ty::Bool)),
                ast::Constant::Int(i) => match i.to_i64() {
                    Some(v) => Some((Expr::I64(v), Ty::I64)),
                    None => {
                        self.diags.push(
                            c.range,
                            "integer literal does not fit in i64; there are no bignums here",
                        );
                        None
                    }
                },
                _ => {
                    self.diags
                        .push(c.range, "only numeric and bool literals are supported");
                    None
                }
            },
            ast::Expr::Name(name) => match self.names.get(name.id.as_str()).copied() {
                Some(slot) => Some((Expr::Local(slot), self.locals[slot as usize].clone())),
                None => {
                    self.diags
                        .push(name.range, format!("`{}` is not defined", name.id.as_str()));
                    None
                }
            },
            ast::Expr::BinOp(b) => {
                let lhs = self.expr(&b.left, depth)?;
                let rhs = self.expr(&b.right, depth)?;
                self.binop(b.op, lhs, rhs, &b.right, b.range)
            }
            ast::Expr::UnaryOp(u) => {
                let (operand, ty) = self.expr(&u.operand, depth)?;
                match u.op {
                    ast::UnaryOp::Not => {
                        if ty != Ty::Bool {
                            self.diags
                                .push(u.range, format!("`not` needs a bool, found {ty}"));
                            return None;
                        }
                        Some((Expr::Not(Box::new(operand)), Ty::Bool))
                    }
                    ast::UnaryOp::UAdd => match ty {
                        Ty::F64 | Ty::I64 => Some((operand, ty)),
                        other => {
                            self.diags
                                .push(u.range, format!("unary `+` needs a number, found {other}"));
                            None
                        }
                    },
                    ast::UnaryOp::USub => {
                        let op = match ty {
                            Ty::F64 => Intrinsic::NegF64,
                            Ty::I64 => Intrinsic::NegI64,
                            other => {
                                self.diags.push(
                                    u.range,
                                    format!("unary `-` needs a number, found {other}"),
                                );
                                return None;
                            }
                        };
                        Some((
                            Expr::Intrinsic {
                                op,
                                args: vec![operand],
                            },
                            ty,
                        ))
                    }
                    ast::UnaryOp::Invert => {
                        self.diags
                            .push(u.range, "`~` is not supported in this phase");
                        None
                    }
                }
            }
            ast::Expr::BoolOp(b) => {
                let mut parts = Vec::new();
                for value in &b.values {
                    let (lowered, ty) = self.expr(value, depth)?;
                    if ty != Ty::Bool {
                        self.diags.push(
                            value.range(),
                            format!("`and`/`or` need bools, found {ty}; bool is not an int here"),
                        );
                        return None;
                    }
                    parts.push(lowered);
                }
                let mut iter = parts.into_iter().rev();
                let mut acc = iter.next()?;
                for lhs in iter {
                    acc = match b.op {
                        ast::BoolOp::And => Expr::And(Box::new(lhs), Box::new(acc)),
                        ast::BoolOp::Or => Expr::Or(Box::new(lhs), Box::new(acc)),
                    };
                }
                Some((acc, Ty::Bool))
            }
            ast::Expr::Compare(c) => self.compare(c, depth),
            ast::Expr::IfExp(c) => {
                let cond = self.condition(&c.test, depth)?;
                let (then, then_ty) = self.expr(&c.body, depth)?;
                let (els, els_ty) = self.expr(&c.orelse, depth)?;
                let (then, els, ty) = if then_ty == els_ty {
                    (then, els, then_ty)
                } else {
                    let (l, r, num) = self.unify((then, then_ty), (els, els_ty), c.range)?;
                    (
                        l,
                        r,
                        match num {
                            Num::F64 => Ty::F64,
                            Num::I64 => Ty::I64,
                        },
                    )
                };
                Some((
                    Expr::Select {
                        cond: Box::new(cond),
                        then: Box::new(then),
                        els: Box::new(els),
                        ty: ty.clone(),
                    },
                    ty,
                ))
            }
            ast::Expr::Call(call) => self.call(call, depth),
            other => {
                self.diags.push(other.range(), expr_refusal(other));
                None
            }
        }
    }

    /// Python's chained comparisons, which evaluate each operand once and
    /// short-circuit: `a < b < c` is `a < b and b < c` with one `b`.
    fn compare(&mut self, c: &ast::ExprCompare, depth: u32) -> Option<(Expr, Ty)> {
        let mut operands = Vec::with_capacity(c.comparators.len() + 1);
        operands.push(self.expr(&c.left, depth)?);
        for cmp in &c.comparators {
            operands.push(self.expr(cmp, depth)?);
        }

        if c.ops.len() == 1 {
            let rhs = operands.pop()?;
            let lhs = operands.pop()?;
            return self.one_compare(c.ops[0], lhs, rhs, c.range);
        }

        // Every operand but the last is read twice, so each lands in a slot
        // before the chain that reads it.
        let mut slots = Vec::with_capacity(operands.len());
        for (expr, ty) in &operands {
            let _ = expr;
            slots.push(self.temp(ty.clone()));
        }

        let last = operands.len() - 1;
        let mut acc = {
            let lhs = (Expr::Local(slots[last - 1]), operands[last - 1].1.clone());
            let rhs = (Expr::Local(slots[last]), operands[last].1.clone());
            self.one_compare(c.ops[last - 1], lhs, rhs, c.range)?.0
        };
        for i in (0..last - 1).rev() {
            let lhs = (Expr::Local(slots[i]), operands[i].1.clone());
            let rhs = (Expr::Local(slots[i + 1]), operands[i + 1].1.clone());
            let step = self.one_compare(c.ops[i], lhs, rhs, c.range)?.0;
            acc = Expr::And(Box::new(step), Box::new(acc));
        }

        // Operands after the first two are only reached if the chain gets
        // that far, matching Python's short-circuit.
        let mut built = acc;
        for (i, (expr, _)) in operands.into_iter().enumerate().rev() {
            built = if i < 2 {
                Expr::Store {
                    local: slots[i],
                    value: Box::new(expr),
                    then: Box::new(built),
                }
            } else {
                bind_before(slots[i], expr, built, i)
            };
        }
        Some((built, Ty::Bool))
    }

    fn one_compare(
        &mut self,
        op: ast::CmpOp,
        lhs: (Expr, Ty),
        rhs: (Expr, Ty),
        range: TextRange,
    ) -> Option<(Expr, Ty)> {
        let cmp = match op {
            ast::CmpOp::Eq => Cmp::Eq,
            ast::CmpOp::NotEq => Cmp::Ne,
            ast::CmpOp::Lt => Cmp::Lt,
            ast::CmpOp::LtE => Cmp::Le,
            ast::CmpOp::Gt => Cmp::Gt,
            ast::CmpOp::GtE => Cmp::Ge,
            ast::CmpOp::Is | ast::CmpOp::IsNot => {
                self.diags
                    .push(range, "`is` has no meaning without object identity");
                return None;
            }
            ast::CmpOp::In | ast::CmpOp::NotIn => {
                self.diags
                    .push(range, "`in` is not supported in this phase");
                return None;
            }
        };
        if lhs.1 == Ty::Bool && rhs.1 == Ty::Bool {
            return match cmp {
                Cmp::Eq | Cmp::Ne => Some((
                    Expr::CmpBool {
                        eq: cmp == Cmp::Eq,
                        lhs: Box::new(lhs.0),
                        rhs: Box::new(rhs.0),
                    },
                    Ty::Bool,
                )),
                _ => {
                    self.diags
                        .push(range, "bools compare only with `==` and `!=`");
                    None
                }
            };
        }
        let (l, r, ty) = self.unify(lhs, rhs, range)?;
        Some((
            Expr::Cmp {
                op: cmp,
                ty,
                lhs: Box::new(l),
                rhs: Box::new(r),
            },
            Ty::Bool,
        ))
    }

    fn call(&mut self, call: &ast::ExprCall, depth: u32) -> Option<(Expr, Ty)> {
        if !call.keywords.is_empty() {
            self.diags
                .push(call.range, "keyword arguments are not supported");
            return None;
        }
        let ast::Expr::Name(name) = call.func.as_ref() else {
            self.diags
                .push(call.func.range(), "only named functions can be called");
            return None;
        };
        let name = name.id.as_str();

        if let Some(index) = self.by_name.get(name).copied() {
            let sig = &self.sigs[index as usize];
            if sig.params.len() != call.args.len() {
                let want = sig.params.len();
                let got = call.args.len();
                self.diags.push(
                    call.range,
                    format!("`{name}` takes {want} arguments, found {got}"),
                );
                return None;
            }
            let wanted: Vec<Ty> = sig.params.iter().map(|(_, t)| t.clone()).collect();
            let ret = sig.ret.clone();
            let mut args = Vec::with_capacity(wanted.len());
            for (arg, want) in call.args.iter().zip(&wanted) {
                let (lowered, got) = self.expr(arg, depth)?;
                args.push(self.coerce(lowered, got, want, arg.range())?);
            }
            return Some((Expr::Call { index, args }, ret));
        }

        self.builtin(name, call, depth)
    }

    fn builtin(&mut self, name: &str, call: &ast::ExprCall, depth: u32) -> Option<(Expr, Ty)> {
        let arity = |n: usize, this: &mut Self| -> bool {
            if call.args.len() == n {
                true
            } else {
                this.diags.push(
                    call.range,
                    format!("`{name}` takes {n} arguments, found {}", call.args.len()),
                );
                false
            }
        };

        if let Some(kernel) = transcendental(name) {
            if !arity(1, self) {
                return None;
            }
            let (arg, ty) = self.expr(&call.args[0], depth)?;
            let arg = self.as_f64(arg, ty, call.args[0].range())?;
            return Some((
                Expr::Kernel {
                    name: kernel,
                    args: vec![arg],
                },
                Ty::F64,
            ));
        }

        let unary_native = |name: &str| match name {
            "sqrt" => Some(Intrinsic::SqrtF64),
            "floor" => Some(Intrinsic::FloorF64),
            "ceil" => Some(Intrinsic::CeilF64),
            "trunc" => Some(Intrinsic::TruncF64),
            "round" => Some(Intrinsic::NearestF64),
            _ => None,
        };
        if let Some(op) = unary_native(name) {
            if !arity(1, self) {
                return None;
            }
            let (arg, ty) = self.expr(&call.args[0], depth)?;
            let arg = self.as_f64(arg, ty, call.args[0].range())?;
            return Some((
                Expr::Intrinsic {
                    op,
                    args: vec![arg],
                },
                Ty::F64,
            ));
        }

        match name {
            "atan2" | "pow" => {
                if !arity(2, self) {
                    return None;
                }
                let (a, at) = self.expr(&call.args[0], depth)?;
                let a = self.as_f64(a, at, call.args[0].range())?;
                let (b, bt) = self.expr(&call.args[1], depth)?;
                let b = self.as_f64(b, bt, call.args[1].range())?;
                let kernel = if name == "atan2" { "atan2" } else { "pow" };
                Some((
                    Expr::Kernel {
                        name: kernel,
                        args: vec![a, b],
                    },
                    Ty::F64,
                ))
            }
            "abs" => {
                if !arity(1, self) {
                    return None;
                }
                let (arg, ty) = self.expr(&call.args[0], depth)?;
                let op = match ty {
                    Ty::F64 => Intrinsic::AbsF64,
                    Ty::I64 => Intrinsic::AbsI64,
                    other => {
                        self.diags
                            .push(call.range, format!("`abs` needs a number, found {other}"));
                        return None;
                    }
                };
                Some((
                    Expr::Intrinsic {
                        op,
                        args: vec![arg],
                    },
                    ty,
                ))
            }
            "min" | "max" => {
                if !arity(2, self) {
                    return None;
                }
                let a = self.expr(&call.args[0], depth)?;
                let b = self.expr(&call.args[1], depth)?;
                let (l, r, num) = self.unify(a, b, call.range)?;
                let op = match (name, num) {
                    ("min", Num::F64) => Intrinsic::MinF64,
                    ("max", Num::F64) => Intrinsic::MaxF64,
                    ("min", Num::I64) => Intrinsic::MinI64,
                    _ => Intrinsic::MaxI64,
                };
                let ty = match num {
                    Num::F64 => Ty::F64,
                    Num::I64 => Ty::I64,
                };
                Some((
                    Expr::Intrinsic {
                        op,
                        args: vec![l, r],
                    },
                    ty,
                ))
            }
            "int" => {
                if !arity(1, self) {
                    return None;
                }
                let (arg, ty) = self.expr(&call.args[0], depth)?;
                match ty {
                    Ty::I64 => Some((arg, Ty::I64)),
                    Ty::F64 => Some((
                        Expr::Intrinsic {
                            op: Intrinsic::FloatToInt,
                            args: vec![arg],
                        },
                        Ty::I64,
                    )),
                    other => {
                        self.diags
                            .push(call.range, format!("`int` needs a number, found {other}"));
                        None
                    }
                }
            }
            "float" => {
                if !arity(1, self) {
                    return None;
                }
                let (arg, ty) = self.expr(&call.args[0], depth)?;
                let arg = self.as_f64(arg, ty, call.args[0].range())?;
                Some((arg, Ty::F64))
            }
            other => {
                self.diags
                    .push(call.range, format!("`{other}` is not defined"));
                None
            }
        }
    }
}

/// Wrap `body` so `expr` lands in `local` first, without disturbing the
/// short-circuit structure the chain already has.
fn bind_before(local: u32, expr: Expr, body: Expr, _position: usize) -> Expr {
    Expr::Store {
        local,
        value: Box::new(expr),
        then: Box::new(body),
    }
}

fn transcendental(name: &str) -> Option<&'static str> {
    Some(match name {
        "sin" => "sin",
        "cos" => "cos",
        "tan" => "tan",
        "asin" => "asin",
        "acos" => "acos",
        "atan" => "atan",
        "exp" => "exp",
        "log" => "log",
        "sinh" => "sinh",
        "cosh" => "cosh",
        "tanh" => "tanh",
        _ => return None,
    })
}

fn always_returns(stmts: &[Stmt]) -> bool {
    stmts.iter().any(|s| match s {
        Stmt::Return(_) => true,
        Stmt::If { then, els, .. } => {
            !els.is_empty() && always_returns(then) && always_returns(els)
        }
        _ => false,
    })
}

fn refusal(stmt: &ast::Stmt) -> &'static str {
    match stmt {
        ast::Stmt::FunctionDef(_) | ast::Stmt::AsyncFunctionDef(_) => {
            "nested functions and closures are not supported"
        }
        ast::Stmt::ClassDef(_) => "classes are not supported in this phase",
        ast::Stmt::Delete(_) => "`del` is not supported",
        ast::Stmt::For(_) | ast::Stmt::AsyncFor(_) => "`for` is not supported in this phase",
        ast::Stmt::With(_) | ast::Stmt::AsyncWith(_) => "`with` is not supported",
        ast::Stmt::Match(_) => "`match` is not supported",
        ast::Stmt::Raise(_) => "`raise` is not supported; a fault is a trap here",
        ast::Stmt::Try(_) | ast::Stmt::TryStar(_) => "`try` is not supported",
        ast::Stmt::Assert(_) => "`assert` is not supported",
        ast::Stmt::Import(_) | ast::Stmt::ImportFrom(_) => "imports are not supported",
        ast::Stmt::Global(_) | ast::Stmt::Nonlocal(_) => "globals cannot be mutated",
        ast::Stmt::TypeAlias(_) => "type aliases are not supported",
        _ => "this statement is not supported",
    }
}

fn expr_refusal(expr: &ast::Expr) -> &'static str {
    match expr {
        ast::Expr::Lambda(_) => "lambdas and closures are not supported",
        ast::Expr::Dict(_) | ast::Expr::DictComp(_) => "dicts are not supported",
        ast::Expr::Set(_) | ast::Expr::SetComp(_) => "sets are not supported",
        ast::Expr::List(_) | ast::Expr::ListComp(_) => "lists are not supported",
        ast::Expr::GeneratorExp(_) => "generators are not supported",
        ast::Expr::Await(_) | ast::Expr::Yield(_) | ast::Expr::YieldFrom(_) => {
            "`await` and `yield` are not supported"
        }
        ast::Expr::JoinedStr(_) | ast::Expr::FormattedValue(_) => "strings are not supported",
        ast::Expr::Starred(_) => "argument unpacking is not supported",
        ast::Expr::NamedExpr(_) => "`:=` is not supported",
        ast::Expr::Attribute(_) => "attribute access arrives with frames, in Phase 1",
        ast::Expr::Subscript(_) | ast::Expr::Slice(_) => {
            "indexing arrives with tensors, in Phase 0's M3"
        }
        ast::Expr::Tuple(_) => "tuples are not supported",
        _ => "this expression is not supported",
    }
}
