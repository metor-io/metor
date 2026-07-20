//! The system registry: the `type="..."` → factory map [`resolve`](super::resolve)
//! instantiates static systems through.
//!
//! An app builds a [`Registry`] (usually from [`with_builtins`](Registry::with_builtins))
//! and calls [`register`](Registry::register)/[`register_pack`](Registry::register_pack)
//! to add its own systems, each keyed by a `type=` string. A registered entry
//! carries a boxed factory — decode params, `new` the system, run its
//! host-side configure step, erase it to a [`Node`] — plus the static
//! descriptor, available without construction so the resolver can order
//! registrations by capability. The cyclic-versus-async branch is inferred
//! from the [`IntoNode`] blanket impls, so a system registers by implementing
//! [`BuildSystem`](crate::BuildSystem) alone.
//!
//! Params reach a factory as a [`StaticParams`] surface — a value tree decoded
//! through serde (field defaults honored, unknown keys rejected), or a
//! paramless surface that decodes an all-defaults value.

use std::collections::HashMap;

use crate::binder::BindPorts;
use crate::coordinator::init::{self, Node};
use crate::descriptor::SystemDescriptor;
use crate::message::MsgTable;
use crate::system::{AsyncSystem, BuildCtx, BuildSystem, ConfigureError, CyclicSystem, HealthOutput};

use super::error::{LoadError, LoadErrorKind};

/// Everything a factory decodes to build a [`Node`], erased of the concrete
/// system type.
pub(crate) struct LoadCtx<'a> {
    /// Where the instance's params come from.
    pub params: StaticParams<'a>,
    /// The instance name the system registers under.
    pub name: &'a str,
    /// The registry's message table, which [`BuildSystem::configure`] resolves
    /// config name tokens against.
    pub msgs: &'a MsgTable,
    /// The mission namespace, if any, forwarded to
    /// [`BuildSystem::configure`] so a system that hashes authored component
    /// names (the alarm engine) can prefix them to match the qualified
    /// registry.
    pub namespace: Option<&'a str>,
}

/// The params surface a static-path factory decodes, the typed twin of the dl
/// path's [`ParamSource`](super::ParamSource) reduction.
pub(crate) enum StaticParams<'a> {
    /// A params value tree. `src` is the best-available diagnostic snippet
    /// (a value tree carries no spans of its own), anchored by the spec's
    /// [`SourceRef`](super::SourceRef) when it has one.
    Value {
        value: &'a serde_json::Value,
        src: &'a str,
    },
    /// No params. Decodes as an all-defaults value, so a required field is
    /// reported as the same missing param an empty params object raises.
    None,
}

impl StaticParams<'_> {
    /// The diagnostic source text, for errors outside the params surface.
    fn diag_src(&self, name: &str) -> String {
        match self {
            StaticParams::Value { src, .. } => (*src).to_string(),
            StaticParams::None => format!("system \"{name}\""),
        }
    }
}

/// A registered factory: decode `ctx.params`, construct the system, run its
/// host-side configure step, and return the finished [`Node`] for the resolver
/// to push. Boxed `Fn`, not a bare `fn`: a pack entry's factory closes over the
/// shared entry it instantiates.
type SystemFactory = Box<dyn Fn(&mut LoadCtx) -> Result<Node, LoadError>>;

/// Marker for a cyclic system's [`IntoNode`] impl.
pub struct CyclicKind;
/// Marker for an async system's [`IntoNode`] impl.
pub struct AsyncKind;

/// Turns a constructed system into an init-graph [`Node`] via the right
/// erasure helper, and reports its static descriptor. The `Kind` parameter
/// keeps the cyclic and async blanket impls from overlapping (a type could in
/// principle implement both system traits), so a single [`Registry::register`]
/// covers either.
///
/// The trait is public (it names a bound on the public
/// [`Registry::register`]), but `into_node` yields the crate-private
/// `Node`: a system author only ever satisfies the trait, never calls it, so
/// the internal return type stays internal.
#[allow(private_interfaces)]
pub trait IntoNode<Kind>: Sized {
    fn into_node(self, name: String) -> Node;
    fn descriptor() -> SystemDescriptor;
}

#[allow(private_interfaces)]
impl<S> IntoNode<CyclicKind> for S
where
    S: CyclicSystem + 'static,
    S::Output: HealthOutput + BindPorts + 'static,
    S::Input: BindPorts + 'static,
{
    fn into_node(self, name: String) -> Node {
        init::cyclic_node(name, self)
    }
    fn descriptor() -> SystemDescriptor {
        <S as CyclicSystem>::descriptor()
    }
}

