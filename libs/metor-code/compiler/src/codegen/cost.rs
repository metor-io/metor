//! Walks over lowered bodies: the worst-case operation count of each entry
//! point, and the math functions they call.
//!
//! The count is in HIR operations, not machine instructions: one per
//! expression node and statement, with loops multiplied by their bound and
//! calls charged their callee's count. Loops are bounded and the call graph
//! is acyclic, so it is finite.

use crate::hir::{Def, Expr, ExprKind, Program, Stmt, Type};
use crate::types::Prim;

/// The worst-case count of each `def`, in order. A `def` only calls defs
/// the checker proved acyclic, so each is costed once its callees are.
pub fn def_costs(defs: &[Def]) -> Vec<u64> {
    let mut costs: Vec<Option<u64>> = vec![None; defs.len()];
    for _ in 0..defs.len() {
        for (index, def) in defs.iter().enumerate() {
            if costs[index].is_none() {
                costs[index] = Walk::new(&costs).stmts(&def.body.stmts);
            }
        }
    }
    costs.into_iter().map(|c| c.unwrap_or(u64::MAX)).collect()
}

/// The worst-case count of one call of a type's `step` or `resume`.
pub fn entry_cost(ty: &Type, defs: &[u64]) -> u64 {
    let known: Vec<Option<u64>> = defs.iter().map(|c| Some(*c)).collect();
    let walk = Walk::new(&known);
    let prelude = walk.stmts(&ty.prelude).unwrap_or(u64::MAX);
    let step = walk.stmts(&ty.step.stmts).unwrap_or(u64::MAX);
    prelude.saturating_add(step)
}

/// Every math function a body calls: its symbol, width and arity.
pub fn math_calls(prog: &Program) -> Vec<(&'static str, Prim, usize)> {
    let mut found = Vec::new();
    let bodies = prog
        .types
        .iter()
        .flat_map(|t| {
            [&t.init.stmts, &t.step.stmts, &t.prelude]
                .into_iter()
                .chain(t.redlines.iter().map(|r| &r.stmts))
                .chain(t.on_abort.iter().map(|b| &b.stmts))
        })
        .chain(prog.defs.iter().map(|d| &d.body.stmts));
    for stmts in bodies {
        for stmt in stmts {
            visit_stmt(stmt, &mut |e| {
                if let ExprKind::Math(symbol, args) = &e.kind
                    && !found.iter().any(|(s, _, _)| s == symbol)
                {
                    found.push((*symbol, e.prim, args.len()));
                }
            });
        }
    }
    found
}

struct Walk<'a> {
    defs: &'a [Option<u64>],
}

impl<'a> Walk<'a> {
    fn new(defs: &'a [Option<u64>]) -> Walk<'a> {
        Walk { defs }
    }

    fn stmts(&self, stmts: &[Stmt]) -> Option<u64> {
        stmts.iter().try_fold(0u64, |sum, s| Some(sum.saturating_add(self.stmt(s)?)))
    }

    fn stmt(&self, stmt: &Stmt) -> Option<u64> {
        Some(match stmt {
            Stmt::SetLocal(_, e) | Stmt::Return(e) => 1 + self.expr(e)?,
            Stmt::Store { value, place, .. } => {
                let index = self.exprs(place.index.iter().map(|i| &i.value))?;
                3 + self.expr(value)? + index
            }
            Stmt::Zero { len, .. } | Stmt::Copy { len, .. } => u64::from(len.div_ceil(8)),
            Stmt::Mark(_) | Stmt::Fail(_) => 1,
            Stmt::If { cond, then, orelse } => 1 + self.expr(cond)? + self.stmts(then)?.max(self.stmts(orelse)?),
            Stmt::For { count, body, .. } => (2 + self.stmts(body)?).saturating_mul(u64::from(*count)),
            Stmt::Wait { micros, .. } => 4 + self.expr(micros)?,
            Stmt::WaitUntil { cond, timeout, .. } => {
                let timeout = timeout.as_ref().map_or(Some(0), |t| self.expr(&t.micros))?;
                4 + self.expr(cond)? + timeout
            }
        })
    }

    fn exprs<'e>(&self, exprs: impl Iterator<Item = &'e Expr>) -> Option<u64> {
        exprs.map(|e| self.expr(e)).try_fold(0u64, |sum, c| Some(sum.saturating_add(c?)))
    }

    fn expr(&self, e: &Expr) -> Option<u64> {
        Some(match &e.kind {
            ExprKind::Const(_) | ExprKind::Local(_) | ExprKind::NowMicros | ExprKind::Now | ExprKind::Dt => 1,
            ExprKind::Load(place) => 1 + self.exprs(place.index.iter().map(|i| &i.value))?,
            ExprKind::Neg(x) | ExprKind::Not(x) | ExprKind::Cast(x) => 1 + self.expr(x)?,
            ExprKind::Binary(_, a, b) => 4 + self.expr(a)? + self.expr(b)?,
            ExprKind::Logic { lhs, rhs, .. } => 2 + self.expr(lhs)? + self.expr(rhs)?,
            ExprKind::Call(def, args) => 2 + (*self.defs.get(*def as usize)?)? + self.exprs(args.iter())?,
            ExprKind::Math(_, args) => 50 + self.exprs(args.iter())?,
            ExprKind::Builtin(_, args) => 2 + self.exprs(args.iter())?,
            ExprKind::Stale { age, .. } => 6 + self.expr(age)?,
        })
    }
}

/// Calls `f` on every expression in a statement, outermost first.
fn visit_stmt(stmt: &Stmt, f: &mut impl FnMut(&Expr)) {
    let mut each = |e: &Expr| visit_expr(e, f);
    match stmt {
        Stmt::SetLocal(_, e) | Stmt::Return(e) => each(e),
        Stmt::Store { place, value, .. } => {
            place.index.iter().for_each(|i| each(&i.value));
            each(value);
        }
        Stmt::If { cond, then, orelse } => {
            each(cond);
            then.iter().chain(orelse).for_each(|s| visit_stmt(s, f));
        }
        Stmt::For { body, .. } => body.iter().for_each(|s| visit_stmt(s, f)),
        Stmt::Wait { micros, .. } => each(micros),
        Stmt::WaitUntil { cond, timeout, .. } => {
            each(cond);
            if let Some(t) = timeout {
                visit_expr(&t.micros, f);
            }
        }
        Stmt::Zero { .. } | Stmt::Copy { .. } | Stmt::Mark(_) | Stmt::Fail(_) => {}
    }
}

fn visit_expr(e: &Expr, f: &mut impl FnMut(&Expr)) {
    f(e);
    match &e.kind {
        ExprKind::Load(place) => place.index.iter().for_each(|i| visit_expr(&i.value, f)),
        ExprKind::Neg(x) | ExprKind::Not(x) | ExprKind::Cast(x) => visit_expr(x, f),
        ExprKind::Binary(_, a, b) | ExprKind::Logic { lhs: a, rhs: b, .. } => {
            visit_expr(a, f);
            visit_expr(b, f);
        }
        ExprKind::Call(_, args) | ExprKind::Math(_, args) | ExprKind::Builtin(_, args) => {
            args.iter().for_each(|a| visit_expr(a, f));
        }
        ExprKind::Stale { age, .. } => visit_expr(age, f),
        ExprKind::Const(_) | ExprKind::Local(_) | ExprKind::NowMicros | ExprKind::Now | ExprKind::Dt => {}
    }
}
