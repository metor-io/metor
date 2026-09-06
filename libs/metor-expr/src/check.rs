//! Type-check Python expressions into trusted IR.

use std::collections::{HashMap, HashSet};

use ruff_python_ast as ast;
use ruff_text_size::{Ranged, TextRange};

use crate::diag::{Diagnostics, Span};
use crate::ir::{
    Arith, Batch, BufId, Cmp, Desc, Emit, Expr, FieldWrite, Func, Intrinsic, Num, Place, Program,
    Seed, Stmt, SystemAbi,
};
use crate::lang::{self, SystemDecl};
use crate::manifest::{self, Frame, Port};
use crate::{Dtype, FnSig, Manifest, Resolver, Ty};

/// Maximum expression depth accepted by the checker.
const MAX_DEPTH: u32 = 512;

/// Maximum source size accepted by the checker.
const MAX_SOURCE_BYTES: usize = 256 * 1024;

/// Stack reserved for recursive parser work.
const PARSE_STACK_BYTES: usize = 64 * 1024 * 1024;

/// Maximum tensor rank supported by kernel descriptors.
const MAX_RANK: usize = 4;

/// Maximum scalar work emitted inline before calling a kernel.
const OPEN_CODE_MAX_OPS: usize = 128;

/// Choose inline code for small operations and a kernel otherwise.
fn emit_for(ops: usize) -> Emit {
    if ops <= OPEN_CODE_MAX_OPS {
        Emit::Open
    } else {
        Emit::Kernel
    }
}

/// Whether the source is a module of declarations or a single expression.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Entry {
    Module,
    Expression,
}

pub(crate) fn check(
    source: &str,
    resolver: &dyn Resolver,
    entry: Entry,
) -> Result<(Program, Manifest), Diagnostics> {
    if source.len() > MAX_SOURCE_BYTES {
        let mut diags = Diagnostics::default();
        let at = MAX_SOURCE_BYTES as u32;
        diags.push(
            Span::new(at, source.len() as u32),
            format!("source is larger than the {MAX_SOURCE_BYTES} byte limit"),
        );
        return Err(diags);
    }

    // Isolate parser stack use and convert parser panics into diagnostics.
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .stack_size(PARSE_STACK_BYTES)
            .spawn_scoped(scope, || check_inner(source, resolver, entry))
            .expect("spawning the parse thread")
            .join()
            .unwrap_or_else(|_| {
                let mut diags = Diagnostics::default();
                diags.push(Span::new(0, 0), "the parser could not handle this source");
                Err(diags)
            })
    })
}

fn check_inner(
    source: &str,
    resolver: &dyn Resolver,
    entry: Entry,
) -> Result<(Program, Manifest), Diagnostics> {
    let mut diags = Diagnostics::default();

    let parsed = ruff_python_parser::parse_unchecked_source(source, ast::PySourceType::Python);
    if !parsed.has_valid_syntax() {
        // The parser recovered a tree anyway, but checking it would report
        // ghosts of the recovery alongside the real mistake. Every syntax
        // error is reported — recovery is what lets there be more than one.
        for err in parsed.errors() {
            diags.push(err.location, format!("{}", err.error));
        }
        return Err(diags);
    }
    let module = parsed.into_syntax().body;

    let decls = match entry {
        Entry::Module => lang::collect(&module, source, resolver, &mut diags),
        Entry::Expression => match module.as_slice() {
            [ast::Stmt::Expr(only)] => {
                let systems: Vec<_> = lang::expression(&only.value, resolver, &mut diags)
                    .into_iter()
                    .collect();
                lang::Decls {
                    order: systems
                        .iter()
                        .enumerate()
                        .map(|(i, _)| lang::Item::System(i))
                        .collect(),
                    systems,
                    stages: Vec::new(),
                    functions: Vec::new(),
                }
            }
            _ => {
                diags.push(Span::new(0, source.len() as u32), "expected one expression");
                return Err(diags);
            }
        },
    };
    let defs = &decls.functions;

    let mut sigs: Vec<FnSig> = Vec::new();
    let mut by_name: HashMap<String, u32> = HashMap::new();
    for def in defs {
        let name = def.name.as_str().to_string();
        let mut params = Vec::new();
        let args = &def.parameters;
        if !args.posonlyargs.is_empty()
            || !args.kwonlyargs.is_empty()
            || args.vararg.is_some()
            || args.kwarg.is_some()
        {
            diags.push(def.range, "only plain positional parameters are supported");
        }
        for arg in &args.args {
            if arg.default.is_some() {
                diags.push(arg.parameter.range, "default arguments are not supported");
            }
            let ty = match &arg.parameter.annotation {
                Some(ann) => annotation(ann, &mut diags),
                None => {
                    diags.push(arg.parameter.range, "parameters must be annotated");
                    Ty::F64
                }
            };
            params.push((arg.parameter.name.as_str().to_string(), ty));
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
        sigs.push(FnSig {
            name,
            params,
            ret,
            source: def.range.into(),
        });
    }

    let mut buffers: Vec<Place> = Vec::new();
    let mut frames: Vec<FnFrame> = Vec::new();
    for sig in &sigs {
        let buffer_abi = sig.uses_buffers();
        let mut arg_buffers = Vec::new();
        let mut ret_buffer = None;
        if buffer_abi {
            for (_, ty) in &sig.params {
                arg_buffers.push(alloc(&mut buffers, slot_bytes(ty)));
            }
            ret_buffer = Some(alloc(&mut buffers, slot_bytes(&sig.ret)));
        }
        frames.push(FnFrame {
            buffer_abi,
            arg_buffers,
            ret_buffer,
        });
    }

    let mut funcs = Vec::new();
    let mut edges: Vec<Vec<u32>> = Vec::new();
    for ((def, sig), frame) in defs.iter().zip(&sigs).zip(&frames) {
        let before = diags.len();
        let mut locals = Vec::new();
        let mut names = HashMap::new();
        let mut prologue = Vec::new();
        for (i, (name, ty)) in sig.params.iter().enumerate() {
            if matches!(ty, Ty::Tensor { .. }) {
                names.insert(
                    name.clone(),
                    Binding::Tensor {
                        buf: frame.arg_buffers[i],
                        ty: ty.clone(),
                    },
                );
                continue;
            }
            let slot = locals.len() as u32;
            locals.push(ty.clone());
            if frame.buffer_abi {
                prologue.push((frame.arg_buffers[i], slot, ty.clone()));
            }
            names.insert(
                name.clone(),
                Binding::Scalar {
                    slot,
                    ty: ty.clone(),
                },
            );
        }
        let param_count = if frame.buffer_abi { 0 } else { locals.len() };

        let mut body = FnChecker {
            diags: &mut diags,
            sigs: &sigs,
            by_name: &by_name,
            frames: &frames,
            buffers: &mut buffers,
            ret: sig.ret.clone(),
            ret_buffer: frame.ret_buffer,
            buffer_abi: frame.buffer_abi,
            output: None,
            now: None,
            synthetic: Vec::new(),
            locals,
            names,
            loop_depth: 0,
            calls: Vec::new(),
        };
        let stmts = body.block(&def.body, 0);
        let complete = always_returns(&stmts);
        let locals = body.locals;
        edges.push(body.calls);
        // Only worth saying when the body itself was understood; otherwise it
        // is noise stacked on the real complaint.
        if !complete && diags.len() == before {
            diags.push(def.range, "not every path through this function returns");
        }
        funcs.push(Func {
            name: sig.name.clone(),
            param_count,
            locals,
            ret: sig.ret.clone(),
            body: stmts,
            buffer_abi: frame.buffer_abi,
            arg_buffers: frame.arg_buffers.clone(),
            ret_buffer: frame.ret_buffer,
            prologue,
            system: None,
        });
    }

    for cycle in tensor_cycles(&edges, &frames) {
        let name = &sigs[cycle as usize].name;
        diags.push(
            defs[cycle as usize].range,
            format!(
                "`{name}` is recursive and passes tensors, whose buffers are statically placed"
            ),
        );
    }

    // Declaration order, because a binding may name what came before it and
    // a stage's output type can be a system's — which is only known once that
    // system's body has been checked.
    let mut systems: Vec<manifest::System> = Vec::new();
    let mut stages: Vec<manifest::Stage> = Vec::new();
    for item in &decls.order {
        match *item {
            lang::Item::System(at) => {
                let decl = &decls.systems[at];
                let before = diags.len();
                match system(
                    decl,
                    Known {
                        sigs: &sigs,
                        by_name: &by_name,
                        frames: &frames,
                        edges: &edges,
                        systems: &systems,
                        stages: &stages,
                    },
                    &mut buffers,
                    &mut diags,
                ) {
                    Some((func, desc)) => {
                        funcs.push(func);
                        systems.push(desc);
                    }
                    None if diags.len() == before => {
                        diags.push(decl.span, format!("`{}` could not be compiled", decl.name))
                    }
                    None => {}
                }
            }
            lang::Item::Stage(at) => {
                let decl = &decls.stages[at];
                let Some(ty) = decl
                    .ty
                    .clone()
                    .or_else(|| produced_ty(&decl.source, &systems, &stages))
                else {
                    diags.push(decl.span, format!("`{}` reads nothing", decl.name));
                    continue;
                };
                stages.push(manifest::Stage {
                    name: decl.name.clone(),
                    kind: decl.kind,
                    source: decl.source.clone(),
                    rate: decl.rate,
                    ty,
                    layout: decl.layout,
                    source_span: decl.span,
                });
            }
        }
    }

    if diags.is_empty() {
        let param_types = sigs
            .iter()
            .map(|sig| sig.params.iter().map(|(_, t)| t.clone()).collect())
            .collect();
        Ok((
            Program {
                funcs,
                param_types,
                buffers,
            },
            Manifest {
                compiler: crate::manifest::COMPILER_VERSION,
                systems,
                stages,
                functions: sigs,
            },
        ))
    } else {
        Err(diags.sorted())
    }
}

/// What an in-program binding carries, once its producer has been checked.
fn produced_ty(
    binding: &manifest::Binding,
    systems: &[manifest::System],
    stages: &[manifest::Stage],
) -> Option<Ty> {
    match binding {
        manifest::Binding::Component(_) => None,
        manifest::Binding::Produced { system, field } => {
            Some(systems.get(*system)?.output.fields.get(*field)?.ty.clone())
        }
        manifest::Binding::Resampled { stage } => Some(stages.get(*stage)?.ty.clone()),
        manifest::Binding::Timestamp => Some(Ty::I64),
    }
}

/// Everything a system may refer to that is not its own: the module's plain
/// functions, and the declarations already finished ahead of it.
struct Known<'a> {
    sigs: &'a [FnSig],
    by_name: &'a HashMap<String, u32>,
    frames: &'a [FnFrame],
    edges: &'a [Vec<u32>],
    systems: &'a [manifest::System],
    stages: &'a [manifest::Stage],
}