#[allow(private_interfaces)]
impl<S> IntoNode<AsyncKind> for S
where
    S: AsyncSystem + 'static,
    S::Input: BindPorts + 'static,
    S::Output: BindPorts + 'static,
{
    fn into_node(self, name: String) -> Node {
        init::async_node(name, self)
    }
    fn descriptor() -> SystemDescriptor {
        <S as AsyncSystem>::descriptor()
    }
}

/// The factory [`Registry::register`] stores for one concrete type:
/// deserialize params, `new` the system, run its host-side configure step, and
/// erase it to a [`Node`] behind a plain `fn` pointer. `()` params deserialize
/// from a paramless surface and reject stray properties. The node carries the
/// instance descriptor, not the static one; a system whose configure step
/// minted ports resolves edges against what this instance actually carries.
fn factory<S, K>(ctx: &mut LoadCtx) -> Result<Node, LoadError>
where
    S: BuildSystem + IntoNode<K>,
    S::Params: serde::de::DeserializeOwned,
{
    let params = decode_static_params::<S::Params>(&ctx.params, ctx.name)?;
    let mut system = S::new(params);
    // Resolve config references (message name tokens) against the registry
    // tables the params value cannot carry.
    system
        .configure(&BuildCtx {
            msgs: ctx.msgs,
            namespace: ctx.namespace,
        })
        .map_err(|e| match e {
            ConfigureError::UnknownMsg { name, available } => LoadErrorKind::UnknownMsgName {
                system: ctx.name.to_string(),
                msg: name,
                available: available.join(", "),
            }
            .whole(ctx.params.diag_src(ctx.name)),
        })?;
    Ok(system.into_node(ctx.name.to_string()))
}

/// Decode a typed `Params` off a [`StaticParams`] surface: a value tree
/// through serde (unknown keys rejected), no params through the all-defaults
/// [`NoParams`] deserializer.
fn decode_static_params<P: serde::de::DeserializeOwned>(
    params: &StaticParams,
    name: &str,
) -> Result<P, LoadError> {
    match params {
        StaticParams::Value { value, src } => decode_value_params(value, name, src),
        StaticParams::None => P::deserialize(NoParams).map_err(|e| {
            LoadErrorKind::ValueParams {
                system: name.to_string(),
                reason: e.to_string(),
            }
            .at(format!("system \"{name}\""), (0, name.len() + 9).into())
        }),
    }
}

/// A serde deserializer for a params surface that carries no fields: it yields
/// the unit value for `()` and an empty map for a struct (so `#[serde(default)]`
/// fields fill in and a required field is a clean missing-field error). A
/// `serde_json::Value` alone cannot serve both, since `()` needs `Null` and a
/// defaulted struct needs `{}`.
pub(crate) struct NoParams;

impl<'de> serde::Deserializer<'de> for NoParams {
    type Error = serde::de::value::Error;

    fn deserialize_any<V: serde::de::Visitor<'de>>(self, v: V) -> Result<V::Value, Self::Error> {
        // Default to an empty map, so a struct with defaulted fields decodes.
        v.visit_map(serde::de::value::MapDeserializer::new(std::iter::empty::<(
            &str,
            &str,
        )>()))
    }

    fn deserialize_unit<V: serde::de::Visitor<'de>>(self, v: V) -> Result<V::Value, Self::Error> {
        v.visit_unit()
    }

    fn deserialize_unit_struct<V: serde::de::Visitor<'de>>(
        self,
        _name: &'static str,
        v: V,
    ) -> Result<V::Value, Self::Error> {
        v.visit_unit()
    }

    fn deserialize_option<V: serde::de::Visitor<'de>>(self, v: V) -> Result<V::Value, Self::Error> {
        v.visit_none()
    }

    fn deserialize_newtype_struct<V: serde::de::Visitor<'de>>(
        self,
        _name: &'static str,
        v: V,
    ) -> Result<V::Value, Self::Error> {
        v.visit_newtype_struct(self)
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf seq tuple tuple_struct map struct enum identifier ignored_any
    }
}

