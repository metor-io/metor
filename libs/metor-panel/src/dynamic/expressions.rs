//! View-owned `=` expressions shared through a weak registry.

use std::collections::HashMap;
use std::sync::{Arc, Weak};

use gpui::{App, Global};
use metor_db::DB;
use metor_expr::Diagnostics;
use metor_proto::types::ComponentId;

use crate::dynamic::ops::program::{self, Compiled, DEFAULT_FUEL};
use crate::dynamic::resolver::DbResolver;
use crate::dynamic::worker::DynamicWorker;
use crate::dynamic::{BuildError, DynamicNode, NodeId};

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

/// What a saved binding turned out to name.
pub struct Bound {
    /// The component to read. Every view binds to one of these, and none of
    /// them has to learn what an expression is.
    pub id: ComponentId,
    /// The running expression, when the binding was one — for a view that
    /// wants to show the text or keep a share of its lifetime.
    pub expression: Option<Expression>,
}

/// Resolve a serialized binding, whichever kind it is.
///
/// This is the *one* rule for turning saved text into something to read: a
/// plain name hashes to its component, as it always did, and an `=` binding is
/// compiled and started so that what comes back is the hidden component it
/// publishes into.
///
/// Every consumer needs it, and the reason is worth stating plainly. Hashing
/// an expression's text as though it were a name yields an id nobody
/// publishes, so the view binds to nothing and shows nothing — a panel that
/// looks configured and is dead. `ComponentId::new("=a + b")` is not a
/// component.
pub fn bind(text: &str, db: &Arc<DB>, cx: &mut App) -> Result<Bound, ExprError> {
    if !is_expression(text) {
        return Ok(Bound {
            id: ComponentId::new(text),
            expression: None,
        });
    }
    let expression = resolve(text, db, cx)?;
    Ok(Bound {
        id: expression.component_id(),
        expression: Some(expression),
    })
}

/// What to serialize for a binding that may be an expression.
///
/// The counterpart to [`bind`], and the reason a round trip closes: a view
/// stores text, so an expression has to store text [`bind`] will recognise.
/// Its component's metadata carries the operator's own words — `persist` names
/// the component by content hash and then labels it with what was typed — so
/// the sigil goes back on and the pair round-trips.
///
/// `None` when the id is not an expression's component, which is the caller's
/// cue to keep the component name it already has.
pub fn binding_text(db: &DB, id: ComponentId) -> Option<String> {
    use metor_proto_wkt::MetadataExt;
    let text = db.with_state(|state| {
        state
            .get_component_metadata(id)
            .and_then(|meta| meta.get("expression"))
            .map(str::to_string)
    })?;
    Some(format!("{SIGIL}{text}"))
}

pub fn running(id: ComponentId, cx: &App) -> Option<Expression> {
    cx.try_global::<Expressions>()?.get(id)
}

/// Live `=` expressions, keyed without extending their lifetime.
#[derive(Default)]
pub struct Expressions {
    live: HashMap<ComponentId, Weak<ExpressionInner>>,
}

impl Global for Expressions {}

impl Expressions {
    pub fn init(cx: &mut App) {
        cx.set_global(Expressions::default());
    }

    /// The expression behind a component id, if this session started one.
    pub fn get(&self, id: ComponentId) -> Option<Expression> {
        self.live.get(&id)?.upgrade().map(Expression)
    }

    /// Whether a component id names an expression this session is running.
    ///
    /// A hidden component left behind by an earlier session answers `false`:
    /// it is a record with history, not something still computing.
    pub fn is_live(&self, id: ComponentId) -> bool {
        self.live.get(&id).and_then(Weak::upgrade).is_some()
    }

