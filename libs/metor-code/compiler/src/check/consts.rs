//! Compile-time constants: `const` declarations, literals, and the values
//! array lengths and loop bounds need.

use crate::ast::{self, BinOp, ExprKind, UnOp};
use crate::diag::Span;
use crate::types::{ConstVal, Prim, prim_name};

use super::{Checker, Const};

impl Checker {
    /// Checks `const NAME [: T] = value`.
    pub(super) fn constant(&mut self, decl: &ast::ConstDecl) {
        if self.consts.contains_key(&decl.name.name) {
            self.error(decl.name.span, format!("`{}` is defined twice", decl.name.name));
        }
        let Some(value) = self.const_eval(&decl.value) else {
            self.error(decl.value.span, "a constant's value must be known at compile time");
            return;
        };
        let prim = match &decl.ty {
            Some(ty) => self.scalar_type(ty),
            None => None,
        };
        let value = match prim {
            Some(prim) => match self.coerce_const(value, Some(prim), decl.value.span) {
                Some((value, _)) => value,
                None => return,
            },
            None => value,
        };
        self.consts.insert(decl.name.name.clone(), Const { value, prim });
    }

    /// Evaluates an expression of literals and constants, or returns `None`
    /// if it is not one.
    pub fn const_eval(&self, e: &ast::Expr) -> Option<ConstVal> {
        match &e.kind {
            ExprKind::Int(v) => Some(ConstVal::Int(i128::from(*v))),
            ExprKind::Float(v) | ExprKind::Duration(v) => Some(ConstVal::Float(*v)),
            ExprKind::Bool(v) => Some(ConstVal::Bool(*v)),
            ExprKind::Name(name) => self.consts.get(name).map(|c| c.value),
            ExprKind::Unary(op, x) => unary(*op, self.const_eval(x)?),
            ExprKind::Binary(op, a, b) => binary(*op, self.const_eval(a)?, self.const_eval(b)?),
            _ => None,
        }
    }

    /// Whether `e` is a constant with no declared type, which takes the type
    /// its use needs.
    pub fn is_flex(&self, e: &ast::Expr) -> bool {
        match &e.kind {
            ExprKind::Int(_) | ExprKind::Float(_) | ExprKind::Duration(_) | ExprKind::Bool(_) => true,
            ExprKind::Name(name) => self.consts.get(name).is_some_and(|c| c.prim.is_none()),
            ExprKind::Unary(_, x) => self.is_flex(x),
            ExprKind::Binary(_, a, b) => self.is_flex(a) && self.is_flex(b) && self.const_eval(e).is_some(),
            _ => false,
        }
    }

    /// Gives a constant the type `want`, or its own default type.
    pub fn coerce_const(&mut self, value: ConstVal, want: Option<Prim>, span: Span) -> Option<(ConstVal, Prim)> {
        let prim = match (want, value) {
            (Some(prim), _) => prim,
            (None, ConstVal::Int(_)) => Prim::I64,
            (None, ConstVal::Float(_)) => Prim::F64,
            (None, ConstVal::Bool(_)) => Prim::Bool,
        };
        let coerced = match (prim, value) {
            (Prim::Bool, ConstVal::Bool(_)) => Some(value),
            (p, ConstVal::Int(v)) if p.is_float() => Some(ConstVal::Float(v as f64)),
            (p, ConstVal::Float(_)) if p.is_float() => Some(value),
            (p, ConstVal::Int(v)) if p != Prim::Bool => fits(v, p).then_some(value),
            _ => None,
        };
        if coerced.is_none() {
            let what = match value {
                ConstVal::Int(v) => format!("integer {v}"),
                ConstVal::Float(v) => format!("float {v}"),
                ConstVal::Bool(v) => format!("bool {}", if v { "True" } else { "False" }),
            };
            self.error(span, format!("the {what} is not a valid `{}`", prim_name(prim)));
        }
        Some((coerced?, prim))
    }

