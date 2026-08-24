//! Declarations: what a module says before any of it is checked.
//!
//! Three tiers of source land here and exactly one shape leaves. A `@system`
//! over declared frames, a `@system` over positional component paths, a
//! top-level binding, and a bare expression in a panel field are all the same
//! construct — an anonymous function over ports, publishing a frame — and the
//! differences between them are all resolved in this module. Everything
//! downstream sees [`SystemDecl`] and cannot tell which tier wrote it. That is
//! what makes the design's promotion gradient a copy-paste rather than a
//! rewrite.
//!
//! What each tier contributes:
//!
//! - **`@system` over frames.** Parameters typed by a `Frame` class are input
//!   ports; a parameter typed by a `State` class is the state record; the
//!   return annotation is the output frame. A port binds to the frame named by
//!   the snake case of its class, or to whatever `bind=` says instead, and the
//!   body addresses it as a record: `imu.omega`.
//! - **`@system` over paths.** `@system("adcs.omega_b")` binds parameters
//!   straight to components, so their types come from the host's schemas
//!   rather than from annotations. Each such port is a one-field frame the
//!   body sees *as* its field — the projection the design doc describes.
//! - **Top-level bindings and bare expressions.** The free component
//!   references in the expression become the ports, the expression becomes the
//!   return, and the first reference drives. A binding is named by its target;
//!   a bare expression is named `expr`.
//!
//! Component references are the syntactic accident the one-liner tier rests
//! on: `adcs.omega_b` is already valid Python, and a bare `omega_b` resolves
//! by unique suffix. Both are looked up here, once, and what the manifest
//! keeps is the resolved path.

use std::collections::{HashMap, HashSet};

use rustpython_parser::ast::{self, Ranged};

use crate::diag::{Diagnostics, Span};
use crate::manifest::{Binding, Field, Frame, Init, Resample, StateField};
use crate::{CompSchema, Resolver, Ty};

/// The name a bare expression's system takes.
pub(crate) const ANONYMOUS: &str = "expr";

/// One system, after every tier has been reduced to the same thing.
pub(crate) struct SystemDecl<'a> {
    pub name: String,
    pub ports: Vec<PortDecl>,
    pub state: Vec<StateSlot>,
    /// `None` until the body decides, which is how a bare expression works.
    pub output: Option<Frame>,
    /// The class a `return Frame(...)` must name. Absent when the output is a
    /// one-field frame the body fills with a bare `return`.
    pub output_class: Option<String>,
    pub anonymous_output: bool,
    pub driving: Option<usize>,
    /// Hertz, for a system that clocks itself instead of waiting on an input.
    pub rate: Option<f64>,
    pub body: Body<'a>,
    pub span: Span,
}

/// How a port's fields reach the body.
pub(crate) struct PortDecl {
    /// The name the body addresses this port by — a parameter name for a
    /// frame port, the component path or bare name the operator typed for a
    /// projected one.
    pub key: String,
    /// Absent when the port reads an anonymous binding whose output type the
    /// body decides — the checker fills it in from the producer, which it has
    /// already checked.
    pub frame: Option<Frame>,
    pub bindings: Vec<Binding>,
    /// A one-field port the body reads *as* the field, not as a record.
    pub projected: bool,
}

pub(crate) struct StateSlot {
    pub field: StateField,
    /// The parameter the body addresses the state record by.
    pub param: String,
}

pub(crate) enum Body<'a> {
    Stmts(&'a [ast::Stmt]),
    Expr(&'a ast::Expr),
}

/// A clock-changing stage, as written: `slow = resample_zoh(fast, 10.0)`.
///
/// The output type is absent when the source is a declaration whose type the
/// body decides — the checker fills it from the producer it has already
/// finished, exactly as it does for a port.
pub(crate) struct StageDecl {
    pub name: String,
    pub kind: Resample,
    pub source: Binding,
    pub ty: Option<Ty>,
    pub rate: f64,
    pub span: Span,
}

/// One top-level declaration, in the order it was written.
#[derive(Clone, Copy)]
pub(crate) enum Item {
    System(usize),
    Stage(usize),
}

/// Everything a module declares, before bodies are looked at.
pub(crate) struct Decls<'a> {
    pub systems: Vec<SystemDecl<'a>>,
    pub stages: Vec<StageDecl>,
    /// Declaration order across both lists, which is the order a binding may
    /// name what came before it in.
    pub order: Vec<Item>,
    pub functions: Vec<&'a ast::StmtFunctionDef>,
}