/// One `@system`, from ports and body to a buffer-ABI function plus the
/// manifest entry that says how to drive it.
fn system(
    decl: &SystemDecl<'_>,
    known: Known<'_>,
    buffers: &mut Vec<Place>,
    diags: &mut Diagnostics,
) -> Option<(Func, manifest::System)> {
    let Known {
        sigs,
        by_name,
        frames,
        edges,
        systems: done,
        stages,
    } = known;
    let mut names: HashMap<String, Binding> = HashMap::new();
    let mut arg_buffers = Vec::with_capacity(decl.ports.len());
    let mut ports = Vec::with_capacity(decl.ports.len());
    for port in &decl.ports {
        // A port reading an earlier anonymous binding takes that binding's
        // output frame, which exists now that the producer has been checked.
        let frame = match &port.frame {
            Some(frame) => frame.clone(),
            None => match &port.bindings[0] {
                manifest::Binding::Produced { system, .. } => done[*system].output.clone(),
                manifest::Binding::Resampled { stage } => {
                    lang::frame_of(&port.key, [(port.key.clone(), stages[*stage].ty.clone())])
                }
                manifest::Binding::Component(path) => {
                    diags.push(decl.span, format!("no component `{path}`"));
                    return None;
                }
                manifest::Binding::Timestamp => {
                    unreachable!("a stamp trails a frame's own fields")
                }
            },
        };
        // A stamped source's frame carries its sample's timestamp as one
        // more field, which is how `deltat` tells one arrival from the next.
        // The body never sees that field by name — it reaches it through
        // `deltat` alone.
        let (frame, bindings) = match port.stamped {
            true => lang::stamped(frame, port.bindings.clone()),
            false => (frame, port.bindings.clone()),
        };
        let block = alloc(buffers, frame.bytes);
        arg_buffers.push(block);
        let mut fields: Vec<(String, Cell)> = frame
            .fields
            .iter()
            .map(|f| {
                (
                    f.name.clone(),
                    Cell {
                        buf: field(buffers, block, f.offset),
                        ty: f.ty.clone(),
                    },
                )
            })
            .collect();
        let stamp = frame.timestamp.map(|i| fields.remove(i).1.buf);
        let binding = if port.projected {
            Binding::Cell {
                cell: fields[0].1.clone(),
                writable: false,
                stamp,
            }
        } else {
            Binding::Record {
                fields,
                writable: false,
                stamp,
            }
        };
        names.insert(port.key.clone(), binding);
        ports.push(Port {
            param: port.key.clone(),
            frame,
            bindings,
        });
    }

    let mut state_buffers = Vec::with_capacity(decl.state.len() + 1);
    let mut seeds = Vec::new();
    for slot in &decl.state {
        let buf = alloc(buffers, slot_bytes(&slot.field.ty));
        state_buffers.push(buf);
        if !slot.field.default.is_zero() {
            seeds.push(Seed {
                dest: buf,
                ty: slot.field.ty.clone(),
                value: slot.field.default.clone(),
            });
        }
        let cell = Cell {
            buf,
            ty: slot.field.ty.clone(),
        };
        match names
            .entry(slot.param.clone())
            .or_insert_with(|| Binding::Record {
                fields: Vec::new(),
                writable: true,
                stamp: None,
            }) {
            Binding::Record { fields, .. } => fields.push((slot.field.name.clone(), cell)),
            _ => {
                diags.push(decl.span, format!("`{}` is already a port", slot.param));
                return None;
            }
        }
    }

    // A declared output frame is placed before the body runs, so a `return`
    // can write straight into its fields. A bare expression's frame cannot be
    // — its type is what the body turns out to compute.
    let placed = decl
        .output
        .as_ref()
        .map(|frame| place_frame(buffers, frame));

    let mut body = FnChecker {
        diags,
        sigs,
        by_name,
        frames,
        buffers,
        ret: Ty::F64,
        ret_buffer: None,
        buffer_abi: true,
        output: placed.as_ref().map(|(_, fields)| Output {
            frame: decl.output.clone().expect("placed implies declared"),
            fields: fields.clone(),
            class: decl.output_class.clone(),
            anonymous: decl.anonymous_output,
        }),
        now: Some(0),
        synthetic: Vec::new(),
        locals: vec![Ty::I64],
        names,
        loop_depth: 0,
        calls: Vec::new(),
    };

    let (stmts, output, ret_block) = match &decl.body {
        lang::Body::Stmts(stmts) => {
            let checked = body.block(stmts, 0);
            if !always_returns(&checked) {
                body.diags
                    .push(decl.span, "not every path through this system returns");
            }
            let (block, _) = placed.expect("a statement body declares its output");
            (
                checked,
                decl.output
                    .clone()
                    .expect("a statement body declares its output"),
                block,
            )
        }
        lang::Body::Expr(expr) => {
            let (value, ty) = body.expr(expr, 0)?;
            let frame = lang::frame_of(&decl.name, [(decl.name.clone(), ty.clone())]);
            let (block, fields) = place_frame(body.buffers, &frame);
            (
                vec![Stmt::ReturnFrame(vec![FieldWrite {
                    dest: fields[0],
                    value,
                    ty,
                }])],
                frame,
                block,
            )
        }
    };
    let dependencies = function_dependencies(&body.calls, edges);
    let locals = body.locals;

    // `random()` keeps a word, `window` keeps a ring, `delta` keeps a sample,
    // and the body is what says whether any is needed — so their slots are
    // allocated where they are first asked for and join the declared fields
    // here, before the guard closes the list. The word and the ring start at
    // zero, which a fresh linear memory already holds (and a window preloaded
    // with zeros is what the panel's Window node published); the sample
    // starts at NaN, which is seeded like a declared default.
    let mut state = decl
        .state
        .iter()
        .map(|s| s.field.clone())
        .collect::<Vec<_>>();
    for (name, ty, buf, default) in body.synthetic {
        state_buffers.push(buf);
        if !default.is_zero() {
            seeds.push(Seed {
                dest: buf,
                ty: ty.clone(),
                value: default.clone(),
            });
        }
        state.push(manifest::StateField { name, ty, default });
    }
    // The seed guard sits one past the state fields, so it is reachable
    // through the same accessor and restores like any other slot.
    if !state_buffers.is_empty() {
        state_buffers.push(alloc(buffers, 8));
    }

    let publishes = output
        .fields
        .iter()
        .map(|f| match decl.anonymous_output {
            true => decl.name.clone(),
            false => format!("{}.{}", decl.name, f.name),
        })
        .collect();

    Some((
        Func {
            name: decl.name.clone(),
            param_count: 1,
            locals,
            ret: Ty::I64,
            body: stmts,
            buffer_abi: true,
            arg_buffers,
            ret_buffer: Some(ret_block),
            prologue: Vec::new(),
            system: Some(SystemAbi {
                state: state_buffers,
                seeds,
            }),
        },
        manifest::System {
            name: decl.name.clone(),
            inputs: ports,
            output,
            publishes,
            state,
            driving: decl.driving,
            rate: decl.rate,
            dependencies,
            layout: decl.layout,
            source: decl.span,
        },
    ))
}

fn function_dependencies(calls: &[u32], edges: &[Vec<u32>]) -> Vec<usize> {
    let mut found = HashSet::new();
    let mut pending = calls.to_vec();
    while let Some(index) = pending.pop() {
        if found.insert(index) {
            pending.extend_from_slice(&edges[index as usize]);
        }
    }
    let mut dependencies: Vec<_> = found.into_iter().map(|index| index as usize).collect();
    dependencies.sort_unstable();
    dependencies
}

/// Give a frame a block and every field a window into it.
fn place_frame(buffers: &mut Vec<Place>, frame: &Frame) -> (BufId, Vec<BufId>) {
    let block = alloc(buffers, frame.bytes);
    let fields = frame
        .fields
        .iter()
        .map(|f| field(buffers, block, f.offset))
        .collect();
    (block, fields)
}

struct FnFrame {
    buffer_abi: bool,
    arg_buffers: Vec<BufId>,
    ret_buffer: Option<BufId>,
}

/// Functions that both take part in a call cycle and cross tensor buffers.
fn tensor_cycles(edges: &[Vec<u32>], frames: &[FnFrame]) -> Vec<u32> {
    let mut cyclic = Vec::new();
    for start in 0..edges.len() as u32 {
        if !frames[start as usize].buffer_abi {
            continue;
        }
        let mut seen = HashSet::new();
        let mut stack = vec![start];
        while let Some(f) = stack.pop() {
            for &next in &edges[f as usize] {
                if next == start {
                    cyclic.push(start);
                    stack.clear();
                    break;
                }
                if seen.insert(next) {
                    stack.push(next);
                }
            }
        }
    }
    cyclic
}

fn alloc(buffers: &mut Vec<Place>, bytes: u32) -> BufId {
    buffers.push(Place::Block(bytes));
    (buffers.len() - 1) as BufId
}

fn field(buffers: &mut Vec<Place>, parent: BufId, offset: u32) -> BufId {
    buffers.push(Place::Field { parent, offset });
    (buffers.len() - 1) as BufId
}

/// Bytes a value of this type occupies in a buffer.
fn slot_bytes(ty: &Ty) -> u32 {
    elems(ty) * 8
}

fn elems(ty: &Ty) -> u32 {
    match ty {
        Ty::Tensor { shape, .. } => shape.iter().product::<usize>() as u32,
        _ => 1,
    }
}

pub(crate) fn annotation(expr: &ast::Expr, diags: &mut Diagnostics) -> Ty {
    match expr {
        ast::Expr::Name(name) => match name.id.as_str() {
            "f64" | "float" => Ty::F64,
            "i64" | "int" => Ty::I64,
            "bool" => Ty::Bool,
            other => {
                diags.push(
                    expr.range(),
                    format!("unknown type `{other}`; expected f64, i64, bool, or Tensor[...]"),
                );
                Ty::F64
            }
        },
        ast::Expr::Subscript(sub) => tensor_annotation(sub, diags),
        _ => {
            diags.push(expr.range(), "expected a type name");
            Ty::F64
        }
    }
}

/// `Tensor[f64, 3]` and `Tensor[f64, (3, 3)]`.
fn tensor_annotation(sub: &ast::ExprSubscript, diags: &mut Diagnostics) -> Ty {
    fn bad(sub: &ast::ExprSubscript, diags: &mut Diagnostics) -> Ty {
        diags.push(
            sub.range,
            "expected Tensor[f64, N] or Tensor[f64, (N, M)] with positive constant dimensions",
        );
        Ty::F64
    }
    let ast::Expr::Name(head) = sub.value.as_ref() else {
        return bad(sub, diags);
    };
    if head.id.as_str() != "Tensor" {
        return bad(sub, diags);
    }
    let ast::Expr::Tuple(parts) = sub.slice.as_ref() else {
        return bad(sub, diags);
    };
    if parts.elts.len() != 2 {
        return bad(sub, diags);
    }
    let dtype = match &parts.elts[0] {
        ast::Expr::Name(n) if matches!(n.id.as_str(), "f64" | "float") => Dtype::F64,
        other => {
            diags.push(other.range(), "only Tensor[f64, ...] exists in this phase");
            return Ty::F64;
        }
    };
    let dims: Vec<&ast::Expr> = match &parts.elts[1] {
        ast::Expr::Tuple(dims) => dims.elts.iter().collect(),
        single => vec![single],
    };
    if dims.is_empty() || dims.len() > MAX_RANK {
        return bad(sub, diags);
    }
    let mut shape = Vec::with_capacity(dims.len());
    for dim in dims {
        let ast::Expr::NumberLiteral(c) = dim else {
            return bad(sub, diags);
        };
        let ast::Number::Int(n) = &c.value else {
            return bad(sub, diags);
        };
        match n.as_usize() {
            Some(n) if n > 0 => shape.push(n),
            _ => return bad(sub, diags),
        }
    }
    Ty::Tensor { dtype, shape }
}