    /// Evaluates a constant count, such as an array length or `range(N)`.
    pub fn const_count(&mut self, e: &ast::Expr, span: Span) -> Option<u32> {
        match self.const_eval(e) {
            Some(ConstVal::Int(v)) if (0..=i128::from(u32::MAX)).contains(&v) => Some(v as u32),
            Some(_) => {
                self.error(span, "a count must be a non-negative integer");
                None
            }
            None => {
                let diag = crate::diag::Diagnostic::new(e.span, "a count must be a constant")
                    .with_note("bounds are fixed at compile time, so every loop finishes");
                self.diags.push(diag);
                None
            }
        }
    }
}

/// Whether integer `v` fits in `prim`.
fn fits(v: i128, prim: Prim) -> bool {
    let (lo, hi): (i128, i128) = match prim {
        Prim::U8 => (0, u8::MAX.into()),
        Prim::U16 => (0, u16::MAX.into()),
        Prim::U32 => (0, u32::MAX.into()),
        Prim::U64 => (0, u64::MAX.into()),
        Prim::I8 => (i8::MIN.into(), i8::MAX.into()),
        Prim::I16 => (i16::MIN.into(), i16::MAX.into()),
        Prim::I32 => (i32::MIN.into(), i32::MAX.into()),
        Prim::I64 => (i64::MIN.into(), i64::MAX.into()),
        _ => return false,
    };
    (lo..=hi).contains(&v)
}

fn unary(op: UnOp, x: ConstVal) -> Option<ConstVal> {
    match (op, x) {
        (UnOp::Neg, ConstVal::Int(v)) => Some(ConstVal::Int(-v)),
        (UnOp::Neg, ConstVal::Float(v)) => Some(ConstVal::Float(-v)),
        (UnOp::Not, ConstVal::Bool(v)) => Some(ConstVal::Bool(!v)),
        _ => None,
    }
}

/// Folds a binary operator. Division by zero is not a constant.
fn binary(op: BinOp, a: ConstVal, b: ConstVal) -> Option<ConstVal> {
    use ConstVal::{Bool, Int};
    match (a, b) {
        (Int(x), Int(y)) => int_op(op, x, y),
        (Bool(x), Bool(y)) => match op {
            BinOp::And => Some(Bool(x && y)),
            BinOp::Or => Some(Bool(x || y)),
            BinOp::Eq => Some(Bool(x == y)),
            BinOp::Ne => Some(Bool(x != y)),
            _ => None,
        },
        (Bool(_), _) | (_, Bool(_)) => None,
        _ => float_op(op, a.as_f64(), b.as_f64()),
    }
}

fn int_op(op: BinOp, x: i128, y: i128) -> Option<ConstVal> {
    use ConstVal::{Bool, Int};
    Some(match op {
        BinOp::Add => Int(x.checked_add(y)?),
        BinOp::Sub => Int(x.checked_sub(y)?),
        BinOp::Mul => Int(x.checked_mul(y)?),
        BinOp::Div => Int(x.checked_div(y)?),
        BinOp::Rem => Int(x.checked_rem(y)?),
        BinOp::Pow => ConstVal::Float((x as f64).powf(y as f64)),
        BinOp::Eq => Bool(x == y),
        BinOp::Ne => Bool(x != y),
        BinOp::Lt => Bool(x < y),
        BinOp::Le => Bool(x <= y),
        BinOp::Gt => Bool(x > y),
        BinOp::Ge => Bool(x >= y),
        BinOp::And | BinOp::Or => return None,
    })
}

fn float_op(op: BinOp, x: f64, y: f64) -> Option<ConstVal> {
    use ConstVal::{Bool, Float};
    Some(match op {
        BinOp::Add => Float(x + y),
        BinOp::Sub => Float(x - y),
        BinOp::Mul => Float(x * y),
        BinOp::Div if y != 0.0 => Float(x / y),
        BinOp::Rem if y != 0.0 => Float(x % y),
        BinOp::Pow => Float(x.powf(y)),
        BinOp::Eq => Bool(x == y),
        BinOp::Ne => Bool(x != y),
        BinOp::Lt => Bool(x < y),
        BinOp::Le => Bool(x <= y),
        BinOp::Gt => Bool(x > y),
        BinOp::Ge => Bool(x >= y),
        _ => return None,
    })
}
