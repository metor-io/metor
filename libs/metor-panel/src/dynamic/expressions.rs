//! View-owned expressions: the `=` tier, running.
//!
//! A component picker with a leading `=` is the spreadsheet convention, and
//! what it produces is a system like any other — the difference is only who
//! owns it. A program pane's systems are owned by the pane and publish real
//! components; an `=` expression is owned by *the view that typed it* and
//! publishes nothing. It exists while a view wants it and stops when the last
//! one stops.
//!
//! That ownership rule is the whole of this module. The registry holds
//! [`Weak`] handles, so it never keeps an expression alive: a view holds the
//! strong `Arc`, two views typing the same text find the same node through its
//! content hash and share it, and the entry falls out on its own once neither
//! is left. No reference counts are kept by hand, and nothing has to be told
//! when a view goes away.
//!
//! What a view serializes is the text the operator typed, prefixed with `=`.
//! The node is derived from it on load — which is also why the resolution
//! recorded in the manifest matters: the text is what the operator wrote, and
//! the compiled binding is the full path it meant at the time.

use std::collections::HashMap;
use std::sync::{Arc, Weak};

use gpui::{App, Global};
use metor_db::DB;
use metor_expr::Diagnostics;
use metor_proto::types::ComponentId;

use crate::dynamic::{BuildError, DynamicNode, NodeId};
use crate::dynamic::ops::program::{self, Compiled, DEFAULT_FUEL};
use crate::dynamic::resolver::DbResolver;
use crate::node_editor::worker::DynamicWorker;

/// The prefix that turns a picker's search field into an expression field.
pub const SIGIL: char = '=';

/// Whether a saved binding is an expression rather than a component name.
pub fn is_expression(text: &str) -> bool {
    text.starts_with(SIGIL)
}

/// The expression inside a `=`-prefixed binding.
pub fn body(text: &str) -> &str {
    text.strip_prefix(SIGIL).unwrap_or(text).trim()
}

/// Live `=` expressions, keyed by what they compute.
///
/// Weak on purpose: see the module docs. Views own their expressions and this
/// only lets them find each other.
#[derive(Default)]
pub struct Expressions {
    live: HashMap<ComponentId, Weak<dyn DynamicNode>>,
}

impl Global for Expressions {}

impl Expressions {
    pub fn init(cx: &mut App) {
        cx.set_global(Expressions::default());
    }

    /// The node behind an expression's component id, if a view still holds it.
    pub fn get(&self, id: ComponentId) -> Option<Arc<dyn DynamicNode>> {
        self.live.get(&id)?.upgrade()
    }
}

/// A running `=` expression, owned by the view that asked for it.
///
/// Dropping this is what ends the expression — assuming no other view is
/// showing the same one.
#[derive(Clone)]
pub struct Expression {
    /// The value node, which is what a view binds to.
    pub node: Arc<dyn DynamicNode>,
    /// Held for its drop: the system feeding `node`.
    _system: Arc<dyn DynamicNode>,
}

impl Expression {
    pub fn component_id(&self) -> ComponentId {
        self.node.id().into()
    }
}

/// Why an expression is not running.
#[derive(Debug)]
pub enum ExprError {
    /// It does not compile. The spans point into the text the operator typed.
    Compile(Diagnostics),
    /// It compiles, but something it names is not publishing yet.
    Unbound(String),
}

impl std::fmt::Display for ExprError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExprError::Compile(diags) => match diags.iter().next() {
                Some(first) => write!(f, "{}", first.message),
                None => write!(f, "this expression could not be compiled"),
            },
            ExprError::Unbound(what) => write!(f, "{what}"),
        }
    }
}

/// Compile an expression and start it, or hand back the one already running
/// it.
///
/// `text` is what the operator typed, with or without its `=`.
pub fn resolve(text: &str, db: &Arc<DB>, cx: &mut App) -> Result<Expression, ExprError> {
    let resolver = DbResolver::snapshot(db);
    let compiled =
        Arc::new(Compiled::expression(body(text), &resolver).map_err(ExprError::Compile)?);

    // Which components the ports read is decided here; the nodes that read
    // them are built on the worker thread. A source node's identity follows
    // from its component id alone, so the hash below needs nothing built.
    let mut sources = Vec::new();
    for port in &compiled.manifest.systems[0].inputs {
        let metor_expr::Binding::Component(path) = &port.bindings[0] else {
            return Err(ExprError::Unbound(
                "an expression reads components, not systems".into(),
            ));
        };
        let id = ComponentId(crate::dynamic::ops::persist::component_id_for_name(path));
        if db.with_state(|s| s.get_component(id).is_none()) {
            return Err(ExprError::Unbound(format!("`{path}` is not publishing")));
        }
        sources.push(id);
    }

    let port_ids: Vec<NodeId> = sources
        .iter()
        .map(|id| crate::dynamic::ops::db_source::from_db_id(*id))
        .collect();
    let system_id = compiled.system_hash(0, &port_ids);
    let value_id = program::field_id(system_id, 0);

    // Two views typing the same expression against the same components reach
    // the same id, and therefore share one running system.
    if let Some(node) = cx.global::<Expressions>().get(value_id.into()) {
        return Ok(Expression {
            node,
            _system: cx
                .global::<Expressions>()
                .get(system_id.into())
                .ok_or_else(|| ExprError::Unbound("this expression stopped".into()))?,
        });
    }

    let worker = cx.global::<DynamicWorker>().handle().clone();
    let built = {
        let compiled = compiled.clone();
        let db = db.clone();
        worker.call(move || -> Result<Expression, BuildError> {
            let mut ports = Vec::with_capacity(sources.len());
            for id in &sources {
                ports.push(crate::dynamic::ops::db_source::from_db(&db, *id)?);
            }
            let system = program::system(&compiled, 0, ports, DEFAULT_FUEL, None)?;
            let node = program::field(&compiled, 0, 0, system.node.clone())?;
            Ok(Expression {
                node,
                _system: system.node,
            })
        })
    };
    let built = built
        .ok_or_else(|| ExprError::Unbound("the node worker is gone".into()))?
        .map_err(|e| ExprError::Unbound(e.to_string()))?;

    let registry = cx.global_mut::<Expressions>();
    registry
        .live
        .insert(built.component_id(), Arc::downgrade(&built.node));
    registry
        .live
        .insert(built._system.id().into(), Arc::downgrade(&built._system));
    Ok(built)
}
