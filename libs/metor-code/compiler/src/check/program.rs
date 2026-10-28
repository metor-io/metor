//! Checking a `system` or a `sequence`: its params, ports, state, alerts,
//! and the bodies the code generator compiles.

use std::collections::BTreeSet;

use mc_rt::desc::{AlertDesc, BlockSizes, EntryDesc, Kind, ParamDesc, PortDesc, TypeDesc};

use crate::ast::{self, Member, ProgramKind, StmtKind};
use crate::diag::{Diagnostic, Span};
use crate::hir::{self, Base, Place};
use crate::types::{ConstVal, FrameTy, Prim, Ty, pascal_case};

use super::scope::{Ctx, Port, Prog, Scope};
use super::Checker;

/// One `alert` and its state slots.
struct Alert {
    name: String,
    severity: String,
    cond: hir::Expr,
    hold_us: i64,
    holding: u32,
    since: u32,
}

/// What a program's members are, once sorted.
#[derive(Default)]
struct Members<'a> {
    doc: Vec<&'a str>,
    steps: Vec<&'a ast::Stmt>,
    loops: Vec<(&'a [ast::Stmt], Span)>,
    alerts: Vec<&'a Member>,
    redlines: Vec<(&'a str, &'a ast::Expr)>,
    on_abort: Vec<(&'a [ast::Stmt], Span)>,
}

impl Checker {
    pub(super) fn program(&mut self, decl: &ast::ProgramDecl) -> Option<hir::Type> {
        let errors = self.diags.len();
        let mut prog = Prog::new(decl.kind);
        let resume_slot = prog.alloc(4);
        let fail_slot = prog.alloc(4);
        let params = self.params(&mut prog, &decl.params);
        let keep = self.ports(&mut prog, decl);
        let members = sort_members(&decl.members);
        let alerts_port = self.alerts_port(&mut prog, decl, &members.alerts);
        let alerts_at = alerts_port.map(|i| (i, prog.outputs[i as usize].offset));
        let (prog, alerts) = self.alerts(prog, &members.alerts);
        let bodies = match decl.kind {
            ProgramKind::System => self.system_bodies(prog, decl, &members),
            ProgramKind::Sequence => self.sequence_bodies(prog, decl, &members, &keep, alerts_port),
        };
        let (mut prog, init, step, redlines, on_abort) = bodies;
        if self.diags.len() > errors {
            return None;
        }
        let desc = TypeDesc {
            name: decl.name.name.clone(),
            kind: match decl.kind {
                ProgramKind::System => Kind::Cyclic,
                ProgramKind::Sequence => Kind::Sequence,
            },
            doc: members.doc.join("\n"),
            inputs: port_descs(&prog.inputs),
            outputs: port_descs(&prog.outputs),
            blocks: BlockSizes {
                inputs: block_size(&prog.inputs, &self.frames),
                outputs: block_size(&prog.outputs, &self.frames),
                state: prog.state_size,
                params: params.iter().map(|p| p.offset + p.prim.size()).max().unwrap_or(0),
            },
            params,
            entry: EntryDesc {
                redlines: members
                    .redlines
                    .iter()
                    .map(|(name, _)| mc_rt::desc::RedlineDesc {
                        name: name.to_string(),
                        func: 0,
                    })
                    .collect(),
                ..EntryDesc::default()
            },
            fails: core::mem::take(&mut prog.fails),
            fail_slot,
            alerts: alerts
                .iter()
                .map(|a| AlertDesc {
                    name: a.name.clone(),
                    severity: a.severity.clone(),
                })
                .collect(),
            alerts_port,
            cost: 0,
        };
        Some(hir::Type {
            desc,
            init,
            prelude: alert_stmts(&alerts, alerts_at),
            step,
            redlines,
            on_abort,
            points: prog.points,
            resume_slot,
        })
    }

    /// Lays out the params block.
    fn params(&mut self, prog: &mut Prog, decls: &[ast::ParamDecl]) -> Vec<ParamDesc> {
        let mut params: Vec<ParamDesc> = Vec::new();
        let mut size = 0u32;
        for decl in decls {
            if params.iter().any(|p| p.name == decl.name.name) {
                self.error(decl.name.span, format!("param `{}` is declared twice", decl.name.name));
                continue;
            }
            let declared = decl.ty.as_ref().and_then(|ty| self.scalar_type(ty));
            let default = decl.default.as_ref().and_then(|e| {
                let Some(value) = self.const_eval(e) else {
                    self.error(e.span, "a param's default must be a constant");
                    return None;
                };
                self.coerce_const(value, declared, e.span)
            });
            let Some(prim) = declared.or(default.map(|(_, prim)| prim)) else {
                self.error(decl.name.span, "a param needs a type or a default, as in `gain = 1.0`");
                continue;
            };
            let offset = size.next_multiple_of(prim.size());
            size = offset + prim.size();
            prog.params.push((decl.name.name.clone(), prim, offset));
            params.push(ParamDesc {
                name: decl.name.name.clone(),
                prim,
                offset,
                default: default.map_or(serde_json::Value::Null, |(v, _)| json(v)),
            });
        }
        params
    }

    /// Resolves the ports and lays out their blocks. Returns which outputs
    /// are marked `keep`.
    fn ports(&mut self, prog: &mut Prog, decl: &ast::ProgramDecl) -> Vec<bool> {
        let mut keep = Vec::new();
        for member in &decl.members {
            match member {
                Member::In { name, source, .. } => {
                    let frame = match source {
                        ast::PortSource::Typed(ty) => self.frame_type(ty),
                        ast::PortSource::Bound { program, port } => self.bound_port(program, port),
                    };
                    if let Some(frame) = frame
                        && self.fresh_port(prog, name)
                    {
                        let offset = next_offset(&prog.inputs, &self.frames);
                        prog.inputs.push(Port { name: name.name.clone(), frame, offset });
                    }
                }
                Member::Out { name, ty, keep: kept, .. } => {
                    if let Some(frame) = self.frame_type(ty)
                        && self.fresh_port(prog, name)
                    {
                        let offset = next_offset(&prog.outputs, &self.frames);
                        prog.outputs.push(Port { name: name.name.clone(), frame, offset });
                        keep.push(*kept);
                    }
                }
                _ => {}
            }
        }
        keep
    }

    /// Whether `name` is free for a port, reporting it if not.
    fn fresh_port(&mut self, prog: &Prog, name: &ast::Ident) -> bool {
        let taken = prog.inputs.iter().chain(&prog.outputs).any(|p| p.name == name.name)
            || prog.params.iter().any(|(n, _, _)| *n == name.name)
            || matches!(name.name.as_str(), "log" | "status" | "abort" | "commands" | "channel_status");
        if taken {
            self.error(name.span, format!("the name `{}` is already taken", name.name));
        }
        !taken
    }

    /// Resolves `in x = prog.port` to that output's frame.
    fn bound_port(&mut self, program: &ast::Ident, port: &ast::Ident) -> Option<usize> {
        let Some(outputs) = self.outputs.get(&program.name) else {
            self.error(program.span, format!("no program `{}` in this file", program.name));
            return None;
        };
        let found = outputs.iter().find(|(name, _)| *name == port.name).map(|(_, f)| *f);
        if found.is_none() {
            self.error(port.span, format!("`{}` has no output `{}`", program.name, port.name));
        }
        found
    }

    /// Adds the `alerts` output, a frame with one `bool` per alert.
    fn alerts_port(&mut self, prog: &mut Prog, decl: &ast::ProgramDecl, alerts: &[&Member]) -> Option<u32> {
        if alerts.is_empty() {
            return None;
        }
        let mut fields = Vec::new();
        for member in alerts {
            let Member::Alert { name, .. } = member else { continue };
            if fields.iter().any(|(n, _)| *n == name.name) {
                self.error(name.span, format!("alert `{}` is declared twice", name.name));
            }
            fields.push((name.name.clone(), Ty::Prim(Prim::Bool)));
        }
        let name = format!("{}Alerts", pascal_case(&decl.name.name));
        let frame = FrameTy::declared(&name, fields, &self.frames, decl.name.span);
        let index = match self.frame_named(&name) {
            Some(index) => index,
            None => {
                self.frames.push(frame);
                self.frames.len() - 1
            }
        };
        if prog.outputs.iter().any(|p| p.name == "alerts") {
            self.error(decl.name.span, "a program with alerts cannot have its own `alerts` output");
            return None;
        }
        let offset = next_offset(&prog.outputs, &self.frames);
        prog.outputs.push(Port { name: "alerts".into(), frame: index, offset });
        Some((prog.outputs.len() - 1) as u32)
    }

    /// Checks each alert's condition and gives it its state slots.
    fn alerts(&mut self, prog: Prog, members: &[&Member]) -> (Prog, Vec<Alert>) {
        let mut sc = Scope::program(Ctx::Redline, prog);
        let mut alerts = Vec::new();
        for member in members {
            let Member::Alert { name, severity, cond, hold, .. } = member else { continue };
            let cond = self.cond(&mut sc, cond);
            let hold_us = match hold {
                None => 0,
                Some(hold) => match self.const_eval(hold) {
                    Some(value) if value.as_f64() >= 0.0 => (value.as_f64() * 1e6).round() as i64,
                    _ => {
                        self.error(hold.span, "an alert's `for` must be a constant duration");
                        0
                    }
                },
            };
            alerts.push(Alert {
                name: name.name.clone(),
                severity: severity.name.clone(),
                cond,
                hold_us,
                holding: sc.prog.alloc(1),
                since: sc.prog.alloc(8),
            });
        }
        (sc.prog, alerts)
    }

    /// A system: its state initializers and its `loop:`.
    fn system_bodies(&mut self, prog: Prog, decl: &ast::ProgramDecl, members: &Members<'_>) -> Bodies {
        let mut sc = Scope::program(Ctx::Init, prog);
        let mut init = Vec::new();
        for stmt in &members.steps {
            self.state_init(&mut sc, stmt, &mut init);
        }
        for (name, _) in &members.redlines {
            self.error(decl.name.span, format!("redline `{name}` belongs in a sequence"));
        }
        if let Some((_, span)) = members.on_abort.first() {
            self.error(*span, "`on abort:` belongs in a sequence");
        }
        if let Some((_, span)) = members.loops.get(1) {
            self.error(*span, "a system has one `loop:`");
        }
        let init = sc.finish(init);
        let mut sc = Scope::program(Ctx::Step, sc.prog);
        let stmts = match members.loops.first() {
            Some((body, _)) => self.stmts(&mut sc, body),
            None => Vec::new(),
        };
        let step = sc.finish(stmts);
        (sc.prog, init, step, Vec::new(), None)
    }

    /// One state declaration at a system's top level: `name = value`.
    fn state_init(&mut self, sc: &mut Scope, stmt: &ast::Stmt, init: &mut Vec<hir::Stmt>) {
        let (name, ty, value) = match &stmt.kind {
            StmtKind::Assign { target, op: None, value } => match &target.kind {
                ast::ExprKind::Name(name) => (name.as_str(), None, value),
                _ => return self.error(stmt.span, "a system's top level declares state, as in `x = 0.0`"),
            },
            StmtKind::Declare { name, ty, value } => (name.name.as_str(), Some(ty), value),
            _ => {
                let diag = Diagnostic::new(stmt.span, "a system's top level declares ports, params, state and alerts")
                    .with_note("statements that run each cycle go in `loop:`");
                return self.diags.push(diag);
            }
        };
        if sc.var(name).is_some() || sc.port(name).is_some() || sc.param(name).is_some() {
            return self.error(stmt.span, format!("`{name}` is already defined"));
        }
        let prim = ty.and_then(|ty| self.scalar_type(ty));
        let value = match prim {
            Some(prim) => self.expect(sc, value, prim),
            None => self.expr(sc, value, None),
        };
        let super::scope::Loc::State(offset) = sc.prog.state_var(name, value.prim) else { return };
        init.push(hir::Stmt::Store {
            place: Place::at(Base::State, offset, value.prim),
            value,
            nan_guard: false,
            mark: None,
        });
    }

    /// A sequence: its steps, redlines and `on abort:`, with the abort
    /// coverage check.
    fn sequence_bodies(&mut self, prog: Prog, decl: &ast::ProgramDecl, members: &Members<'_>, keep: &[bool], alerts: Option<u32>) -> Bodies {
        if let Some((_, span)) = members.loops.first() {
            self.error(*span, "a sequence runs its steps once; it has no `loop:`");
        }
        if let Some((_, span)) = members.on_abort.get(1) {
            self.error(*span, "a sequence has one `on abort:`");
        }
        let mut sc = Scope::program(Ctx::SeqBody, prog);
        let steps: Vec<ast::Stmt> = members.steps.iter().map(|s| (*s).clone()).collect();
        let stmts = self.stmts(&mut sc, &steps);
        let written = core::mem::take(&mut sc.writes);
        let step = sc.finish(stmts);
        let mut redlines = Vec::new();
        let mut prog = sc.prog;
        for (_, cond) in &members.redlines {
            let mut sc = Scope::program(Ctx::Redline, prog);
            let cond = self.cond(&mut sc, cond);
            redlines.push(sc.finish(vec![hir::Stmt::Return(cond)]));
            prog = sc.prog;
        }
        let mut sc = Scope::program(Ctx::SeqAbort, prog);
        let on_abort = members.on_abort.first().map(|(body, _)| {
            let stmts = self.stmts(&mut sc, body);
            sc.finish(stmts)
        });
        let covered = core::mem::take(&mut sc.writes);
        self.check_coverage(&sc.prog, decl, &written, &covered, keep, alerts);
        (sc.prog, hir::Body::default(), step, redlines, on_abort)
    }

    /// Every output the steps write must be written in `on abort:` too, or
    /// be marked `keep`.
    fn check_coverage(&mut self, prog: &Prog, decl: &ast::ProgramDecl, written: &BTreeSet<u32>, covered: &BTreeSet<u32>, keep: &[bool], alerts: Option<u32>) {
        for &index in written {
            let kept = keep.get(index as usize).copied().unwrap_or(false);
            if covered.contains(&index) || kept || Some(index) == alerts {
                continue;
            }
            let port = &prog.outputs[index as usize].name;
            let span = out_span(decl, port).unwrap_or(decl.name.span);
            let diag = Diagnostic::new(span, format!("`{}` writes `{port}`, but `on abort:` does not", decl.name.name))
                .with_label(format!("`{port}` is left as the steps wrote it if the sequence aborts"))
                .with_note(format!("write a safe value in `on abort:`, or mark it `out {port}: … keep`"));
            self.diags.push(diag);
        }
    }
}

/// The bodies of a program: its prog, `init`, `step`, redlines and
/// `on abort:`.
type Bodies = (Prog, hir::Body, hir::Body, Vec<hir::Body>, Option<hir::Body>);

fn sort_members(members: &[Member]) -> Members<'_> {
    let mut sorted = Members::default();
    for member in members {
        match member {
            Member::Doc(doc) => sorted.doc.push(doc),
            Member::Stmt(stmt) => sorted.steps.push(stmt),
            Member::Loop { body, span } => sorted.loops.push((body, *span)),
            Member::Alert { .. } => sorted.alerts.push(member),
            Member::Redline { name, cond, .. } => sorted.redlines.push((name, cond)),
            Member::OnAbort { body, span } => sorted.on_abort.push((body, *span)),
            Member::In { .. } | Member::Out { .. } => {}
        }
    }
    sorted
}