    /// How many expressions are running, for a sweep to reason about.
    pub fn len(&self) -> usize {
        self.live
            .values()
            .filter(|entry| entry.strong_count() > 0)
            .count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Take ownership of a running expression, keyed by what it publishes.
    pub fn insert(&mut self, expression: Expression) {
        self.live
            .retain(|_, expression| expression.strong_count() > 0);
        self.live
            .insert(expression.component_id(), Arc::downgrade(&expression.0));
    }
}

/// A running `=` expression, owned by the view that asked for it.
///
/// Dropping this is what stops the expression computing — assuming no other
/// view is showing the same one.
#[derive(Clone)]
pub struct Expression(Arc<ExpressionInner>);

struct ExpressionInner {
    _node: Arc<dyn DynamicNode>,
    /// The hidden component this expression publishes into. Views bind to
    /// this and take the ordinary path — history, LoD, alarms and all.
    pub component: ComponentId,
    /// Held for their drop: the system feeding the component, and the field
    /// node between them.
    _system: Arc<dyn DynamicNode>,
    _field: Arc<dyn DynamicNode>,
}

impl Expression {
    /// Assemble the parts of a running expression.
    ///
    /// `system` and `field` are held only for their drop — cancelling their
    /// tasks is what stops an expression — so they are taken here rather than
    /// exposed as fields nobody should read.
    pub fn new(
        node: Arc<dyn DynamicNode>,
        component: ComponentId,
        system: Arc<dyn DynamicNode>,
        field: Arc<dyn DynamicNode>,
    ) -> Self {
        Expression(Arc::new(ExpressionInner {
            _node: node,
            component,
            _system: system,
            _field: field,
        }))
    }

    pub fn component_id(&self) -> ComponentId {
        self.0.component
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

/// The component each port of a compiled expression reads.
///
/// Ids are taken from the resolver that found them, never re-derived from the
/// path. A component's id belongs to whoever created it, and hashing the name
/// a second time agrees with the real id for only about half of all names —
/// which fails as "not publishing" for a component that is publishing
/// perfectly well.
pub(crate) fn port_components(
    manifest: &metor_expr::Manifest,
    resolver: &DbResolver,
) -> Result<Vec<ComponentId>, ExprError> {
    let mut sources = Vec::new();
    for port in &manifest.systems[0].inputs {
        let metor_expr::Binding::Component(path) = &port.bindings[0] else {
            return Err(ExprError::Unbound(
                "an expression reads components, not systems".into(),
            ));
        };
        let id = resolver
            .id_of(path)
            .ok_or_else(|| ExprError::Unbound(format!("`{path}` is not a known component")))?;
        sources.push(id);
    }
    Ok(sources)
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
    let sources = port_components(&compiled.manifest, &resolver)?;

    let port_ids: Vec<NodeId> = sources
        .iter()
        .map(|id| crate::dynamic::ops::db_source::from_db_id(*id))
        .collect();
    let system_id = compiled.system_hash(0, &port_ids);
    let name = component_name(program::field_id(system_id, 0));
    let component = ComponentId::new(&name);

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
            Ok(Expression::new(node, component, system.node, field))
        })
    };
    let built = built
        .ok_or_else(|| ExprError::Unbound("the node worker is gone".into()))?
        .map_err(|e| ExprError::Unbound(e.to_string()))?;

    // The registry keeps it running. Nothing else will: the caller is handed
    // a component id, and an id cannot own anything.
    cx.global_mut::<Expressions>().insert(built.clone());
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
        component_id: ComponentId::new(name),
        name: label.to_string(),
        metadata: Default::default(),
    };
    metadata.set("source", "dynamic");
    metadata.set("hidden", "true");
    // What the operator typed, recorded on the component itself. A view
    // serializes text, so reloading one has to recover text — and asking the
    // registry would only work while the session that made it is still
    // running. This is a fact about the component, so it outlives the session
    // exactly as the component does.
    metadata.set("expression", label);
    if let Err(err) = db.with_state_mut(|s| s.set_component_metadata(metadata, &db.path)) {
        tracing::warn!(?err, "expression: could not hide its component");
    }
    Ok(published)
}