/// nox's broadcast rule: shapes right-aligned, an extent of one stretches.
fn broadcast(a: &[usize], b: &[usize]) -> Option<Vec<usize>> {
    let rank = a.len().max(b.len());
    let mut out = Vec::with_capacity(rank);
    for i in 0..rank {
        out.push(match (axis(a, rank, i), axis(b, rank, i)) {
            (x, y) if x == y => x,
            (1, y) => y,
            (x, 1) => x,
            _ => return None,
        });
    }
    Some(out)
}

fn axis(shape: &[usize], rank: usize, i: usize) -> usize {
    let lead = rank - shape.len();
    if i < lead { 1 } else { shape[i - lead] }
}

fn padded(shape: &[usize], rank: usize) -> Vec<u32> {
    (0..rank).map(|i| axis(shape, rank, i) as u32).collect()
}

/// A value the host owns a buffer for: a frame field, a state slot, or a
/// projected port.
#[derive(Clone)]
struct Cell {
    buf: BufId,
    ty: Ty,
}

enum Binding {
    Scalar {
        slot: u32,
        ty: Ty,
    },
    Tensor {
        buf: BufId,
        ty: Ty,
    },
    Cell {
        cell: Cell,
        writable: bool,
        /// The sample's timestamp, when this is an input port.
        stamp: Option<BufId>,
    },
    /// A frame or state parameter, addressed field by field. Input frames are
    /// read-only; state is what a system is allowed to keep.
    Record {
        fields: Vec<(String, Cell)>,
        writable: bool,
        /// The sample's timestamp, when this is an input port.
        stamp: Option<BufId>,
    },
}

/// Where a system's `return` goes.
struct Output {
    frame: Frame,
    fields: Vec<BufId>,
    /// The class a `return Frame(...)` must name, absent for a one-field
    /// output the body returns bare.
    class: Option<String>,
    anonymous: bool,
}

struct FnChecker<'a> {
    diags: &'a mut Diagnostics,
    sigs: &'a [FnSig],
    by_name: &'a HashMap<String, u32>,
    frames: &'a [FnFrame],
    buffers: &'a mut Vec<Place>,
    ret: Ty,
    ret_buffer: Option<BufId>,
    buffer_abi: bool,
    output: Option<Output>,
    /// The slot `now()` reads, in a system.
    now: Option<u32>,
    /// State the body asked for that no `State` class declared: the word
    /// `random()` advances, one ring per `window` call site, one previous
    /// sample per `delta` and `deltat`. Each is allocated where it is first
    /// needed, with the default it starts from, so a system that asks for
    /// none carries no state at all.
    synthetic: Vec<(String, Ty, BufId, crate::Init)>,
    locals: Vec<Ty>,
    names: HashMap<String, Binding>,
    loop_depth: u32,
    calls: Vec<u32>,
}