/// Deserialize a typed `Params` from a value tree.
///
/// Unlike the dl path's schema conformance, this is plain serde, so
/// `#[serde(default)]` field attributes are honored. `serde_ignored` supplies
/// the typo guard serde_json's deserializer lacks: a key no field consumed is
/// an [`UnknownParam`](LoadErrorKind::UnknownParam), and any other decode failure
/// is a [`ValueParams`](LoadErrorKind::ValueParams) carrying serde's own message.
pub(crate) fn decode_value_params<P: serde::de::DeserializeOwned>(
    value: &serde_json::Value,
    system: &str,
    src: &str,
) -> Result<P, LoadError> {
    let mut unknown: Option<String> = None;
    let params = serde_ignored::deserialize(value, |path: serde_ignored::Path<'_>| {
        unknown.get_or_insert_with(|| path.to_string());
    })
    .map_err(|e| {
        LoadErrorKind::ValueParams {
            system: system.to_string(),
            reason: e.to_string(),
        }
        .whole(src)
    })?;
    if let Some(property) = unknown {
        return Err(LoadErrorKind::UnknownParam {
            property,
            system: system.to_string(),
        }
        .whole(src));
    }
    Ok(params)
}

/// One registered type: its factory plus the static descriptor, available
/// without constructing the system so [`resolve`](super::resolve) can order
/// registrations by capability.
pub(super) struct RegistryEntry {
    pub(super) factory: SystemFactory,
    descriptor: EntryDescriptor,
}

/// A registered type's descriptor without construction: computed on demand
/// for a type-registered system, carried by value for a pack entry.
enum EntryDescriptor {
    Fn(fn() -> SystemDescriptor),
    Value(Box<SystemDescriptor>),
}

impl EntryDescriptor {
    fn capabilities_contain(&self, cap: crate::Capability) -> bool {
        match self {
            EntryDescriptor::Fn(f) => f().capabilities.contains(&cap),
            EntryDescriptor::Value(d) => d.capabilities.contains(&cap),
        }
    }
}

/// The app-built map from a `type="..."` string to a system factory. It is an
/// explicit table; each system crate can expose a `pub fn register(&mut
/// Registry)` that adds its own systems.
#[derive(Default)]
pub struct Registry {
    pub(super) factories: HashMap<&'static str, RegistryEntry>,
    /// Message types added via [`register_msg`](Self::register_msg), keyed by
    /// [`NamedMsg::NAME`](crate::NamedMsg). Config name tokens (an uplink's
    /// `msgs` list) resolve against this table in [`BuildSystem::configure`].
    pub(super) msgs: MsgTable,
    /// Pack-declared shared states, keyed by their declaration name — the
    /// `type=` a [`StateSpec`](super::StateSpec) constructs through.
    pub(super) states: HashMap<&'static str, std::cell::RefCell<crate::pack::StateEntry>>,
}