/// A `Frame` or `State` class, as written.
struct ClassDecl {
    fields: Vec<(String, Ty, Option<Init>)>,
    state: bool,
}

pub(crate) fn collect<'a>(
    module: &'a [ast::Stmt],
    resolver: &dyn Resolver,
    diags: &mut Diagnostics,
) -> Decls<'a> {
    let mut classes: HashMap<String, ClassDecl> = HashMap::new();
    let mut functions = Vec::new();
    let mut systems = Vec::new();
    let mut bindings = Vec::new();

    for stmt in module {
        match stmt {
            ast::Stmt::ClassDef(class) => {
                if let Some(decl) = class_decl(class, diags) {
                    classes.insert(class.name.as_str().to_string(), decl);
                }
            }
            ast::Stmt::FunctionDef(def) if def.decorator_list.is_empty() => functions.push(def),
            ast::Stmt::FunctionDef(def) => systems.push(def),
            ast::Stmt::Assign(assign) => bindings.push(assign),
            other => diags.push(
                other.range(),
                "a module holds classes, functions, and bindings",
            ),
        }
    }

    // Output frames have to exist before any port can bind to one, so the
    // signatures are read in full before any binding is resolved.
    let mut signatures = Vec::new();
    for def in &systems {
        signatures.push(signature(def, &classes, diags));
    }
    let produced: HashMap<String, usize> = signatures
        .iter()
        .enumerate()
        .filter_map(|(i, sig)| sig.as_ref().map(|s| (s.output.name.clone(), i)))
        .collect();

    let mut decls = Vec::new();
    let mut order = Vec::new();
    for (def, sig) in systems.iter().zip(signatures) {
        let Some(sig) = sig else { continue };
        if let Some(decl) = system_decl(def, sig, &classes, &produced, resolver, diags) {
            order.push(Item::System(decls.len()));
            decls.push(decl);
        }
    }
    let mut stages: Vec<StageDecl> = Vec::new();
    for assign in bindings {
        // A binding whose right-hand side is exactly a resample call is a
        // stage, not a system: it changes the clock, which is the host's job.
        match stage_decl(assign, &decls, &stages, resolver, diags) {
            Some(Some(stage)) => {
                order.push(Item::Stage(stages.len()));
                stages.push(stage);
            }
            Some(None) => {}
            None => {
                if let Some(decl) = binding_decl(assign, &decls, &stages, resolver, diags) {
                    order.push(Item::System(decls.len()));
                    decls.push(decl);
                }
            }
        }
    }

    let mut seen = HashSet::new();
    for (name, span) in decls
        .iter()
        .map(|d| (&d.name, d.span))
        .chain(stages.iter().map(|s| (&s.name, s.span)))
    {
        if !seen.insert(name.clone()) {
            diags.push(span, format!("`{name}` is declared more than once"));
        }
    }

    Decls {
        systems: decls,
        stages,
        order,
        functions,
    }
}