impl FnChecker<'_> {
    fn temp(&mut self, ty: Ty) -> u32 {
        self.locals.push(ty);
        (self.locals.len() - 1) as u32
    }

    fn buffer(&mut self, ty: &Ty) -> BufId {
        alloc(self.buffers, slot_bytes(ty))
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
                if self.output.is_some() {
                    return Some(vec![self.return_frame(value, depth)?]);
                }
                let (expr, ty) = self.expr(value, depth)?;
                let want = self.ret.clone();
                let expr = self.coerce(expr, ty, &want, value.range())?;
                Some(vec![if self.buffer_abi {
                    Stmt::ReturnBuffer {
                        dest: self.ret_buffer.expect("a buffer-ABI function has one"),
                        value: expr,
                        ty: want,
                    }
                } else {
                    Stmt::Return(expr)
                }])
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
                Some(vec![self.bind(target.id.as_str(), ty, expr)])
            }
            ast::Stmt::Assign(assign) => {
                if assign.targets.len() != 1 {
                    self.diags
                        .push(assign.range, "chained assignment is not supported");
                    return None;
                }
                match &assign.targets[0] {
                    ast::Expr::Attribute(_) => {
                        let (value, got) = self.expr(&assign.value, depth)?;
                        let cell = self.writable_cell(&assign.targets[0])?;
                        let value = self.coerce(value, got, &cell.ty, assign.value.range())?;
                        Some(vec![self.write_cell(&cell, value)])
                    }
                    ast::Expr::Name(target) => {
                        let (expr, got) = self.expr(&assign.value, depth)?;
                        let name = target.id.as_str();
                        match self.names.get(name) {
                            Some(Binding::Scalar { slot, ty }) => {
                                let (slot, want) = (*slot, ty.clone());
                                let expr = self.coerce(expr, got, &want, assign.value.range())?;
                                Some(vec![Stmt::Assign {
                                    local: slot,
                                    value: expr,
                                }])
                            }
                            Some(Binding::Tensor { buf, ty }) => {
                                let (buf, want) = (*buf, ty.clone());
                                let expr = self.coerce(expr, got, &want, assign.value.range())?;
                                Some(vec![Stmt::TensorAssign {
                                    dest: buf,
                                    value: expr,
                                    bytes: slot_bytes(&want),
                                }])
                            }
                            Some(Binding::Cell {
                                cell,
                                writable: true,
                                ..
                            }) => {
                                let cell = cell.clone();
                                let expr =
                                    self.coerce(expr, got, &cell.ty, assign.value.range())?;
                                Some(vec![self.write_cell(&cell, expr)])
                            }
                            Some(_) => {
                                self.diags.push(
                                    target.range,
                                    format!("`{name}` is an input; it cannot be assigned"),
                                );
                                None
                            }
                            None => Some(vec![self.bind(name, got, expr)]),
                        }
                    }
                    ast::Expr::Subscript(sub) => {
                        let (value, got) = self.expr(&assign.value, depth)?;
                        let value = self.coerce(value, got, &Ty::F64, assign.value.range())?;
                        let (target, index, len) = self.index_of(sub, depth)?;
                        Some(vec![Stmt::ElementAssign {
                            target,
                            index,
                            value,
                            len,
                        }])
                    }
                    other => {
                        self.diags
                            .push(other.range(), "only names and elements can be assigned");
                        None
                    }
                }
            }
            ast::Stmt::AugAssign(aug) => {
                if matches!(aug.target.as_ref(), ast::Expr::Attribute(_)) {
                    let cell = self.writable_cell(&aug.target)?;
                    let lhs = (self.read_cell(&cell), cell.ty.clone());
                    let rhs = self.expr(&aug.value, depth)?;
                    let (expr, got) = self.binop(aug.op, lhs, rhs, &aug.value, aug.range)?;
                    let expr = self.coerce(expr, got, &cell.ty, aug.range)?;
                    return Some(vec![self.write_cell(&cell, expr)]);
                }
                let ast::Expr::Name(target) = aug.target.as_ref() else {
                    self.diags
                        .push(aug.target.range(), "only plain names can be assigned");
                    return None;
                };
                let name = target.id.as_str();
                let (lhs, dest) = match self.names.get(name) {
                    Some(Binding::Scalar { slot, ty }) => {
                        ((Expr::Local(*slot), ty.clone()), Err(*slot))
                    }
                    Some(Binding::Tensor { buf, ty }) => {
                        ((Expr::Tensor(*buf), ty.clone()), Ok(*buf))
                    }
                    Some(Binding::Cell {
                        cell,
                        writable: true,
                        ..
                    }) => {
                        let cell = cell.clone();
                        let lhs = (self.read_cell(&cell), cell.ty.clone());
                        let rhs = self.expr(&aug.value, depth)?;
                        let (expr, got) = self.binop(aug.op, lhs, rhs, &aug.value, aug.range)?;
                        let expr = self.coerce(expr, got, &cell.ty, aug.range)?;
                        return Some(vec![self.write_cell(&cell, expr)]);
                    }
                    Some(_) => {
                        self.diags.push(
                            aug.target.range(),
                            format!("`{name}` is an input; it cannot be assigned"),
                        );
                        return None;
                    }
                    None => {
                        self.diags
                            .push(aug.target.range(), format!("`{name}` is not defined"));
                        return None;
                    }
                };
                let want = lhs.1.clone();
                let rhs = self.expr(&aug.value, depth)?;
                let (expr, got) = self.binop(aug.op, lhs, rhs, &aug.value, aug.range)?;
                let expr = self.coerce(expr, got, &want, aug.range)?;
                Some(vec![match dest {
                    Ok(buf) => Stmt::TensorAssign {
                        dest: buf,
                        value: expr,
                        bytes: slot_bytes(&want),
                    },
                    Err(slot) => Stmt::Assign {
                        local: slot,
                        value: expr,
                    },
                }])
            }
            ast::Stmt::If(branch) => {
                let cond = self.condition(&branch.test, depth)?;
                let then = self.block(&branch.body, depth + 1);
                let els = self.otherwise(&branch.elif_else_clauses, depth + 1);
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
            ast::Stmt::For(loop_) if !loop_.is_async => self.for_range(loop_, depth),
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
                let (expr, ty) = self.expr(&e.value, depth)?;
                if matches!(ty, Ty::Tensor { .. }) {
                    // The kernels already ran; the address is what is dropped.
                    return Some(vec![Stmt::TensorAssign {
                        dest: expr.buffer(),
                        value: expr,
                        bytes: 0,
                    }]);
                }
                Some(vec![Stmt::Drop(expr)])
            }
            other => {
                self.diags.push(other.range(), refusal(other));
                None
            }
        }
    }

    /// The `elif`/`else` tail of an `if`, nested back into the chain of
    /// two-armed branches the IR is written in.
    fn otherwise(&mut self, clauses: &[ast::ElifElseClause], depth: u32) -> Vec<Stmt> {
        let Some((clause, rest)) = clauses.split_first() else {
            return Vec::new();
        };
        let Some(test) = &clause.test else {
            return self.block(&clause.body, depth);
        };
        if depth > MAX_DEPTH {
            self.diags.push(clause.range, "nested too deeply");
            return Vec::new();
        }
        let Some(cond) = self.condition(test, depth) else {
            return Vec::new();
        };
        let then = self.block(&clause.body, depth + 1);
        let els = self.otherwise(rest, depth + 1);
        vec![Stmt::If { cond, then, els }]
    }

    fn read(&mut self, name: &str, range: TextRange) -> Option<(Expr, Ty)> {
        match self.names.get(name) {
            Some(Binding::Scalar { slot, ty }) => Some((Expr::Local(*slot), ty.clone())),
            Some(Binding::Tensor { buf, ty }) => Some((Expr::Tensor(*buf), ty.clone())),
            Some(Binding::Cell { cell, .. }) => {
                let cell = cell.clone();
                Some((self.read_cell(&cell), cell.ty))
            }
            Some(Binding::Record { .. }) => {
                self.diags.push(
                    range,
                    format!("`{name}` is a frame; name one of its fields"),
                );
                None
            }
            None => {
                self.diags.push(range, format!("`{name}` is not defined"));
                None
            }
        }
    }

    /// Read a value out of the buffer the host owns it in.
    fn read_cell(&self, cell: &Cell) -> Expr {
        match cell.ty {
            Ty::Tensor { .. } => Expr::Tensor(cell.buf),
            _ => Expr::Load {
                buf: cell.buf,
                ty: cell.ty.clone(),
            },
        }
    }

    fn write_cell(&self, cell: &Cell, value: Expr) -> Stmt {
        match cell.ty {
            Ty::Tensor { .. } => Stmt::TensorAssign {
                dest: cell.buf,
                value,
                bytes: slot_bytes(&cell.ty),
            },
            _ => Stmt::Store {
                dest: cell.buf,
                value,
                ty: cell.ty.clone(),
            },
        }
    }

    /// The cell a dotted path names, refusing anything a body may not write.
    fn writable_cell(&mut self, target: &ast::Expr) -> Option<Cell> {
        let ast::Expr::Attribute(attr) = target else {
            self.diags
                .push(target.range(), "only names and fields can be assigned");
            return None;
        };
        let Some(head) = lang::dotted(&attr.value) else {
            self.diags
                .push(target.range(), "only names and fields can be assigned");
            return None;
        };
        match self.names.get(&head) {
            Some(Binding::Record {
                fields,
                writable: true,
                ..
            }) => match fields.iter().find(|(name, _)| name == attr.attr.as_str()) {
                Some((_, cell)) => Some(cell.clone()),
                None => {
                    self.diags.push(
                        target.range(),
                        format!("`{head}` has no field `{}`", attr.attr.as_str()),
                    );
                    None
                }
            },
            Some(_) => {
                self.diags.push(
                    target.range(),
                    format!("`{head}` is an input; it cannot be assigned"),
                );
                None
            }
            None => {
                self.diags
                    .push(target.range(), format!("`{head}` is not defined"));
                None
            }
        }
    }

    /// A system's `return`: either the output frame constructed field by
    /// field, or a bare value filling a one-field anonymous frame.
    fn return_frame(&mut self, value: &ast::Expr, depth: u32) -> Option<Stmt> {
        let output = self.output.as_ref().expect("only a system returns a frame");
        let (frame, fields, class, anonymous) = (
            output.frame.clone(),
            output.fields.clone(),
            output.class.clone(),
            output.anonymous,
        );

        if anonymous {
            let ty = frame.fields[0].ty.clone();
            let (expr, got) = self.expr(value, depth)?;
            let expr = self.coerce(expr, got, &ty, value.range())?;
            return Some(Stmt::ReturnFrame(vec![FieldWrite {
                dest: fields[0],
                value: expr,
                ty,
            }]));
        }

        let class = class.expect("a declared output frame names its class");
        let ast::Expr::Call(call) = value else {
            self.diags
                .push(value.range(), format!("a system returns `{class}(...)`"));
            return None;
        };
        let named = matches!(call.func.as_ref(), ast::Expr::Name(n) if n.id.as_str() == class);
        if !named || !call.arguments.args.is_empty() {
            self.diags.push(
                value.range(),
                format!("a system returns `{class}(...)` with one keyword per field"),
            );
            return None;
        }

        let mut writes = Vec::with_capacity(frame.fields.len());
        for (field, dest) in frame.fields.iter().zip(&fields) {
            let Some(keyword) = call
                .arguments
                .keywords
                .iter()
                .find(|k| k.arg.as_ref().is_some_and(|a| a.as_str() == field.name))
            else {
                self.diags
                    .push(call.range, format!("`{}` is not given a value", field.name));
                return None;
            };
            let (expr, got) = self.expr(&keyword.value, depth)?;
            let expr = self.coerce(expr, got, &field.ty, keyword.value.range())?;
            writes.push(FieldWrite {
                dest: *dest,
                value: expr,
                ty: field.ty.clone(),
            });
        }
        for keyword in &call.arguments.keywords {
            let unknown = keyword
                .arg
                .as_ref()
                .is_none_or(|a| frame.field(a.as_str()).is_none());
            if unknown {
                self.diags
                    .push(keyword.range, format!("`{class}` has no such field"));
                return None;
            }
        }
        Some(Stmt::ReturnFrame(writes))
    }

    /// Introduce a name, in a wasm local or a buffer depending on its type.
    fn bind(&mut self, name: &str, ty: Ty, value: Expr) -> Stmt {
        if matches!(ty, Ty::Tensor { .. }) {
            let buf = self.buffer(&ty);
            let bytes = slot_bytes(&ty);
            self.names
                .insert(name.to_string(), Binding::Tensor { buf, ty });
            Stmt::TensorAssign {
                dest: buf,
                value,
                bytes,
            }
        } else {
            let slot = self.temp(ty.clone());
            self.names
                .insert(name.to_string(), Binding::Scalar { slot, ty });
            Stmt::Assign { local: slot, value }
        }
    }

    /// `for i in range(...)`, the only iteration the subset has.
    fn for_range(&mut self, loop_: &ast::StmtFor, depth: u32) -> Option<Vec<Stmt>> {
        if !loop_.orelse.is_empty() {
            self.diags
                .push(loop_.range, "`for ... else` is not supported");
        }
        let ast::Expr::Name(target) = loop_.target.as_ref() else {
            self.diags.push(
                loop_.target.range(),
                "the loop variable must be a plain name",
            );
            return None;
        };
        let ast::Expr::Call(call) = loop_.iter.as_ref() else {
            self.diags
                .push(loop_.iter.range(), "`for` iterates `range(...)` only");
            return None;
        };
        let is_range = matches!(call.func.as_ref(), ast::Expr::Name(n) if n.id.as_str() == "range");
        if !is_range || call.arguments.args.is_empty() || call.arguments.args.len() > 3 {
            self.diags
                .push(loop_.iter.range(), "`for` iterates `range(...)` only");
            return None;
        }
        let mut bounds = Vec::new();
        for arg in &call.arguments.args {
            let (expr, ty) = self.expr(arg, depth)?;
            bounds.push(self.coerce(expr, ty, &Ty::I64, arg.range())?);
        }
        let (start, stop, step) = match bounds.len() {
            1 => (Expr::I64(0), bounds.remove(0), Expr::I64(1)),
            2 => {
                let stop = bounds.remove(1);
                (bounds.remove(0), stop, Expr::I64(1))
            }
            _ => {
                let step = bounds.remove(2);
                let stop = bounds.remove(1);
                (bounds.remove(0), stop, step)
            }
        };

        let slot = self.temp(Ty::I64);
        self.names.insert(
            target.id.as_str().to_string(),
            Binding::Scalar { slot, ty: Ty::I64 },
        );
        let limit = self.temp(Ty::I64);
        let stride = self.temp(Ty::I64);

        self.loop_depth += 1;
        let mut body = self.block(&loop_.body, depth + 1);
        self.loop_depth -= 1;
        body.push(Stmt::Assign {
            local: slot,
            value: Expr::Arith {
                op: Arith::Add,
                ty: Num::I64,
                lhs: Box::new(Expr::Local(slot)),
                rhs: Box::new(Expr::Local(stride)),
            },
        });

        // A negative step counts down, so the guard flips with it.
        let cond = Expr::Select {
            cond: Box::new(Expr::Cmp {
                op: Cmp::Gt,
                ty: Num::I64,
                lhs: Box::new(Expr::Local(stride)),
                rhs: Box::new(Expr::I64(0)),
            }),
            then: Box::new(Expr::Cmp {
                op: Cmp::Lt,
                ty: Num::I64,
                lhs: Box::new(Expr::Local(slot)),
                rhs: Box::new(Expr::Local(limit)),
            }),
            els: Box::new(Expr::Cmp {
                op: Cmp::Gt,
                ty: Num::I64,
                lhs: Box::new(Expr::Local(slot)),
                rhs: Box::new(Expr::Local(limit)),
            }),
            ty: Ty::Bool,
        };

        Some(vec![
            Stmt::Assign {
                local: slot,
                value: start,
            },
            Stmt::Assign {
                local: limit,
                value: stop,
            },
            Stmt::Assign {
                local: stride,
                value: step,
            },
            Stmt::While { cond, body },
        ])
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
                    format!("arithmetic needs numbers, found {} and {}", lhs.1, rhs.1),
                );
                None
            }
        }
    }

    fn as_f64(&mut self, expr: Expr, ty: Ty, range: TextRange) -> Option<Expr> {
        self.coerce(expr, ty, &Ty::F64, range)
    }

    /// Put a scalar in a one-element buffer so it can broadcast.
    fn splat(&mut self, expr: Expr, ty: Ty, range: TextRange) -> Option<(Expr, Vec<usize>)> {
        match ty {
            Ty::Tensor { shape, .. } => Some((expr, shape)),
            other => {
                let value = self.as_f64(expr, other, range)?;
                let dest = self.buffer(&Ty::F64);
                Some((
                    Expr::Splat {
                        dest,
                        value: Box::new(value),
                    },
                    vec![1],
                ))
            }
        }
    }

    fn elementwise(
        &mut self,
        kernel: &'static str,
        lhs: (Expr, Ty),
        rhs: (Expr, Ty),
        range: TextRange,
    ) -> Option<(Expr, Ty)> {
        let (lhs_expr, lhs_shape) = self.splat(lhs.0, lhs.1, range)?;
        let (rhs_expr, rhs_shape) = self.splat(rhs.0, rhs.1, range)?;
        let Some(out_shape) = broadcast(&lhs_shape, &rhs_shape) else {
            self.diags.push(
                range,
                format!("shapes {lhs_shape:?} and {rhs_shape:?} do not broadcast"),
            );
            return None;
        };
        let rank = out_shape.len();
        let ty = Ty::Tensor {
            dtype: Dtype::F64,
            shape: out_shape.clone(),
        };
        let dest = self.buffer(&ty);
        Some((
            Expr::Elementwise {
                kernel,
                dest,
                lhs: Box::new(lhs_expr),
                rhs: Box::new(rhs_expr),
                desc: Desc {
                    rank: rank as u32,
                    lhs: padded(&lhs_shape, rank),
                    rhs: padded(&rhs_shape, rank),
                    out: padded(&out_shape, rank),
                },
                emit: emit_for(out_shape.iter().product()),
            },
            ty,
        ))
    }

    fn binop(
        &mut self,
        op: ast::Operator,
        lhs: (Expr, Ty),
        rhs: (Expr, Ty),
        rhs_ast: &ast::Expr,
        range: TextRange,
    ) -> Option<(Expr, Ty)> {
        if matches!(lhs.1, Ty::Tensor { .. }) || matches!(rhs.1, Ty::Tensor { .. }) {
            let kernel = match op {
                ast::Operator::Add => "k_add",
                ast::Operator::Sub => "k_sub",
                ast::Operator::Mult => "k_mul",
                ast::Operator::Div => "k_div",
                ast::Operator::Pow => return self.tensor_pow(lhs, rhs, rhs_ast, range),
                ast::Operator::MatMult => return self.matmul(lhs, rhs, range),
                _ => {
                    self.diags
                        .push(range, "tensors support `+ - * / **` in this phase");
                    return None;
                }
            };
            return self.elementwise(kernel, lhs, rhs, range);
        }

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
            ast::Operator::MatMult => return self.matmul(lhs, rhs, range),
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
        if let Some(n) = literal_exponent(rhs_ast) {
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

    /// The same rule as scalars, one kernel call per multiplication.
    fn tensor_pow(
        &mut self,
        lhs: (Expr, Ty),
        rhs: (Expr, Ty),
        rhs_ast: &ast::Expr,
        range: TextRange,
    ) -> Option<(Expr, Ty)> {
        let Some(n) = literal_exponent(rhs_ast) else {
            return self.elementwise("k_pow", lhs, rhs, range);
        };
        if !matches!(lhs.1, Ty::Tensor { .. }) || n == 0 {
            return self.elementwise("k_pow", lhs, rhs, range);
        }
        let base = Expr::Tensor(lhs.0.buffer());
        let base_ty = lhs.1.clone();
        let mut acc = lhs;
        for _ in 1..n {
            let next = match &base {
                Expr::Tensor(id) => Expr::Tensor(*id),
                _ => unreachable!("a tensor operand has a buffer"),
            };
            acc = self.elementwise("k_mul", acc, (next, base_ty.clone()), range)?;
        }
        Some(acc)
    }

    fn expr(&mut self, expr: &ast::Expr, depth: u32) -> Option<(Expr, Ty)> {
        if depth > MAX_DEPTH {
            self.diags.push(expr.range(), "nested too deeply");
            return None;
        }
        let depth = depth + 1;
        match expr {
            ast::Expr::NumberLiteral(c) => match &c.value {
                ast::Number::Float(f) => Some((Expr::F64(*f), Ty::F64)),
                ast::Number::Int(i) => match i.as_i64() {
                    Some(v) => Some((Expr::I64(v), Ty::I64)),
                    None => {
                        self.diags.push(
                            c.range,
                            "integer literal does not fit in i64; there are no bignums here",
                        );
                        None
                    }
                },
                ast::Number::Complex { .. } => {
                    self.diags
                        .push(c.range, "only numeric and bool literals are supported");
                    None
                }
            },
            ast::Expr::BooleanLiteral(b) => Some((Expr::Bool(b.value), Ty::Bool)),
            ast::Expr::StringLiteral(_)
            | ast::Expr::BytesLiteral(_)
            | ast::Expr::NoneLiteral(_)
            | ast::Expr::EllipsisLiteral(_) => {
                self.diags
                    .push(expr.range(), "only numeric and bool literals are supported");
                None
            }
            ast::Expr::Name(name) => self.read(name.id.as_str(), name.range),
            ast::Expr::Attribute(attr) => {
                // A projected port's key is the whole dotted path the operator
                // typed; a frame parameter's is just its head.
                if let Some(path) = lang::dotted(expr)
                    && self.names.contains_key(&path)
                {
                    return self.read(&path, attr.range);
                }
                let head = lang::dotted(&attr.value)?;
                let Some(Binding::Record { fields, .. }) = self.names.get(&head) else {
                    self.diags
                        .push(attr.range, format!("`{head}` is not a frame here"));
                    return None;
                };
                match fields.iter().find(|(name, _)| name == attr.attr.as_str()) {
                    Some((_, cell)) => {
                        let cell = cell.clone();
                        Some((self.read_cell(&cell), cell.ty))
                    }
                    None => {
                        self.diags.push(
                            attr.range,
                            format!("`{head}` has no field `{}`", attr.attr.as_str()),
                        );
                        None
                    }
                }
            }
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
                        Ty::F64 | Ty::I64 | Ty::Tensor { .. } => Some((operand, ty)),
                        other => {
                            self.diags
                                .push(u.range, format!("unary `+` needs a number, found {other}"));
                            None
                        }
                    },
                    ast::UnaryOp::USub => {
                        if matches!(ty, Ty::Tensor { .. }) {
                            let dest = self.buffer(&ty);
                            let count = elems(&ty);
                            return Some((
                                Expr::TensorNeg {
                                    dest,
                                    operand: Box::new(operand),
                                    elems: count,
                                    emit: emit_for(count as usize),
                                },
                                ty,
                            ));
                        }
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
            ast::Expr::If(c) => {
                let cond = self.condition(&c.test, depth)?;
                let (then, then_ty) = self.expr(&c.body, depth)?;
                let (els, els_ty) = self.expr(&c.orelse, depth)?;
                if matches!(then_ty, Ty::Tensor { .. }) || matches!(els_ty, Ty::Tensor { .. }) {
                    self.diags
                        .push(c.range, "a conditional expression cannot yield a tensor");
                    return None;
                }
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
            ast::Expr::Subscript(sub) => {
                let (source, index, len) = self.index_of(sub, depth)?;
                Some((
                    Expr::Element {
                        source: Box::new(source),
                        index: Box::new(index),
                        len,
                    },
                    Ty::F64,
                ))
            }
            ast::Expr::Call(call) => self.call(call, depth),
            ast::Expr::List(list) => self.tensor_literal(list, depth),
            other => {
                self.diags.push(other.range(), expr_refusal(other));
                None
            }
        }
    }

    /// `v[i]` and `m[i, j]`, flattened to one row-major offset.
    fn index_of(&mut self, sub: &ast::ExprSubscript, depth: u32) -> Option<(Expr, Expr, u32)> {
        let (source, ty) = self.expr(&sub.value, depth)?;
        let Ty::Tensor { shape, .. } = ty else {
            self.diags
                .push(sub.range, format!("{ty} cannot be indexed"));
            return None;
        };
        let indices: Vec<&ast::Expr> = match sub.slice.as_ref() {
            ast::Expr::Tuple(parts) => parts.elts.iter().collect(),
            single => vec![single],
        };
        if indices.len() != shape.len() {
            self.diags.push(
                sub.range,
                format!(
                    "a rank-{} tensor needs {} indices, found {}",
                    shape.len(),
                    shape.len(),
                    indices.len()
                ),
            );
            return None;
        }

        let len: u32 = shape.iter().product::<usize>() as u32;
        let mut offset: Option<Expr> = None;
        for (which, index) in indices.iter().enumerate() {
            let stride: i64 = shape[which + 1..].iter().product::<usize>() as i64;
            let (lowered, index_ty) = self.expr(index, depth)?;
            let lowered = self.coerce(lowered, index_ty, &Ty::I64, index.range())?;
            let lowered = match lowered {
                Expr::I64(constant) => {
                    if !(0..shape[which] as i64).contains(&constant) {
                        self.diags.push(
                            index.range(),
                            format!(
                                "index {constant} is outside 0..{} for this axis",
                                shape[which]
                            ),
                        );
                        return None;
                    }
                    Expr::I64(constant)
                }
                value => Expr::CheckedIndex {
                    value: Box::new(value),
                    len: shape[which] as u32,
                },
            };
            let term = if stride == 1 {
                lowered
            } else {
                Expr::Arith {
                    op: Arith::Mul,
                    ty: Num::I64,
                    lhs: Box::new(lowered),
                    rhs: Box::new(Expr::I64(stride)),
                }
            };
            offset = Some(match offset {
                None => term,
                Some(acc) => Expr::Arith {
                    op: Arith::Add,
                    ty: Num::I64,
                    lhs: Box::new(acc),
                    rhs: Box::new(term),
                },
            });
        }
        Some((source, offset.expect("rank is at least one"), len))
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
        for (_, ty) in &operands {
            if matches!(ty, Ty::Tensor { .. }) {
                self.diags.push(c.range, "tensors do not compare");
                return None;
            }
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
            built = Expr::Store {
                local: slots[i],
                value: Box::new(expr),
                then: Box::new(built),
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
        if matches!(lhs.1, Ty::Tensor { .. }) || matches!(rhs.1, Ty::Tensor { .. }) {
            self.diags.push(range, "tensors do not compare");
            return None;
        }
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
        if !call.arguments.keywords.is_empty() {
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
            if sig.params.len() != call.arguments.args.len() {
                let want = sig.params.len();
                let got = call.arguments.args.len();
                self.diags.push(
                    call.range,
                    format!("`{name}` takes {want} arguments, found {got}"),
                );
                return None;
            }
            self.calls.push(index);
            let wanted: Vec<Ty> = sig.params.iter().map(|(_, t)| t.clone()).collect();
            let ret = sig.ret.clone();
            let buffer_abi = self.frames[index as usize].buffer_abi;
            let mut args = Vec::with_capacity(wanted.len());
            for (arg, want) in call.arguments.args.iter().zip(&wanted) {
                let (lowered, got) = self.expr(arg, depth)?;
                args.push(self.coerce(lowered, got, want, arg.range())?);
            }
            return Some(if buffer_abi {
                let dest = matches!(ret, Ty::Tensor { .. }).then(|| self.buffer(&ret));
                (
                    Expr::BufferCall {
                        index,
                        args,
                        dest,
                        ret: ret.clone(),
                    },
                    ret,
                )
            } else {
                (Expr::Call { index, args }, ret)
            });
        }

        self.builtin(name, call, depth)
    }

    fn builtin(&mut self, name: &str, call: &ast::ExprCall, depth: u32) -> Option<(Expr, Ty)> {
        let arity = |n: usize, this: &mut Self| -> bool {
            if call.arguments.args.len() == n {
                true
            } else {
                this.diags.push(
                    call.range,
                    format!(
                        "`{name}` takes {n} arguments, found {}",
                        call.arguments.args.len()
                    ),
                );
                false
            }
        };

        match name {
            "dot" => {
                self.diags
                    .push(call.range, "`dot(a, b)` is written `a @ b`");
                return None;
            }
            "sum" => {
                if !arity(1, self) {
                    return None;
                }
                let (source, ty) = self.expr(&call.arguments.args[0], depth)?;
                if !matches!(ty, Ty::Tensor { .. }) {
                    self.diags
                        .push(call.range, format!("`sum` needs a tensor, found {ty}"));
                    return None;
                }
                let count = elems(&ty);
                return Some((
                    Expr::Sum {
                        source: Box::new(source),
                        elems: count,
                        emit: emit_for(count as usize),
                    },
                    Ty::F64,
                ));
            }
            "now" => {
                if !arity(0, self) {
                    return None;
                }
                return match self.now {
                    Some(slot) => Some((Expr::Local(slot), Ty::I64)),
                    None => {
                        self.diags
                            .push(call.range, "`now()` exists inside a system");
                        None
                    }
                };
            }
            // The generators. A source system's body is where a test signal
            // comes from now that `@system(rate=)` supplies the clock, so the
            // legacy Waveform, Random, and Constant nodes are these calls.
            "constant" => {
                if !arity(1, self) {
                    return None;
                }
                return self.expr(&call.arguments.args[0], depth);
            }
            "sine" | "cosine" | "square" | "sawtooth" => {
                if !arity(2, self) {
                    return None;
                }
                return self.waveform(name, call, depth);
            }
            "random" => {
                if !arity(0, self) {
                    return None;
                }
                if self.now.is_none() {
                    self.diags
                        .push(call.range, "`random()` exists inside a system");
                    return None;
                }
                // One word for the whole system: every call site advances the
                // same sequence, which is what makes it one generator.
                let buf = match self
                    .synthetic
                    .iter()
                    .find(|(name, ..)| name == crate::state::RNG_FIELD)
                {
                    Some((_, _, buf, _)) => *buf,
                    None => self.keep(
                        crate::state::RNG_FIELD.to_string(),
                        Ty::I64,
                        crate::Init::I64(0),
                    ),
                };
                return Some((
                    Expr::Kernel {
                        name: "rng_unit",
                        args: vec![Expr::Address(buf)],
                    },
                    Ty::F64,
                ));
            }
            // Resampling changes which clock a value ticks on, so it is
            // scheduling rather than arithmetic and the host owns it. The
            // shape is recognised at the top level; here it can only be a
            // mistake, and saying where it belongs is the whole diagnostic.
            "resample_zoh" | "resample_linear" => {
                self.diags.push(
                    call.range,
                    format!(
                        "`{name}` changes the clock, so it is a top-level binding of its own: \
                         `slow = {name}(fast, 10.0)`"
                    ),
                );
                return None;
            }
            "window" => {
                if !arity(2, self) {
                    return None;
                }
                return self.window(call, depth);
            }
            "delta" => {
                if !arity(1, self) {
                    return None;
                }
                return self.delta(call, depth);
            }
            "deltat" => {
                if !arity(1, self) {
                    return None;
                }
                return self.deltat(call);
            }
            "fft" => {
                if !arity(1, self) {
                    return None;
                }
                return self.fft(call, depth);
            }
            "len" => {
                if !arity(1, self) {
                    return None;
                }
                let (_, ty) = self.expr(&call.arguments.args[0], depth)?;
                let Ty::Tensor { shape, .. } = &ty else {
                    self.diags
                        .push(call.range, format!("`len` needs a tensor, found {ty}"));
                    return None;
                };
                return Some((Expr::I64(shape[0] as i64), Ty::I64));
            }
            _ => {}
        }

        if let Some(kernel) = transcendental(name) {
            if !arity(1, self) {
                return None;
            }
            let (arg, ty) = self.expr(&call.arguments.args[0], depth)?;
            let arg = self.as_f64(arg, ty, call.arguments.args[0].range())?;
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
            let (arg, ty) = self.expr(&call.arguments.args[0], depth)?;
            let arg = self.as_f64(arg, ty, call.arguments.args[0].range())?;
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
                let (a, at) = self.expr(&call.arguments.args[0], depth)?;
                let a = self.as_f64(a, at, call.arguments.args[0].range())?;
                let (b, bt) = self.expr(&call.arguments.args[1], depth)?;
                let b = self.as_f64(b, bt, call.arguments.args[1].range())?;
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
                let (arg, ty) = self.expr(&call.arguments.args[0], depth)?;
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
                let a = self.expr(&call.arguments.args[0], depth)?;
                let b = self.expr(&call.arguments.args[1], depth)?;
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
            // Conveniences spelled out of what the language already has, so
            // the prelude stays as it is and a `mean` costs what `sum` does.
            // `mean(x, n)` is `mean(window(x, n))`: the ring is the same one
            // `window` keeps, zeros included, so the average ramps in over
            // the first `n` samples.
            "mean" => {
                let (source, ty) = match call.arguments.args.len() {
                    2 => self.window(call, depth)?,
                    _ => {
                        if !arity(1, self) {
                            return None;
                        }
                        self.expr(&call.arguments.args[0], depth)?
                    }
                };
                if !matches!(ty, Ty::Tensor { .. }) {
                    self.diags
                        .push(call.range, format!("`mean` needs a tensor, found {ty}"));
                    return None;
                }
                let count = elems(&ty);
                Some((
                    Expr::Arith {
                        op: Arith::Div,
                        ty: Num::F64,
                        lhs: Box::new(Expr::Sum {
                            source: Box::new(source),
                            elems: count,
                            emit: emit_for(count as usize),
                        }),
                        rhs: Box::new(Expr::F64(count as f64)),
                    },
                    Ty::F64,
                ))
            }
            "clamp" => {
                if !arity(3, self) {
                    return None;
                }
                let x = self.expr(&call.arguments.args[0], depth)?;
                let lo = self.expr(&call.arguments.args[1], depth)?;
                let hi = self.expr(&call.arguments.args[2], depth)?;
                let (x, lo, num) = self.unify(x, lo, call.range)?;
                let ty = match num {
                    Num::F64 => Ty::F64,
                    Num::I64 => Ty::I64,
                };
                let floor = Expr::Intrinsic {
                    op: match num {
                        Num::F64 => Intrinsic::MaxF64,
                        Num::I64 => Intrinsic::MaxI64,
                    },
                    args: vec![x, lo],
                };
                let (floor, hi, num) = self.unify((floor, ty), hi, call.range)?;
                let ty = match num {
                    Num::F64 => Ty::F64,
                    Num::I64 => Ty::I64,
                };
                Some((
                    Expr::Intrinsic {
                        op: match num {
                            Num::F64 => Intrinsic::MinF64,
                            Num::I64 => Intrinsic::MinI64,
                        },
                        args: vec![floor, hi],
                    },
                    ty,
                ))
            }
            "sign" => {
                if !arity(1, self) {
                    return None;
                }
                let (arg, ty) = self.expr(&call.arguments.args[0], depth)?;
                let num = match ty {
                    Ty::F64 => Num::F64,
                    Ty::I64 => Num::I64,
                    other => {
                        self.diags
                            .push(call.range, format!("`sign` needs a number, found {other}"));
                        return None;
                    }
                };
                let lit = |v: i64| match num {
                    Num::F64 => Expr::F64(v as f64),
                    Num::I64 => Expr::I64(v),
                };
                let slot = self.temp(ty.clone());
                let cmp = |op| Expr::Cmp {
                    op,
                    ty: num,
                    lhs: Box::new(Expr::Local(slot)),
                    rhs: Box::new(lit(0)),
                };
                Some((
                    Expr::Store {
                        local: slot,
                        value: Box::new(arg),
                        then: Box::new(Expr::Select {
                            cond: Box::new(cmp(Cmp::Gt)),
                            then: Box::new(lit(1)),
                            els: Box::new(Expr::Select {
                                cond: Box::new(cmp(Cmp::Lt)),
                                then: Box::new(lit(-1)),
                                els: Box::new(lit(0)),
                                ty: ty.clone(),
                            }),
                            ty: ty.clone(),
                        }),
                    },
                    ty,
                ))
            }
            "hypot" => {
                if !arity(2, self) {
                    return None;
                }
                let (a, at) = self.expr(&call.arguments.args[0], depth)?;
                let a = self.as_f64(a, at, call.arguments.args[0].range())?;
                let (b, bt) = self.expr(&call.arguments.args[1], depth)?;
                let b = self.as_f64(b, bt, call.arguments.args[1].range())?;
                let (x, y) = (self.temp(Ty::F64), self.temp(Ty::F64));
                let square = |slot| Expr::Arith {
                    op: Arith::Mul,
                    ty: Num::F64,
                    lhs: Box::new(Expr::Local(slot)),
                    rhs: Box::new(Expr::Local(slot)),
                };
                Some((
                    Expr::Store {
                        local: x,
                        value: Box::new(a),
                        then: Box::new(Expr::Store {
                            local: y,
                            value: Box::new(b),
                            then: Box::new(Expr::Intrinsic {
                                op: Intrinsic::SqrtF64,
                                args: vec![Expr::Arith {
                                    op: Arith::Add,
                                    ty: Num::F64,
                                    lhs: Box::new(square(x)),
                                    rhs: Box::new(square(y)),
                                }],
                            }),
                        }),
                    },
                    Ty::F64,
                ))
            }
            "lerp" => {
                if !arity(3, self) {
                    return None;
                }
                let mut args = Vec::with_capacity(3);
                for arg in &call.arguments.args {
                    let (e, ty) = self.expr(arg, depth)?;
                    args.push(self.as_f64(e, ty, arg.range())?);
                }
                let t = args.pop()?;
                let b = args.pop()?;
                let a = args.pop()?;
                let slot = self.temp(Ty::F64);
                Some((
                    Expr::Store {
                        local: slot,
                        value: Box::new(a),
                        then: Box::new(Expr::Arith {
                            op: Arith::Add,
                            ty: Num::F64,
                            lhs: Box::new(Expr::Local(slot)),
                            rhs: Box::new(Expr::Arith {
                                op: Arith::Mul,
                                ty: Num::F64,
                                lhs: Box::new(Expr::Arith {
                                    op: Arith::Sub,
                                    ty: Num::F64,
                                    lhs: Box::new(b),
                                    rhs: Box::new(Expr::Local(slot)),
                                }),
                                rhs: Box::new(t),
                            }),
                        }),
                    },
                    Ty::F64,
                ))
            }
            "log2" | "log10" | "degrees" | "radians" => {
                if !arity(1, self) {
                    return None;
                }
                let (arg, ty) = self.expr(&call.arguments.args[0], depth)?;
                let arg = self.as_f64(arg, ty, call.arguments.args[0].range())?;
                let (lhs, scale) = match name {
                    "log2" => (kernel_log(arg), 1.0 / std::f64::consts::LN_2),
                    "log10" => (kernel_log(arg), 1.0 / std::f64::consts::LN_10),
                    "degrees" => (arg, 180.0 / std::f64::consts::PI),
                    _ => (arg, std::f64::consts::PI / 180.0),
                };
                Some((
                    Expr::Arith {
                        op: Arith::Mul,
                        ty: Num::F64,
                        lhs: Box::new(lhs),
                        rhs: Box::new(Expr::F64(scale)),
                    },
                    Ty::F64,
                ))
            }
            "int" => {
                if !arity(1, self) {
                    return None;
                }
                let (arg, ty) = self.expr(&call.arguments.args[0], depth)?;
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
                let (arg, ty) = self.expr(&call.arguments.args[0], depth)?;
                let arg = self.as_f64(arg, ty, call.arguments.args[0].range())?;
                Some((arg, Ty::F64))
            }
            other => {
                self.diags
                    .push(call.range, format!("`{other}` is not defined"));
                None
            }
        }
    }

    /// `[a, b, c]` and `[[a, b], [c, d]]`: a tensor, written out.
    ///
    /// A literal is a *constructor*, not a value of some list type. It types
    /// as a tensor the moment it is read, its length is its shape, and there
    /// is nothing to append to — which is why admitting this does not admit
    /// lists. Elements are scalars unified to one numeric type, and since
    /// tensors are `f64` in this phase that unification always lands on `f64`;
    /// what it decides is what to *refuse*, which is `bool` and anything that
    /// is itself a tensor.
    fn tensor_literal(&mut self, list: &ast::ExprList, depth: u32) -> Option<(Expr, Ty)> {
        if list.elts.is_empty() {
            self.diags
                .push(list.range, "a tensor literal needs at least one element");
            return None;
        }

        let rows: Vec<&ast::Expr> = list.elts.iter().collect();
        let nested = rows
            .iter()
            .filter(|e| matches!(e, ast::Expr::List(_)))
            .count();
        if nested != 0 && nested != rows.len() {
            self.diags.push(
                list.range,
                "a tensor literal is all rows or all elements, not a mixture",
            );
            return None;
        }

        let (shape, flat) = match nested {
            0 => (vec![rows.len()], rows),
            _ => {
                let mut flat = Vec::new();
                let mut cols = None;
                for row in &rows {
                    let ast::Expr::List(row) = row else {
                        unreachable!("every element was checked to be a row");
                    };
                    let width = *cols.get_or_insert(row.elts.len());
                    if row.elts.len() != width {
                        self.diags.push(
                            row.range,
                            format!(
                                "this row has {} elements but the first has {width}",
                                row.elts.len()
                            ),
                        );
                        return None;
                    }
                    if row.elts.iter().any(|e| matches!(e, ast::Expr::List(_))) {
                        self.diags
                            .push(row.range, "a tensor literal goes two deep, not three");
                        return None;
                    }
                    flat.extend(row.elts.iter());
                }
                if cols == Some(0) {
                    self.diags
                        .push(list.range, "a tensor literal needs at least one element");
                    return None;
                }
                (
                    vec![rows.len(), cols.expect("there is at least one row")],
                    flat,
                )
            }
        };

        let mut elements = Vec::with_capacity(flat.len());
        for element in flat {
            let (value, ty) = self.expr(element, depth)?;
            match ty {
                Ty::F64 | Ty::I64 => {
                    elements.push(self.as_f64(value, ty, element.range())?);
                }
                other => {
                    self.diags.push(
                        element.range(),
                        format!("a tensor literal holds numbers, found {other}"),
                    );
                    return None;
                }
            }
        }

        let ty = Ty::Tensor {
            dtype: Dtype::F64,
            shape,
        };
        let dest = self.buffer(&ty);
        Some((Expr::TensorLit { dest, elements }, ty))
    }

    /// Claim a state slot the body asked for but no `State` class declared.
    fn keep(&mut self, name: String, ty: Ty, default: crate::Init) -> BufId {
        let buf = self.buffer(&ty);
        self.synthetic.push((name, ty, buf, default));
        buf
    }

    /// `window(x, N)`: the last `N` samples of `x`, newest last.
    ///
    /// The ring is a state slot and the result *is* that slot — the sample
    /// pushes in and the whole ring reads out, which is the layout the panel's
    /// Window node published and therefore what every saved plot expects. A
    /// tensor sample keeps its shape, so the result is one rank deeper.
    fn window(&mut self, call: &ast::ExprCall, depth: u32) -> Option<(Expr, Ty)> {
        if self.now.is_none() {
            self.diags
                .push(call.range, "`window` exists inside a system");
            return None;
        }
        let Some(len) = literal_count(&call.arguments.args[1]) else {
            self.diags.push(
                call.arguments.args[1].range(),
                "a window's length is a positive integer literal",
            );
            return None;
        };
        let (value, ty) = self.expr(&call.arguments.args[0], depth)?;
        let (value, sample) = match ty {
            Ty::Tensor { .. } => (value, ty.clone()),
            other => (
                self.as_f64(value, other, call.arguments.args[0].range())?,
                Ty::F64,
            ),
        };

        let mut shape = vec![len];
        if let Ty::Tensor { shape: dims, .. } = &sample {
            shape.extend(dims.iter().copied());
        }
        if shape.len() > MAX_RANK {
            self.diags.push(
                call.range,
                format!("a window of {sample} would exceed rank {MAX_RANK}"),
            );
            return None;
        }
        let ty = Ty::Tensor {
            dtype: Dtype::F64,
            shape,
        };
        let index = self.synthetic.len();
        let state = self.keep(
            format!("@window{index}"),
            ty.clone(),
            crate::Init::Fill(0.0),
        );
        Some((
            Expr::Window {
                state,
                value: Box::new(value),
                elems: elems(&sample),
                len: len as u32,
            },
            ty,
        ))
    }

    /// `delta(x)`: the change since the previous sample.
    ///
    /// The previous sample is a state slot of its own per call site, like a
    /// window's ring, seeded with NaN so the first evaluation can tell it has
    /// nothing to differ from and read 0 rather than `x` itself.
    fn delta(&mut self, call: &ast::ExprCall, depth: u32) -> Option<(Expr, Ty)> {
        if self.now.is_none() {
            self.diags
                .push(call.range, "`delta` exists inside a system");
            return None;
        }
        let (value, ty) = self.expr(&call.arguments.args[0], depth)?;
        let value = self.as_f64(value, ty, call.arguments.args[0].range())?;
        let index = self.synthetic.len();
        let state = self.keep(
            format!("@delta{index}"),
            Ty::F64,
            crate::Init::F64(f64::NAN),
        );
        Some((self.since(state, value), Ty::F64))
    }

    /// `deltat(x)`: seconds between the current sample of the input `x` and
    /// the one before it, 0 until there is one before it.
    ///
    /// The input's frame carries its sample's timestamp, and that stamp is
    /// what tells one sample from the next — not the evaluation, since a held
    /// input keeps its newest sample across many of them. Two slots per call
    /// site: the stamps of the newest sample and of the one before, both
    /// seeded NaN so the arrivals count from zero. Between arrivals the gap
    /// holds, which is what makes `delta(x) / deltat(x)` a rate a plot can
    /// read.
    fn deltat(&mut self, call: &ast::ExprCall) -> Option<(Expr, Ty)> {
        if self.now.is_none() {
            self.diags
                .push(call.range, "`deltat` exists inside a system");
            return None;
        }
        let stamp = self.port_stamp(&call.arguments.args[0])?;
        let index = self.synthetic.len();
        let prev = self.keep(
            format!("@deltat{index}.prev"),
            Ty::F64,
            crate::Init::F64(f64::NAN),
        );
        let last = self.keep(
            format!("@deltat{index}.last"),
            Ty::F64,
            crate::Init::F64(f64::NAN),
        );

        let sample = self.temp(Ty::F64);
        let newest = self.temp(Ty::F64);
        let older = self.temp(Ty::F64);
        let sink = self.temp(Ty::F64);
        let f64_local = |slot: u32| Box::new(Expr::Local(slot));
        let differs = |a: u32, b: u32| {
            Box::new(Expr::Cmp {
                op: Cmp::Ne,
                ty: Num::F64,
                lhs: f64_local(a),
                rhs: f64_local(b),
            })
        };
        // `a - b` in seconds, or 0 while `b` is still the NaN seed.
        let gap = |a: u32, b: u32| Expr::Select {
            cond: differs(b, b),
            then: Box::new(Expr::F64(0.0)),
            els: Box::new(Expr::Arith {
                op: Arith::Mul,
                ty: Num::F64,
                lhs: Box::new(Expr::Arith {
                    op: Arith::Sub,
                    ty: Num::F64,
                    lhs: f64_local(a),
                    rhs: f64_local(b),
                }),
                rhs: Box::new(Expr::F64(1e-6)),
            }),
            ty: Ty::F64,
        };
        let expr = Expr::Store {
            local: sample,
            value: Box::new(Expr::Intrinsic {
                op: Intrinsic::IntToFloat,
                args: vec![Expr::Load {
                    buf: stamp,
                    ty: Ty::I64,
                }],
            }),
            then: Box::new(Expr::Store {
                local: newest,
                value: Box::new(Expr::Load {
                    buf: last,
                    ty: Ty::F64,
                }),
                then: Box::new(Expr::Select {
                    // A new sample: what was newest becomes the one before.
                    cond: differs(sample, newest),
                    then: Box::new(Expr::Store {
                        local: sink,
                        value: Box::new(Expr::Exchange {
                            state: prev,
                            value: Box::new(Expr::Exchange {
                                state: last,
                                value: f64_local(sample),
                                ty: Ty::F64,
                            }),
                            ty: Ty::F64,
                        }),
                        then: Box::new(gap(sample, newest)),
                    }),
                    els: Box::new(Expr::Store {
                        local: older,
                        value: Box::new(Expr::Load {
                            buf: prev,
                            ty: Ty::F64,
                        }),
                        then: Box::new(gap(newest, older)),
                    }),
                    ty: Ty::F64,
                }),
            }),
        };
        Some((expr, Ty::F64))
    }

    /// The timestamp field of the input an expression names: a component
    /// port by its full path, a frame parameter, or one of its fields.
    fn port_stamp(&mut self, arg: &ast::Expr) -> Option<BufId> {
        // An input's stamp, or `None` for a name that is not an input.
        let input = |binding: Option<&Binding>| match binding {
            Some(Binding::Cell { stamp, .. }) | Some(Binding::Record { stamp, .. }) => Some(*stamp),
            _ => None,
        };
        let mut found = None;
        if let Some(path) = lang::dotted(arg) {
            found = input(self.names.get(&path));
        }
        if found.is_none()
            && let ast::Expr::Attribute(attr) = arg
            && let Some(head) = lang::dotted(&attr.value)
        {
            found = input(self.names.get(&head));
        }
        match found {
            Some(Some(stamp)) => Some(stamp),
            Some(None) => {
                self.diags.push(
                    arg.range(),
                    "this input's samples carry no timestamp for `deltat` to read",
                );
                None
            }
            None => {
                self.diags.push(
                    arg.range(),
                    "`deltat` needs an input: a component, a frame parameter, or one of its fields",
                );
                None
            }
        }
    }

    /// `value` minus what `state` held, leaving `value` there — and 0 while
    /// the slot still holds its NaN seed, since nothing came before.
    fn since(&mut self, state: BufId, value: Expr) -> Expr {
        let current = self.temp(Ty::F64);
        let previous = self.temp(Ty::F64);
        Expr::Store {
            local: current,
            value: Box::new(value),
            then: Box::new(Expr::Store {
                local: previous,
                value: Box::new(Expr::Exchange {
                    state,
                    value: Box::new(Expr::Local(current)),
                    ty: Ty::F64,
                }),
                then: Box::new(Expr::Select {
                    cond: Box::new(Expr::Cmp {
                        op: Cmp::Ne,
                        ty: Num::F64,
                        lhs: Box::new(Expr::Local(previous)),
                        rhs: Box::new(Expr::Local(previous)),
                    }),
                    then: Box::new(Expr::F64(0.0)),
                    els: Box::new(Expr::Arith {
                        op: Arith::Sub,
                        ty: Num::F64,
                        lhs: Box::new(Expr::Local(current)),
                        rhs: Box::new(Expr::Local(previous)),
                    }),
                    ty: Ty::F64,
                }),
            }),
        }
    }

    /// `fft(x)`: one-sided magnitudes along the last axis.
    ///
    /// Radix-2 wants a power of two and there is no padding rule worth
    /// guessing at, so anything else is a diagnostic naming the length. The
    /// output replaces the last axis with `N / 2 + 1`, which is what the
    /// panel's Fft node published.
    fn fft(&mut self, call: &ast::ExprCall, depth: u32) -> Option<(Expr, Ty)> {
        let (source, ty) = self.expr(&call.arguments.args[0], depth)?;
        let Ty::Tensor { shape, .. } = &ty else {
            self.diags
                .push(call.range, format!("`fft` needs a tensor, found {ty}"));
            return None;
        };
        let len = *shape.last().expect("a tensor has at least one axis");
        if len < 2 || !len.is_power_of_two() {
            self.diags.push(
                call.range,
                format!("`fft` needs a power-of-two last axis, found {len}"),
            );
            return None;
        }
        let groups = shape[..shape.len() - 1].iter().product::<usize>();

        let mut out = shape.clone();
        *out.last_mut().expect("a tensor has at least one axis") = len / 2 + 1;
        let ty = Ty::Tensor {
            dtype: Dtype::F64,
            shape: out,
        };
        let dest = self.buffer(&ty);
        let scratch = self.buffer(&Ty::Tensor {
            dtype: Dtype::F64,
            shape: vec![2 * len],
        });
        Some((
            Expr::Fft {
                dest,
                source: Box::new(source),
                scratch,
                len: len as u32,
                groups: groups as u32,
            },
            ty,
        ))
    }

    /// A periodic signal of the timestamp the system was handed.
    ///
    /// Phase is measured in whole cycles — `freq * t`, with `t` in seconds —
    /// so `sawtooth` and `square` read the fractional part directly and the
    /// two trigonometric shapes multiply by a turn. The kind is four names
    /// rather than one function's argument because the subset has no strings
    /// to spell a kind with, and four palette entries autocomplete better than
    /// one that needs its first argument memorised.
    fn waveform(&mut self, kind: &str, call: &ast::ExprCall, depth: u32) -> Option<(Expr, Ty)> {
        let Some(now) = self.now else {
            self.diags
                .push(call.range, format!("`{kind}` exists inside a system"));
            return None;
        };
        let (freq, freq_ty) = self.expr(&call.arguments.args[0], depth)?;
        let freq = self.as_f64(freq, freq_ty, call.arguments.args[0].range())?;
        let (amp, amp_ty) = self.expr(&call.arguments.args[1], depth)?;
        let amp = self.as_f64(amp, amp_ty, call.arguments.args[1].range())?;

        let seconds = Expr::Arith {
            op: Arith::Mul,
            ty: Num::F64,
            lhs: Box::new(Expr::Intrinsic {
                op: Intrinsic::IntToFloat,
                args: vec![Expr::Local(now)],
            }),
            rhs: Box::new(Expr::F64(1e-6)),
        };
        let cycles = Expr::Arith {
            op: Arith::Mul,
            ty: Num::F64,
            lhs: Box::new(freq),
            rhs: Box::new(seconds),
        };

        let shape = match kind {
            "sine" | "cosine" => Expr::Kernel {
                name: if kind == "sine" { "sin" } else { "cos" },
                args: vec![Expr::Arith {
                    op: Arith::Mul,
                    ty: Num::F64,
                    lhs: Box::new(cycles),
                    rhs: Box::new(Expr::F64(std::f64::consts::TAU)),
                }],
            },
            // The fraction of a cycle is read twice, so it lands in a slot
            // first — `x - floor(x)` would otherwise recompute the phase.
            _ => {
                let slot = self.temp(Ty::F64);
                let fraction = Expr::Arith {
                    op: Arith::Sub,
                    ty: Num::F64,
                    lhs: Box::new(Expr::Local(slot)),
                    rhs: Box::new(Expr::Intrinsic {
                        op: Intrinsic::FloorF64,
                        args: vec![Expr::Local(slot)],
                    }),
                };
                let read = match kind {
                    "sawtooth" => Expr::Arith {
                        op: Arith::Sub,
                        ty: Num::F64,
                        lhs: Box::new(Expr::Arith {
                            op: Arith::Mul,
                            ty: Num::F64,
                            lhs: Box::new(Expr::F64(2.0)),
                            rhs: Box::new(Expr::Local(slot)),
                        }),
                        rhs: Box::new(Expr::F64(1.0)),
                    },
                    _ => Expr::Select {
                        cond: Box::new(Expr::Cmp {
                            op: Cmp::Le,
                            ty: Num::F64,
                            lhs: Box::new(Expr::Local(slot)),
                            rhs: Box::new(Expr::F64(0.5)),
                        }),
                        then: Box::new(Expr::F64(1.0)),
                        els: Box::new(Expr::F64(-1.0)),
                        ty: Ty::F64,
                    },
                };
                Expr::Store {
                    local: slot,
                    value: Box::new(cycles),
                    then: Box::new(Expr::Store {
                        local: slot,
                        value: Box::new(fraction),
                        then: Box::new(read),
                    }),
                }
            }
        };

        Some((
            Expr::Arith {
                op: Arith::Mul,
                ty: Num::F64,
                lhs: Box::new(amp),
                rhs: Box::new(shape),
            },
            Ty::F64,
        ))
    }

    /// `@`, with Python's rank rules: rank-1 against rank-1 is the inner
    /// product, rank-2 is matrix multiplication, and above that the leading
    /// dimensions broadcast while the last two contract.
    ///
    /// A rank-1 operand is promoted for the duration — prepended on the left,
    /// appended on the right — and the axis that promotion invented is dropped
    /// from the result, which is what makes `m @ v` a vector rather than a
    /// one-column matrix.
    fn matmul(&mut self, lhs: (Expr, Ty), rhs: (Expr, Ty), range: TextRange) -> Option<(Expr, Ty)> {
        let (Ty::Tensor { shape: a, .. }, Ty::Tensor { shape: b, .. }) = (&lhs.1, &rhs.1) else {
            self.diags.push(
                range,
                format!("`@` contracts two tensors, found {} and {}", lhs.1, rhs.1),
            );
            return None;
        };
        let (a, b) = (a.clone(), b.clone());

        let refuse = |this: &mut Self, why: &str| {
            this.diags
                .push(range, format!("shapes {a:?} and {b:?} {why}"));
            None::<(Expr, Ty)>
        };

        if let ([n], [m]) = (a.as_slice(), b.as_slice()) {
            if n != m {
                return refuse(self, "do not contract under `@`");
            }
            return Some((
                Expr::Dot {
                    lhs: Box::new(lhs.0),
                    rhs: Box::new(rhs.0),
                    len: *n as u32,
                    emit: emit_for(*n),
                },
                Ty::F64,
            ));
        }

        let (row_vector, column_vector) = (a.len() == 1, b.len() == 1);
        let left = match row_vector {
            true => vec![1, a[0]],
            false => a.clone(),
        };
        let right = match column_vector {
            true => vec![b[0], 1],
            false => b.clone(),
        };
        let (m, k) = (left[left.len() - 2], left[left.len() - 1]);
        let (inner, n) = (right[right.len() - 2], right[right.len() - 1]);
        if k != inner {
            return refuse(self, "do not contract under `@`");
        }

        let (lead_lhs, lead_rhs) = (&left[..left.len() - 2], &right[..right.len() - 2]);
        let Some(lead) = broadcast(lead_lhs, lead_rhs) else {
            return refuse(self, "have leading dimensions that do not broadcast");
        };

        let batches = batches(lead_lhs, lead_rhs, &lead, m * k, inner * n, m * n);

        // The promoted axis leaves with the promotion that invented it.
        let mut shape = lead;
        if !row_vector {
            shape.push(m);
        }
        if !column_vector {
            shape.push(n);
        }
        let ty = Ty::Tensor {
            dtype: Dtype::F64,
            shape,
        };
        let dest = self.buffer(&ty);
        Some((
            Expr::MatMul {
                dest,
                lhs: Box::new(lhs.0),
                rhs: Box::new(rhs.0),
                m: m as u32,
                k: k as u32,
                n: n as u32,
                emit: emit_for(batches.len() * m * k * n),
                batches,
            },
            ty,
        ))
    }
}

/// One matrix product per element of the broadcast leading shape, as element
/// offsets into each operand.
fn batches(
    lhs: &[usize],
    rhs: &[usize],
    out: &[usize],
    lhs_matrix: usize,
    rhs_matrix: usize,
    out_matrix: usize,
) -> Vec<Batch> {
    let rank = out.len();
    let strides = |shape: &[usize]| {
        let mut row = vec![0usize; rank];
        let mut acc = 1;
        for axis in (0..rank).rev() {
            let extent = self::axis(shape, rank, axis);
            row[axis] = if extent == 1 { 0 } else { acc };
            acc *= extent;
        }
        row
    };
    let (lhs_stride, rhs_stride) = (strides(lhs), strides(rhs));

    let mut index = vec![0usize; rank];
    let mut all = Vec::with_capacity(out.iter().product::<usize>().max(1));
    for at in 0..out.iter().product::<usize>().max(1) {
        let (l, r) = (0..rank).fold((0, 0), |(l, r), axis| {
            (
                l + index[axis] * lhs_stride[axis],
                r + index[axis] * rhs_stride[axis],
            )
        });
        all.push(Batch {
            lhs: (l * lhs_matrix) as u32,
            rhs: (r * rhs_matrix) as u32,
            out: (at * out_matrix) as u32,
        });
        for axis in (0..rank).rev() {
            index[axis] += 1;
            if index[axis] < out[axis] {
                break;
            }
            index[axis] = 0;
        }
    }
    all
}

/// A positive integer literal, for the lengths that have to be static.
fn literal_count(expr: &ast::Expr) -> Option<usize> {
    let ast::Expr::NumberLiteral(c) = expr else {
        return None;
    };
    let ast::Number::Int(n) = &c.value else {
        return None;
    };
    n.as_usize().filter(|n| *n > 0)
}

fn literal_exponent(expr: &ast::Expr) -> Option<u32> {
    let ast::Expr::NumberLiteral(c) = expr else {
        return None;
    };
    let ast::Number::Int(n) = &c.value else {
        return None;
    };
    n.as_u64().filter(|n| *n <= 16).map(|x| x as u32)
}

fn kernel_log(arg: Expr) -> Expr {
    Expr::Kernel {
        name: "log",
        args: vec![arg],
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
        Stmt::Return(_) | Stmt::ReturnBuffer { .. } | Stmt::ReturnFrame(_) => true,
        Stmt::If { then, els, .. } => {
            !els.is_empty() && always_returns(then) && always_returns(els)
        }
        _ => false,
    })
}

fn refusal(stmt: &ast::Stmt) -> &'static str {
    match stmt {
        ast::Stmt::FunctionDef(_) => "nested functions and closures are not supported",
        ast::Stmt::ClassDef(_) => "classes are not supported in this phase",
        ast::Stmt::Delete(_) => "`del` is not supported",
        ast::Stmt::For(_) => "`async for` is not supported",
        ast::Stmt::With(_) => "`with` is not supported",
        ast::Stmt::Match(_) => "`match` is not supported",
        ast::Stmt::Raise(_) => "`raise` is not supported; a fault is a trap here",
        ast::Stmt::Try(_) => "`try` is not supported",
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
        ast::Expr::ListComp(_) => "list comprehensions are not supported",
        ast::Expr::Generator(_) => "generators are not supported",
        ast::Expr::Await(_) | ast::Expr::Yield(_) | ast::Expr::YieldFrom(_) => {
            "`await` and `yield` are not supported"
        }
        ast::Expr::FString(_) => "strings are not supported",
        ast::Expr::Starred(_) => "argument unpacking is not supported",
        ast::Expr::Named(_) => "`:=` is not supported",
        ast::Expr::Attribute(_) => "attribute access is only supported for frame fields",
        ast::Expr::Slice(_) => "slicing is not supported in this phase",
        ast::Expr::Tuple(_) => "tuples are not supported",
        _ => "this expression is not supported",
    }
}
