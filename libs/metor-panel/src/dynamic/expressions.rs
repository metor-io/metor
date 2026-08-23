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
//! ## Ephemeral means hidden, not unregistered
//!
//! An expression's output *is* registered as a db component — named by its
//! content hash and marked `hidden`, which is the flag the db already has for
//! "queryable by id, absent from pickers and browsers". Registration is what
//! buys history, and history is what every view that draws a line needs: a
//! plot reads `component.time_series`, not a stream, so a trace bound to a
//! bare ring would wait forever and draw nothing.
//!
//! Hiddenness, not absence, is therefore what makes an expression ephemeral.
//! It never appears anywhere an operator picks from; it simply exists where
//! views can read it, exactly as a `Persist`ed node does.
//!
//! **The component outlives the expression, deliberately.** The db is
//! insert-only — there is no `remove_component` — so when the last view drops
//! an expression its task stops and its ring goes quiet, while the component
//! record stays behind holding whatever history it accumulated. That is the
//! conservative direction to be wrong in: a stale hidden component is
//! invisible and costs a directory, whereas removing one out from under a
//! view still reading it would not be recoverable. Reclaiming them is a sweep
//! at startup, when nothing can hold a reference.
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
    live: HashMap<ComponentId, ExpressionRef>,
}

/// What the registry keeps: every part of a running expression, weakly.
///
/// All three have to come back together — a view that finds only the value
/// node would hold the component alive while the system feeding it stopped —
/// so an entry is live only while the whole chain is.
struct ExpressionRef {
    node: Weak<dyn DynamicNode>,
    system: Weak<dyn DynamicNode>,
    field: Weak<dyn DynamicNode>,
    component: ComponentId,
}

impl Global for Expressions {}

impl Expressions {
    pub fn init(cx: &mut App) {
        cx.set_global(Expressions::default());
    }

    /// The expression behind a component id, if a view still holds it.
    pub fn get(&self, id: ComponentId) -> Option<Expression> {
        let entry = self.live.get(&id)?;
        Some(Expression {
            node: entry.node.upgrade()?,
            component: entry.component,
            _system: entry.system.upgrade()?,
            _field: entry.field.upgrade()?,
        })
    }

    /// Whether a component id names an expression this session started.
    ///
    /// A hidden component left behind by a previous session answers `false`:
    /// it is a record with history, not something still computing.
    pub fn is_live(&self, id: ComponentId) -> bool {
        self.live.get(&id).is_some_and(|e| e.node.strong_count() > 0)
    }
}

/// A running `=` expression, owned by the view that asked for it.
///
/// Dropping this is what stops the expression computing — assuming no other
/// view is showing the same one.
#[derive(Clone)]
pub struct Expression {
    /// The node writing into the component, which is also what a view can
    /// bind to directly when it wants the live stream rather than history.
    pub node: Arc<dyn DynamicNode>,
    /// The hidden component this expression publishes into. Views bind to
    /// this and take the ordinary path — history, LoD, alarms and all.
    pub component: ComponentId,
    /// Held for their drop: the system feeding the component, and the field
    /// node between them.
    _system: Arc<dyn DynamicNode>,
    _field: Arc<dyn DynamicNode>,
}

impl Expression {
    pub fn component_id(&self) -> ComponentId {
        self.component
    }
}

/// What an expression's component is called.
///
/// Derived from the content hash rather than from the text, because the text
/// is not the identity: two expressions reading the same channels through the
/// same resolved paths are the same computation however they were spelled, and
/// the same text against a different component tree is not.
pub fn component_name(hash: NodeId) -> String {
    format!("expr.{:016x}", hash.0)
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
    let name = component_name(program::field_id(system_id, 0));
    let component = ComponentId(crate::dynamic::ops::persist::component_id_for_name(&name));

    // Two views typing the same expression against the same components reach
    // the same hash, and therefore share one running system rather than each
    // starting a copy of it.
    if let Some(running) = cx.global::<Expressions>().get(component) {
        return Ok(running);
    }

    let worker = cx.global::<DynamicWorker>().handle().clone();
    let built = {
        let compiled = compiled.clone();
        let db = db.clone();
        let name = name.clone();
        let label = body(text).to_string();
        worker.call(move || -> Result<Expression, BuildError> {
            let mut ports = Vec::with_capacity(sources.len());
            for id in &sources {
                ports.push(program::PortSource {
                    node: crate::dynamic::ops::db_source::from_db(&db, *id)?,
                    seed: program::latest_sample(&db, *id),
                });
            }
            let system = program::system(&compiled, 0, ports, DEFAULT_FUEL, None)?;
            let field = program::field(&compiled, 0, 0, system.node.clone())?;
            let node = publish(&db, &name, field.clone(), &label)?;
            Ok(Expression {
                node,
                component,
                _system: system.node,
                _field: field,
            })
        })
    };
    let built = built
        .ok_or_else(|| ExprError::Unbound("the node worker is gone".into()))?
        .map_err(|e| ExprError::Unbound(e.to_string()))?;

    cx.global_mut::<Expressions>().live.insert(
        component,
        ExpressionRef {
            node: Arc::downgrade(&built.node),
            system: Arc::downgrade(&built._system),
            field: Arc::downgrade(&built._field),
            component,
        },
    );
    Ok(built)
}

/// Register an expression's output as the hidden component views read it
/// through.
///
/// Publishing through `persist` is what gives the expression a time series,
/// and a time series is what every view that draws a line reads — a plot asks
/// the db for history, not for a stream. The node it returns writes into the
/// component's own WAL, so binding to either reaches the same bytes.
///
/// The metadata is then rewritten: `persist` named the component by its hash,
/// which is right for identity and useless in a legend, so the label becomes
/// the text the operator typed and the `hidden` flag goes on. Neither touches
/// the id, which was derived from the hash and stays there.
pub(crate) fn publish(
    db: &DB,
    name: &str,
    node: Arc<dyn DynamicNode>,
    label: &str,
) -> Result<Arc<dyn DynamicNode>, BuildError> {
    use metor_proto_wkt::MetadataExt;
    let published = crate::dynamic::ops::persist::persist(db, name.to_string(), node)?;
    let mut metadata = metor_proto_wkt::ComponentMetadata {
        component_id: ComponentId(crate::dynamic::ops::persist::component_id_for_name(name)),
        name: label.to_string(),
        metadata: Default::default(),
    };
    metadata.set("source", "dynamic");
    metadata.set("hidden", "true");
    if let Err(err) = db.with_state_mut(|s| s.set_component_metadata(metadata, &db.path)) {
        tracing::warn!(?err, "expression: could not hide its component");
    }
    Ok(published)
}
