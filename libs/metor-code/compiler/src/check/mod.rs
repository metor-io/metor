//! Name resolution and type checking, from the syntax tree to [`hir`].
//!
//! The checker resolves frames (declared and imported), constants, helper
//! `def`s and each program's ports, params and state, and lowers every body
//! to typed statements over block offsets. It rejects anything that could
//! stall or crash: loops without a constant bound, recursive `def`s,
//! constant division by zero, constant indices out of range, and sequence
//! outputs that `on abort:` leaves unwritten.

mod consts;
mod expr;
mod program;
mod scope;
mod stmt;
#[cfg(test)]
mod tests;

use std::collections::HashMap;

use mc_rt::desc::FrameDesc;

use crate::ast::{self, Item};
use crate::diag::{Diagnostic, Span};
use crate::hir;
use crate::types::{ConstVal, FrameTy, Prim, Ty, prim_named};

pub use scope::Scope;

/// Resolves `use frames from "pack"` to the frames that pack declares.
pub trait Imports {
    fn frames(&self, pack: &str) -> Result<Vec<FrameDesc>, String>;
}

/// A resolver that knows no packs.
pub struct NoImports;

impl Imports for NoImports {
    fn frames(&self, pack: &str) -> Result<Vec<FrameDesc>, String> {
        Err(format!("no pack `{pack}` is known"))
    }
}

/// Checks a parsed file. `src` is its text, which some messages quote.
pub fn check(
    file: &ast::File,
    src: &str,
    imports: &dyn Imports,
) -> Result<hir::Program, Vec<Diagnostic>> {
    let mut checker = Checker {
        src: src.to_string(),
        ..Checker::default()
    };
    checker.items(file, imports);
    match checker.diags.is_empty() {
        true => Ok(hir::Program {
            frames: checker.frames,
            defs: checker.defs.into_iter().map(|d| d.def).collect(),
            types: checker.types,
        }),
        false => Err(checker.diags),
    }
}

/// A named constant.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Const {
    pub value: ConstVal,
    /// The declared type, or `None` for a constant that takes the type its
    /// use needs, as a literal does.
    pub prim: Option<Prim>,
}

/// A `def`'s signature and, once checked, its body.
#[derive(Clone, Debug)]
pub(crate) struct DefEntry {
    pub params: Vec<(String, Prim)>,
    pub span: Span,
    pub def: hir::Def,
    /// The defs this one calls.
    pub calls: Vec<u32>,
}

#[derive(Default)]
pub(crate) struct Checker {
    pub diags: Vec<Diagnostic>,
    pub frames: Vec<FrameTy>,
    pub consts: HashMap<String, Const>,
    pub defs: Vec<DefEntry>,
    pub types: Vec<hir::Type>,
    /// Each program's outputs, for `in x = prog.port`.
    pub outputs: HashMap<String, Vec<(String, usize)>>,
    pub src: String,
}

impl Checker {
    pub fn error(&mut self, span: Span, message: impl Into<String>) {
        self.diags.push(Diagnostic::new(span, message));
    }

    pub fn frame_named(&self, name: &str) -> Option<usize> {
        self.frames.iter().position(|f| f.name == name)
    }

    pub fn def_named(&self, name: &str) -> Option<u32> {
        self.defs
            .iter()
            .position(|d| d.def.name == name)
            .map(|i| i as u32)
    }