/// `name = resample_zoh(source, rate)`, or `None` when the assignment is not
/// that shape at all. `Some(None)` means it was, and was malformed.
fn stage_decl(
    assign: &ast::StmtAssign,
    systems: &[SystemDecl<'_>],
    stages: &[StageDecl],
    resolver: &dyn Resolver,
    diags: &mut Diagnostics,
) -> Option<Option<StageDecl>> {
    let ast::Expr::Call(call) = assign.value.as_ref() else {
        return None;
    };
    let ast::Expr::Name(callee) = call.func.as_ref() else {
        return None;
    };
    let kind = match callee.id.as_str() {
        "resample_zoh" => Resample::Zoh,
        "resample_linear" => Resample::Linear,
        _ => return None,
    };

    let [ast::Expr::Name(target)] = assign.targets.as_slice() else {
        diags.push(
            assign.range,
            "a resample is a top-level binding to one plain name",
        );
        return Some(None);
    };
    if call.args.len() != 2 || !call.keywords.is_empty() {
        diags.push(
            call.range,
            format!("`{}` takes a source and a rate in hertz", callee.id),
        );
        return Some(None);
    }
    let Some(rate) = number_of(&call.args[1]).filter(|hz| hz.is_finite() && *hz > 0.0) else {
        diags.push(
            call.args[1].range(),
            "a resample rate is a positive number of hertz",
        );
        return Some(None);
    };
    let Some(key) = dotted(&call.args[0]) else {
        diags.push(call.args[0].range(), "a resample reads one channel");
        return Some(None);
    };

    let span: Span = call.args[0].range().into();
    let (source, ty) = if let Some(stage) = stages.iter().position(|s| s.name == key) {
        (Binding::Resampled { stage }, stages[stage].ty.clone())
    } else if let Some(system) = systems.iter().position(|s| s.name == key) {
        (
            Binding::Produced { system, field: 0 },
            systems[system]
                .output
                .as_ref()
                .map(|frame| frame.fields[0].ty.clone()),
        )
    } else {
        let Some(path) = resolve_path(&key, resolver, span, diags) else {
            return Some(None);
        };
        let Some(CompSchema { ty }) = resolver.component(&path) else {
            diags.push(span, format!("no component `{path}`"));
            return Some(None);
        };
        (Binding::Component(path), Some(ty))
    };

    Some(Some(StageDecl {
        name: target.id.as_str().to_string(),
        kind,
        source,
        ty,
        rate,
        span: assign.range.into(),
    }))
}

/// A dotted path as written, or the one a bare name's unique suffix finds.
fn resolve_path(
    key: &str,
    resolver: &dyn Resolver,
    span: Span,
    diags: &mut Diagnostics,
) -> Option<String> {
    if key.contains('.') {
        return Some(key.to_string());
    }
    match resolver.suffix(key).as_slice() {
        [only] => Some(only.clone()),
        [] => {
            diags.push(span, format!("`{key}` is not defined and names no component"));
            None
        }
        many => {
            diags.push(span, format!("`{key}` is ambiguous: {}", many.join(", ")));
            None
        }
    }
}

/// Desugar a bare expression into the single system a `=` field runs.
pub(crate) fn expression<'a>(
    expr: &'a ast::Expr,
    resolver: &dyn Resolver,
    diags: &mut Diagnostics,
) -> Option<SystemDecl<'a>> {
    anonymous(ANONYMOUS.to_string(), expr, &[], &[], resolver, diags)
}

fn class_decl(class: &ast::StmtClassDef, diags: &mut Diagnostics) -> Option<ClassDecl> {
    let kind = match class.bases.as_slice() {
        [ast::Expr::Name(base)] => base.id.as_str(),
        _ => {
            diags.push(
                class.range,
                "a class derives from exactly one of Frame or State",
            );
            return None;
        }
    };
    let state = match kind {
        "Frame" => false,
        "State" => true,
        other => {
            diags.push(
                class.range,
                format!("`{other}` is not a base class here; use Frame or State"),
            );
            return None;
        }
    };

    let mut fields = Vec::new();
    for stmt in &class.body {
        let ast::Stmt::AnnAssign(ann) = stmt else {
            if !matches!(stmt, ast::Stmt::Pass(_)) {
                diags.push(
                    stmt.range(),
                    "a Frame or State class holds annotated fields",
                );
            }
            continue;
        };
        let ast::Expr::Name(name) = ann.target.as_ref() else {
            diags.push(ann.target.range(), "a field is a plain name");
            continue;
        };
        let ty = crate::check::annotation(&ann.annotation, diags);
        let init = match (&ann.value, state) {
            (Some(value), true) => default_of(value, &ty, diags),
            (Some(value), false) => {
                diags.push(value.range(), "a Frame field carries no default");
                None
            }
            (None, true) => {
                diags.push(
                    ann.range,
                    "a State field needs a default; it is what the first evaluation sees",
                );
                None
            }
            (None, false) => None,
        };
        if state && init.is_none() {
            continue;
        }
        fields.push((name.id.as_str().to_string(), ty, init));
    }
    Some(ClassDecl { fields, state })
}