impl Registry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// A registry pre-loaded with the built-in systems under their `type=`
    /// names — the alarm engine (`"Alarms"`) and the link pack: one shared
    /// `"TcpServer"` state serving the `"Downlink"` and `"Uplink"` systems
    /// attached to it — plus the well-known message set in the message table
    /// so a mission's `msgs` list can name any of them out of the box. An
    /// app-built registry starts here and adds its own systems.
    pub fn with_builtins() -> Self {
        use metor_proto_wkt::{
            AlarmAck, AlarmCleared, AlarmDef, AlarmDefs, AlarmRaised, ReloadSequences,
            SequenceChannelEvent, SequenceCommand, SequenceRegistry,
        };
        use crate::telemetry::{LinkParams, LinkState, TelemetrySystem, UplinkSystem};

        let mut r = Self::new();
        r.register::<crate::AlarmSystem, _>("Alarms");
        let mut link_pack = crate::Pack::new();
        let link = link_pack.shared_state("TcpServer", |p: LinkParams| LinkState::bind(p.addr));
        let link_pack = link_pack
            .system_type_shared::<TelemetrySystem, _>("Downlink", &link, {
                let link = link.clone();
                move |p| <TelemetrySystem as BuildSystem>::new(p).attach(link.clone())
            })
            .system_type_shared::<UplinkSystem, _>("Uplink", &link, {
                let link = link.clone();
                move |p| <UplinkSystem as BuildSystem>::new(p).attach(link.clone())
            });
        r.register_pack(link_pack);
        r.register_msg::<SequenceCommand>()
            .register_msg::<SequenceRegistry>()
            .register_msg::<SequenceChannelEvent>()
            .register_msg::<ReloadSequences>()
            .register_msg::<AlarmDef>()
            .register_msg::<AlarmDefs>()
            .register_msg::<AlarmRaised>()
            .register_msg::<AlarmCleared>()
            .register_msg::<AlarmAck>()
            .register_msg::<metor_proto_wkt::LogEvent>();
        r
    }

    /// Register message type `M` under its stable
    /// [`NamedMsg::NAME`](crate::NamedMsg) token so config lists can name it.
    /// Idempotent.
    pub fn register_msg<M: crate::NamedMsg>(&mut self) -> &mut Self {
        self.msgs.insert::<M>();
        self
    }

    /// Register concrete system `S` under `type_name`.
    ///
    /// `S` supplies its params type and `new` via the format-independent
    /// [`BuildSystem`](crate::BuildSystem); the only params requirement is
    /// `S::Params: DeserializeOwned`, the same derive the postcard contract
    /// already needs, so a statically linked system registers by implementing
    /// `BuildSystem` alone. The cyclic-versus-async branch comes from the
    /// [`IntoNode`] blanket impls, inferred here.
    pub fn register<S, K>(&mut self, type_name: &'static str) -> &mut Self
    where
        S: BuildSystem + IntoNode<K> + 'static,
        S::Params: serde::de::DeserializeOwned,
        K: 'static,
    {
        self.factories.insert(
            type_name,
            RegistryEntry {
                factory: Box::new(factory::<S, K>),
                descriptor: EntryDescriptor::Fn(<S as IntoNode<K>>::descriptor),
            },
        );
        self
    }

    /// Register every entry of a pack under its entry name as the `type=`
    /// key, so the same `pack()` a cdylib exports serves a statically-linked
    /// mission. Two instances of one entry construct through the same shared
    /// entry (a non-reloadable `.state(...)` entry rejects the second).
    pub fn register_pack(&mut self, pack: crate::Pack) -> &mut Self {
        use std::cell::RefCell;
        use std::rc::Rc;

        let (entries, states) = pack.into_parts();
        for state in states {
            self.states.insert(state.name(), RefCell::new(state));
        }
        for entry in entries {
            let name = entry.name();
            let descriptor = Box::new(entry.descriptor().clone());
            let entry = Rc::new(RefCell::new(entry));
            let factory = Box::new(move |ctx: &mut LoadCtx| {
                let mut entry = entry.borrow_mut();
                // A paramless surface conforms an empty object against the
                // entry's schema, so its declared defaults fill in — the pack
                // twin of the static `NoParams` decode.
                let empty = serde_json::Value::Object(serde_json::Map::new());
                let synthesized_src;
                let (value, src) = match &ctx.params {
                    StaticParams::Value { value, src } => (*value, *src),
                    StaticParams::None => {
                        synthesized_src = ctx.params.diag_src(ctx.name);
                        (&empty, synthesized_src.as_str())
                    }
                };
                let params = crate::pack::EntryParams::Value {
                    value,
                    src,
                    name: ctx.name,
                    msgs: ctx.msgs,
                };
                // The create phase runs here (a bad config fails at registration);
                // the returned node rides the ordinary cyclic bind path.
                init::pending_node(ctx.name.to_string(), &mut entry, params).map_err(|e| match e {
                    // The params decode already carries its span; unwrap it.
                    crate::pack::MakeError::Params(e) => *e,
                    other => LoadErrorKind::PackCreate {
                        system: ctx.name.to_string(),
                        message: other.to_string(),
                    }
                    .whole(ctx.params.diag_src(ctx.name)),
                })
            });
            self.factories.insert(
                name,
                RegistryEntry {
                    factory,
                    descriptor: EntryDescriptor::Value(descriptor),
                },
            );
        }
        self
    }

    /// Whether `ty` names a registered system whose descriptor carries
    /// [`Capability::ReceiveAll`](crate::Capability). Unknown types answer
    /// `false`; the systems pass reports them as [`LoadErrorKind::UnknownType`] in
    /// document order.
    pub(super) fn is_receive_all(&self, ty: Option<&str>) -> bool {
        ty.and_then(|ty| self.factories.get(ty)).is_some_and(|e| {
            e.descriptor
                .capabilities_contain(crate::Capability::ReceiveAll)
        })
    }
}

/// Register a concrete system on a [`Registry`] under a `type=` name, keeping
/// the call site terse: `register_system!(registry, ImuDriver => "ImuDriver")`.
#[macro_export]
macro_rules! register_system {
    ($registry:expr, $ty:ty => $name:expr) => {
        $registry.register::<$ty, _>($name)
    };
}