/// Evaluates each alert and writes the `alerts` output:
///
/// ```text
/// if cond:
///     if not holding: holding = True; since = now
///     alerts.name = now - since >= hold
/// else:
///     holding = False; alerts.name = False
/// ```
///
/// `port` is the `alerts` output's index and offset. Its frame holds one
/// `bool` per alert after the timestamp.
fn alert_stmts(alerts: &[Alert], port: Option<(u32, u32)>) -> Vec<hir::Stmt> {
    let Some((port, port_offset)) = port else { return Vec::new() };
    let state = |offset, prim| Place::at(Base::State, offset, prim);
    let set = |place: Place, value| hir::Stmt::Store { place, value, nan_guard: false, mark: None };
    let bool_const = |v| hir::Expr::constant(ConstVal::Bool(v), Prim::Bool);
    let now = || hir::Expr { kind: hir::ExprKind::NowMicros, prim: Prim::I64 };
    let mut stmts = Vec::new();
    for (index, alert) in alerts.iter().enumerate() {
        let flag = Place::at(Base::Output, port_offset + 8 + index as u32, Prim::Bool);
        let held = hir::Expr {
            kind: hir::ExprKind::Binary(
                hir::BinOp::Ge,
                Box::new(hir::Expr {
                    kind: hir::ExprKind::Binary(
                        hir::BinOp::Sub,
                        Box::new(now()),
                        Box::new(hir::Expr::load(state(alert.since, Prim::I64))),
                    ),
                    prim: Prim::I64,
                }),
                Box::new(hir::Expr::constant(ConstVal::Int(alert.hold_us.into()), Prim::I64)),
            ),
            prim: Prim::Bool,
        };
        let start = hir::Stmt::If {
            cond: hir::Expr {
                kind: hir::ExprKind::Not(Box::new(hir::Expr::load(state(alert.holding, Prim::Bool)))),
                prim: Prim::Bool,
            },
            then: vec![set(state(alert.holding, Prim::Bool), bool_const(true)), set(state(alert.since, Prim::I64), now())],
            orelse: Vec::new(),
        };
        stmts.push(hir::Stmt::If {
            cond: alert.cond.clone(),
            then: vec![start, set(flag.clone(), held)],
            orelse: vec![set(state(alert.holding, Prim::Bool), bool_const(false)), set(flag, bool_const(false))],
        });
    }
    stmts.push(hir::Stmt::Mark(port));
    stmts
}

fn port_descs(ports: &[Port]) -> Vec<PortDesc> {
    ports
        .iter()
        .map(|p| PortDesc {
            name: p.name.clone(),
            frame: p.frame as u32,
            offset: p.offset,
        })
        .collect()
}

/// The offset of the next port's frame in a block.
fn next_offset(ports: &[Port], frames: &[FrameTy]) -> u32 {
    block_size(ports, frames).next_multiple_of(8)
}

fn block_size(ports: &[Port], frames: &[FrameTy]) -> u32 {
    ports.last().map_or(0, |p| p.offset + frames[p.frame].size)
}

fn out_span(decl: &ast::ProgramDecl, port: &str) -> Option<Span> {
    decl.members.iter().find_map(|m| match m {
        Member::Out { name, span, .. } if name.name == port => Some(*span),
        _ => None,
    })
}

fn json(value: ConstVal) -> serde_json::Value {
    match value {
        ConstVal::Int(v) => i64::try_from(v)
            .map(serde_json::Value::from)
            .unwrap_or_else(|_| serde_json::Value::from(v as u64)),
        ConstVal::Float(v) => serde_json::Value::from(v),
        ConstVal::Bool(v) => serde_json::Value::from(v),
    }
}