fn default_of(value: &ast::Expr, ty: &Ty, diags: &mut Diagnostics) -> Option<Init> {
    let bad = |diags: &mut Diagnostics| {
        diags.push(
            value.range(),
            format!("a {ty} default is a literal of that type"),
        );
        None
    };
    let (negate, inner) = match value {
        ast::Expr::UnaryOp(u) if matches!(u.op, ast::UnaryOp::USub) => (true, u.operand.as_ref()),
        other => (false, other),
    };
    let ast::Expr::Constant(c) = inner else {
        return bad(diags);
    };
    let sign = if negate { -1.0 } else { 1.0 };
    match (&c.value, ty) {
        (ast::Constant::Float(f), Ty::F64) => Some(Init::F64(sign * f)),
        (ast::Constant::Int(i), Ty::F64) => {
            Some(Init::F64(sign * num_traits::ToPrimitive::to_f64(i)?))
        }
        (ast::Constant::Int(i), Ty::I64) => {
            let v: i64 = num_traits::ToPrimitive::to_i64(i)?;
            Some(Init::I64(if negate { -v } else { v }))
        }
        (ast::Constant::Bool(b), Ty::Bool) if !negate => Some(Init::Bool(*b)),
        (ast::Constant::Float(f), Ty::Tensor { .. }) => Some(Init::Fill(sign * f)),
        (ast::Constant::Int(i), Ty::Tensor { .. }) => {
            Some(Init::Fill(sign * num_traits::ToPrimitive::to_f64(i)?))
        }
        _ => bad(diags),
    }
}

/// The port and output shape of a `@system`, before bindings are resolved.
struct Signature {
    /// Parameter name, class name, and whether the class is `State`.
    params: Vec<(String, Option<String>)>,
    state: Vec<StateSlot>,
    output: Frame,
    output_class: Option<String>,
    anonymous_output: bool,
}

fn signature(
    def: &ast::StmtFunctionDef,
    classes: &HashMap<String, ClassDecl>,
    diags: &mut Diagnostics,
) -> Option<Signature> {
    let args = &def.args;
    if !args.posonlyargs.is_empty()
        || !args.kwonlyargs.is_empty()
        || args.vararg.is_some()
        || args.kwarg.is_some()
    {
        diags.push(def.range, "only plain positional parameters are supported");
        return None;
    }

    let mut params = Vec::new();
    let mut state = Vec::new();
    for arg in &args.args {
        let name = arg.def.arg.as_str().to_string();
        let class = match &arg.def.annotation {
            Some(ann) => match ann.as_ref() {
                ast::Expr::Name(n) => Some(n.id.as_str().to_string()),
                other => {
                    diags.push(
                        other.range(),
                        "a system parameter names a Frame or State class",
                    );
                    return None;
                }
            },
            None => None,
        };
        match class.as_deref().and_then(|c| classes.get(c)) {
            Some(decl) if decl.state => {
                for (field, ty, init) in &decl.fields {
                    state.push(StateSlot {
                        field: StateField {
                            name: field.clone(),
                            ty: ty.clone(),
                            default: init
                                .clone()
                                .expect("a State field without a default is dropped"),
                        },
                        param: name.clone(),
                    });
                }
            }
            _ => params.push((name, class)),
        }
    }

    // A frame's name is what edges match on, so an output frame and the port
    // that reads it arrive at the same string from the same class.
    let (output, output_class, anonymous_output) = match &def.returns {
        Some(ann) => match ann.as_ref() {
            ast::Expr::Name(n) if classes.contains_key(n.id.as_str()) => {
                let class = n.id.as_str();
                let decl = &classes[class];
                if decl.state {
                    diags.push(ann.range(), "a system returns a Frame, not a State");
                    return None;
                }
                (
                    frame_of(
                        &snake_case(class),
                        decl.fields.iter().map(|(n, t, _)| (n.clone(), t.clone())),
                    ),
                    Some(class.to_string()),
                    false,
                )
            }
            other => {
                let ty = crate::check::annotation(other, diags);
                (
                    frame_of(def.name.as_str(), [(def.name.as_str().to_string(), ty)]),
                    None,
                    true,
                )
            }
        },
        None => {
            diags.push(def.range, "a system needs a return type");
            return None;
        }
    };

    Some(Signature {
        params,
        state,
        output,
        output_class,
        anonymous_output,
    })
}

/// Lay a field list out contiguously, which is what `arg_ptr` addresses.
pub(crate) fn frame_of(name: &str, fields: impl IntoIterator<Item = (String, Ty)>) -> Frame {
    let mut offset = 0;
    let fields: Vec<Field> = fields
        .into_iter()
        .map(|(name, ty)| {
            let at = offset;
            offset += bytes_of(&ty);
            Field {
                name,
                ty,
                offset: at,
            }
        })
        .collect();
    Frame {
        name: name.to_string(),
        fields,
        bytes: offset,
    }
}