    /// Checks every item, in an order that lets each refer to the others.
    fn items(&mut self, file: &ast::File, imports: &dyn Imports) {
        for item in &file.items {
            match item {
                Item::Use { pack, span } => self.import(pack, *span, imports),
                Item::Const(decl) => self.constant(decl),
                Item::Machine { span, .. } => {
                    self.error(*span, "`machine` is not implemented yet");
                }
                _ => {}
            }
        }
        for item in &file.items {
            if let Item::Frame(decl) = item {
                self.frame(decl);
            }
        }
        let defs: Vec<&ast::DefDecl> = file
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Def(decl) => Some(decl),
                _ => None,
            })
            .collect();
        for decl in &defs {
            self.def_signature(decl);
        }
        for (index, decl) in defs.iter().enumerate() {
            self.def_body(index, decl);
        }
        self.check_call_graph();
        let programs: Vec<&ast::ProgramDecl> = file
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Program(decl) => Some(decl),
                _ => None,
            })
            .collect();
        for decl in &programs {
            self.collect_outputs(decl);
        }
        for decl in programs {
            if let Some(ty) = self.program(decl) {
                self.types.push(ty);
            }
        }
    }

    /// Adds the frames a pack declares, checking any the file also declares
    /// by layout.
    fn import(&mut self, pack: &str, span: Span, imports: &dyn Imports) {
        let descs = match imports.frames(pack) {
            Ok(descs) => descs,
            Err(message) => {
                return self.error(span, format!("importing frames from `{pack}`: {message}"));
            }
        };
        for desc in descs {
            let Some(frame) = FrameTy::imported(&desc, pack, span) else {
                self.error(
                    span,
                    format!(
                        "frame `{}` of `{pack}` has fields metor-code cannot express",
                        desc.name
                    ),
                );
                continue;
            };
            if let Some(known) = self.frame_named(&frame.name) {
                if self.frames[known].desc() != frame.desc() {
                    self.error(
                        span,
                        format!("two imports define frame `{}` differently", frame.name),
                    );
                }
                continue;
            }
            self.frames.push(frame);
        }
    }

    /// Lays out a declared frame.
    fn frame(&mut self, decl: &ast::FrameDecl) {
        let mut fields = Vec::new();
        for field in &decl.fields {
            if fields.iter().any(|(name, _)| *name == field.name.name) {
                self.error(
                    field.name.span,
                    format!("field `{}` is declared twice", field.name.name),
                );
            }
            if let Some(ty) = self.field_type(&field.ty) {
                fields.push((field.name.name.clone(), ty));
            }
        }
        let frame = FrameTy::declared(&decl.name.name, fields, &self.frames, decl.span);
        match self.frame_named(&frame.name) {
            Some(known) if self.frames[known].pack.is_some() => {
                if self.frames[known].desc() != frame.desc() {
                    let pack = self.frames[known].pack.clone().unwrap_or_default();
                    let diag = Diagnostic::new(decl.name.span, format!("frame `{}` does not match the layout `{pack}` reports", frame.name))
                        .with_note("a frame declared here and imported must have the same fields, in the same order");
                    self.diags.push(diag);
                }
            }
            Some(_) => self.error(
                decl.name.span,
                format!("frame `{}` is declared twice", frame.name),
            ),
            None => self.frames.push(frame),
        }
    }

    /// Resolves a frame field's type: a scalar, or an array of them.
    fn field_type(&mut self, ty: &ast::TypeExpr) -> Option<Ty> {
        match ty {
            ast::TypeExpr::Named(name) => match prim_named(&name.name) {
                Some(prim) => Some(Ty::Prim(prim)),
                None => {
                    let message = match self.frame_named(&name.name) {
                        Some(_) => format!(
                            "a frame field cannot be the frame `{}`; nested frames are not supported yet",
                            name.name
                        ),
                        None => format!("`{}` is not a type", name.name),
                    };
                    self.error(name.span, message);
                    None
                }
            },
            ast::TypeExpr::Array { elem, len, span } => {
                let elem = self.field_type(elem)?;
                let len = self.const_count(len, *span)?;
                if len == 0 {
                    self.error(*span, "an array needs at least one element");
                    return None;
                }
                Some(Ty::Array(Box::new(elem), len))
            }
        }
    }

    /// Resolves a scalar type, as params, locals and `def`s use.
    pub fn scalar_type(&mut self, ty: &ast::TypeExpr) -> Option<Prim> {
        if let ast::TypeExpr::Named(name) = ty
            && let Some(prim) = prim_named(&name.name)
        {
            return Some(prim);
        }
        self.error(
            ty.span(),
            "expected a scalar type: bool, an integer, f32 or f64",
        );
        None
    }

    /// Resolves a port's type, which must be a frame.
    pub fn frame_type(&mut self, ty: &ast::TypeExpr) -> Option<usize> {
        if let ast::TypeExpr::Named(name) = ty
            && let Some(frame) = self.frame_named(&name.name)
        {
            return Some(frame);
        }
        let diag = Diagnostic::new(ty.span(), "a port carries a frame")
            .with_label(format!("no frame named `{}`", type_text(ty)))
            .with_note(
                "declare it with `frame Name:`, or import it with `use frames from \"pack\"`",
            );
        self.diags.push(diag);
        None
    }

    fn def_signature(&mut self, decl: &ast::DefDecl) {
        if self.def_named(&decl.name.name).is_some() {
            self.error(
                decl.name.span,
                format!("`{}` is defined twice", decl.name.name),
            );
        }
        let params: Vec<(String, Prim)> = decl
            .params
            .iter()
            .filter_map(|(name, ty)| Some((name.name.clone(), self.scalar_type(ty)?)))
            .collect();
        let ret = self.scalar_type(&decl.ret).unwrap_or(Prim::F64);
        self.defs.push(DefEntry {
            def: hir::Def {
                name: decl.name.name.clone(),
                params: params.iter().map(|(_, p)| *p).collect(),
                ret,
                body: hir::Body::default(),
            },
            params,
            span: decl.name.span,
            calls: Vec::new(),
        });
    }

    fn def_body(&mut self, index: usize, decl: &ast::DefDecl) {
        let entry = &self.defs[index];
        let mut scope = Scope::def(&entry.params, entry.def.ret);
        let stmts = self.stmts(&mut scope, &decl.body);
        let entry = &mut self.defs[index];
        entry.calls = scope.calls.clone();
        entry.def.body = scope.finish(stmts);
    }

    /// Refuses recursion: the `def` call graph must have no cycle.
    fn check_call_graph(&mut self) {
        let mut state = vec![0u8; self.defs.len()];
        for start in 0..self.defs.len() {
            if let Some(cycle) = self.find_cycle(start, &mut state) {
                let name = &self.defs[cycle].def.name;
                let span = self.defs[cycle].span;
                let diag = Diagnostic::new(
                    span,
                    format!("`{name}` calls itself, directly or through other defs"),
                )
                .with_note("recursion is not allowed, so every call finishes in bounded time");
                self.diags.push(diag);
                return;
            }
        }
    }

    /// Depth-first search; `state` is 0 unseen, 1 on the stack, 2 done.
    fn find_cycle(&self, def: usize, state: &mut [u8]) -> Option<usize> {
        match state[def] {
            1 => return Some(def),
            2 => return None,
            _ => {}
        }
        state[def] = 1;
        for &callee in &self.defs[def].calls {
            if let Some(cycle) = self.find_cycle(callee as usize, state) {
                return Some(cycle);
            }
        }
        state[def] = 2;
        None
    }

    fn collect_outputs(&mut self, decl: &ast::ProgramDecl) {
        let mut outputs = Vec::new();
        for member in &decl.members {
            if let ast::Member::Out { name, ty, .. } = member
                && let ast::TypeExpr::Named(frame) = ty
                && let Some(frame) = self.frame_named(&frame.name)
            {
                outputs.push((name.name.clone(), frame));
            }
        }
        self.outputs.insert(decl.name.name.clone(), outputs);
    }
}

/// The source text of a type, for messages.
fn type_text(ty: &ast::TypeExpr) -> String {
    match ty {
        ast::TypeExpr::Named(name) => name.name.clone(),
        ast::TypeExpr::Array { elem, .. } => format!("[{}; _]", type_text(elem)),
    }
}