pub(crate) fn bytes_of(ty: &Ty) -> u32 {
    match ty {
        Ty::Tensor { shape, .. } => shape.iter().product::<usize>() as u32 * 8,
        _ => 8,
    }
}

fn system_decl<'a>(
    def: &'a ast::StmtFunctionDef,
    sig: Signature,
    classes: &HashMap<String, ClassDecl>,
    produced: &HashMap<String, usize>,
    resolver: &dyn Resolver,
    diags: &mut Diagnostics,
) -> Option<SystemDecl<'a>> {
    let decorator = decorator(def, diags)?;

    let mut ports = Vec::new();
    if decorator.paths.is_empty() {
        for (param, class) in &sig.params {
            let Some(class) = class else {
                diags.push(
                    def.range,
                    format!(
                        "`{param}` needs a Frame annotation, or a component path in @system(...)"
                    ),
                );
                return None;
            };
            let Some(decl) = classes.get(class) else {
                diags.push(def.range, format!("`{class}` is not a Frame class here"));
                return None;
            };
            let frame_name = decorator
                .bind
                .get(param)
                .cloned()
                .unwrap_or_else(|| snake_case(class));
            let frame = frame_of(
                &frame_name,
                decl.fields.iter().map(|(n, t, _)| (n.clone(), t.clone())),
            );
            let bindings = bind_frame(&frame, produced, resolver, def.range.into(), diags)?;
            ports.push(PortDecl {
                key: param.clone(),
                frame: Some(frame),
                bindings,
                projected: false,
            });
        }
    } else {
        if decorator.paths.len() != sig.params.len() {
            diags.push(
                def.range,
                format!(
                    "@system binds {} paths but `{}` takes {} parameters",
                    decorator.paths.len(),
                    def.name.as_str(),
                    sig.params.len()
                ),
            );
            return None;
        }
        for ((param, _), path) in sig.params.iter().zip(&decorator.paths) {
            ports.push(projected_port(
                param.clone(),
                path,
                resolver,
                def.range.into(),
                diags,
            )?);
        }
    }

    let driving = driving_port(&decorator, &ports, def.range.into(), diags)?;

    Some(SystemDecl {
        name: def.name.as_str().to_string(),
        ports,
        state: sig.state,
        output: Some(sig.output),
        output_class: sig.output_class,
        anonymous_output: sig.anonymous_output,
        driving,
        rate: decorator.rate,
        body: Body::Stmts(&def.body),
        span: def.range.into(),
    })
}

/// A port over a single component, seen by the body as the value itself.
fn projected_port(
    key: String,
    path: &str,
    resolver: &dyn Resolver,
    span: Span,
    diags: &mut Diagnostics,
) -> Option<PortDecl> {
    let Some(CompSchema { ty }) = resolver.component(path) else {
        diags.push(span, format!("no component `{path}`"));
        return None;
    };
    let field = path.rsplit('.').next().unwrap_or(path).to_string();
    Some(PortDecl {
        key,
        frame: Some(frame_of(path, [(field, ty)])),
        bindings: vec![Binding::Component(path.to_string())],
        projected: true,
    })
}

/// Every field of a declared frame, against a system that produces it or
/// against the host's component tree.
fn bind_frame(
    frame: &Frame,
    produced: &HashMap<String, usize>,
    resolver: &dyn Resolver,
    span: Span,
    diags: &mut Diagnostics,
) -> Option<Vec<Binding>> {
    if let Some(system) = produced.get(&frame.name) {
        return Some(
            (0..frame.fields.len())
                .map(|field| Binding::Produced {
                    system: *system,
                    field,
                })
                .collect(),
        );
    }
    if let Some(schema) = resolver.frame(&frame.name) {
        let want: Vec<(String, Ty)> = frame
            .fields
            .iter()
            .map(|f| (f.name.clone(), f.ty.clone()))
            .collect();
        if schema.fields != want {
            diags.push(
                span,
                format!("frame `{}` does not match the host's fields", frame.name),
            );
            return None;
        }
    }
    let mut bindings = Vec::with_capacity(frame.fields.len());
    for field in &frame.fields {
        let path = format!("{}.{}", frame.name, field.name);
        match resolver.component(&path) {
            Some(schema) if schema.ty == field.ty => bindings.push(Binding::Component(path)),
            Some(schema) => {
                diags.push(
                    span,
                    format!(
                        "`{path}` is {}, but the frame declares {}",
                        schema.ty, field.ty
                    ),
                );
                return None;
            }
            None => {
                diags.push(span, format!("no component `{path}`"));
                return None;
            }
        }
    }
    Some(bindings)
}

/// What `@system(...)` said.
#[derive(Default)]
struct Decorator {
    paths: Vec<String>,
    bind: HashMap<String, String>,
    on: Option<String>,
    rate: Option<f64>,
}

fn decorator(def: &ast::StmtFunctionDef, diags: &mut Diagnostics) -> Option<Decorator> {
    if def.decorator_list.len() != 1 {
        diags.push(
            def.range,
            "a function carries at most one @system decorator",
        );
        return None;
    }
    let mut out = Decorator::default();
    match &def.decorator_list[0] {
        ast::Expr::Name(n) if n.id.as_str() == "system" => Some(out),
        ast::Expr::Call(call) => {
            let named =
                matches!(call.func.as_ref(), ast::Expr::Name(n) if n.id.as_str() == "system");
            if !named {
                diags.push(call.range, "the only decorator is @system");
                return None;
            }
            for arg in &call.args {
                match string_of(arg) {
                    Some(path) => out.paths.push(path),
                    None => {
                        diags.push(
                            arg.range(),
                            "a positional @system argument is a component path",
                        );
                        return None;
                    }
                }
            }
            for keyword in &call.keywords {
                match keyword.arg.as_ref().map(|a| a.as_str()) {
                    Some("bind") => {
                        let ast::Expr::Dict(dict) = &keyword.value else {
                            diags.push(keyword.range, "`bind` takes a dict of parameter to frame");
                            return None;
                        };
                        for (key, value) in dict.keys.iter().zip(&dict.values) {
                            let (Some(key), Some(value)) =
                                (key.as_ref().and_then(string_of), string_of(value))
                            else {
                                diags.push(keyword.range, "`bind` maps strings to strings");
                                return None;
                            };
                            out.bind.insert(key, value);
                        }
                    }
                    Some("on") => match string_of(&keyword.value) {
                        Some(on) => out.on = Some(on),
                        None => {
                            diags.push(keyword.range, "`on` names a parameter");
                            return None;
                        }
                    },
                    Some("rate") => match number_of(&keyword.value) {
                        Some(hz) if hz.is_finite() && hz > 0.0 => out.rate = Some(hz),
                        _ => {
                            diags.push(keyword.range, "`rate` is a positive number of hertz");
                            return None;
                        }
                    },
                    _ => {
                        diags.push(keyword.range, "@system takes `bind`, `on`, and `rate`");
                        return None;
                    }
                }
            }
            Some(out)
        }
        other => {
            diags.push(other.range(), "the only decorator is @system");
            None
        }
    }
}

/// Which port fires the system, or none when its own clock does.
///
/// The two are exclusive by construction: a system is either source-clocked or
/// input-driven, and saying both is a question with no answer rather than a
/// preference to resolve.
fn driving_port(
    decorator: &Decorator,
    ports: &[PortDecl],
    span: Span,
    diags: &mut Diagnostics,
) -> Option<Option<usize>> {
    if decorator.rate.is_some() {
        if decorator.on.is_some() {
            diags.push(
                span,
                "a system is either source-clocked (`rate`) or input-driven (`on`)",
            );
            return None;
        }
        return Some(None);
    }
    match &decorator.on {
        None => Some((!ports.is_empty()).then_some(0)),
        Some(on) => match ports.iter().position(|p| p.key == *on) {
            Some(index) => Some(Some(index)),
            None => {
                diags.push(span, format!("`on` names `{on}`, which is not a parameter"));
                None
            }
        },
    }
}

fn string_of(expr: &ast::Expr) -> Option<String> {
    match expr {
        ast::Expr::Constant(c) => match &c.value {
            ast::Constant::Str(s) => Some(s.clone()),
            _ => None,
        },
        _ => None,
    }
}

fn number_of(expr: &ast::Expr) -> Option<f64> {
    match expr {
        ast::Expr::Constant(c) => match &c.value {
            ast::Constant::Float(f) => Some(*f),
            ast::Constant::Int(i) => num_traits::ToPrimitive::to_f64(i),
            _ => None,
        },
        _ => None,
    }
}

fn binding_decl<'a>(
    assign: &'a ast::StmtAssign,
    existing: &[SystemDecl<'_>],
    stages: &[StageDecl],
    resolver: &dyn Resolver,
    diags: &mut Diagnostics,
) -> Option<SystemDecl<'a>> {
    let [ast::Expr::Name(target)] = assign.targets.as_slice() else {
        diags.push(assign.range, "a top-level binding assigns one plain name");
        return None;
    };
    anonymous(
        target.id.as_str().to_string(),
        &assign.value,
        existing,
        stages,
        resolver,
        diags,
    )
}

/// The shared shape behind a top-level binding and a bare `=` expression: free
/// component references become the ports, the expression becomes the return.
fn anonymous<'a>(
    name: String,
    expr: &'a ast::Expr,
    existing: &[SystemDecl<'_>],
    stages: &[StageDecl],
    resolver: &dyn Resolver,
    diags: &mut Diagnostics,
) -> Option<SystemDecl<'a>> {
    let mut references = Vec::new();
    gather(expr, &mut references);

    let mut ports: Vec<PortDecl> = Vec::new();
    for (key, span) in references {
        if ports.iter().any(|p| p.key == key) {
            continue;
        }
        // An earlier binding in the same module is a frame edge, not a
        // component lookup, and so is an earlier stage.
        if let Some(system) = existing.iter().position(|s| s.name == key) {
            ports.push(PortDecl {
                key,
                frame: existing[system].output.clone(),
                bindings: vec![Binding::Produced { system, field: 0 }],
                projected: true,
            });
            continue;
        }
        if let Some(stage) = stages.iter().position(|s| s.name == key) {
            ports.push(PortDecl {
                key: key.clone(),
                frame: stages[stage]
                    .ty
                    .clone()
                    .map(|ty| frame_of(&key, [(key.clone(), ty)])),
                bindings: vec![Binding::Resampled { stage }],
                projected: true,
            });
            continue;
        }
        let path = resolve_path(&key, resolver, span, diags)?;
        ports.push(projected_port(key, &path, resolver, span, diags)?);
    }

    let driving = (!ports.is_empty()).then_some(0);
    Some(SystemDecl {
        name,
        ports,
        state: Vec::new(),
        output: None,
        output_class: None,
        anonymous_output: true,
        driving,
        rate: None,
        body: Body::Expr(expr),
        span: expr.range().into(),
    })
}

/// Every free name and dotted path an expression mentions, in source order.
///
/// A call's *callee* is not a reference — `sqrt(x)` names a builtin, not a
/// component — but its arguments are.
fn gather(expr: &ast::Expr, out: &mut Vec<(String, Span)>) {
    if let Some(path) = dotted(expr) {
        out.push((path, expr.range().into()));
        return;
    }
    match expr {
        ast::Expr::Call(call) => {
            for arg in &call.args {
                gather(arg, out);
            }
            for keyword in &call.keywords {
                gather(&keyword.value, out);
            }
        }
        ast::Expr::BinOp(b) => {
            gather(&b.left, out);
            gather(&b.right, out);
        }
        ast::Expr::UnaryOp(u) => gather(&u.operand, out),
        ast::Expr::BoolOp(b) => {
            for value in &b.values {
                gather(value, out);
            }
        }
        ast::Expr::Compare(c) => {
            gather(&c.left, out);
            for cmp in &c.comparators {
                gather(cmp, out);
            }
        }
        ast::Expr::IfExp(c) => {
            gather(&c.test, out);
            gather(&c.body, out);
            gather(&c.orelse, out);
        }
        ast::Expr::Subscript(sub) => {
            gather(&sub.value, out);
            gather(&sub.slice, out);
        }
        ast::Expr::Tuple(t) => {
            for element in &t.elts {
                gather(element, out);
            }
        }
        _ => {}
    }
}

/// The dotted text of a name or attribute chain, when the whole thing is one.
pub(crate) fn dotted(expr: &ast::Expr) -> Option<String> {
    match expr {
        ast::Expr::Name(n) => Some(n.id.as_str().to_string()),
        ast::Expr::Attribute(a) => Some(format!("{}.{}", dotted(&a.value)?, a.attr.as_str())),
        _ => None,
    }
}

/// `RateEstimate` to `rate_estimate` — the default frame a port binds to.
fn snake_case(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    for (i, c) in name.chars().enumerate() {
        if c.is_uppercase() && i > 0 {
            out.push('_');
        }
        out.extend(c.to_lowercase());
    }
    out
}
