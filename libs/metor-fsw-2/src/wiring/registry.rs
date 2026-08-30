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

use crate::async_system::AsyncSystem;
use crate::coordinator::init::{self, Node};
use metor_fsw_2_core::BindPorts;
use metor_fsw_2_core::MsgTable;
use metor_fsw_2_core::SystemDescriptor;
use metor_fsw_2_core::{BuildCtx, BuildSystem, ConfigureError, CyclicSystem, LogOutput};

use metor_fsw_2_core::params::{NoParams, ParamErrorKind, decode_value_params};

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
    /// The target namespace, if any, forwarded to
    /// [`BuildSystem::configure`] so a system that hashes authored component
    /// names (the alarm engine) can prefix them to match the qualified
    /// registry.
    pub namespace: Option<&'a str>,
    /// The shared state this instance attaches to, resolved by name from
    /// [`SystemSpec::attach`](super::SystemSpec) during the states pass.
    /// `None` for an ordinary system; a shared-state entry's factory requires
    /// it.
    pub attach: Option<&'a metor_fsw_2_core::AttachTarget>,
}

/// The params surface a static-path factory decodes, the typed twin of the dl
/// path's [`ParamSource`](super::ParamSource) reduction.
pub(crate) enum StaticParams<'a> {
    /// A params value tree.
    Value(&'a serde_json::Value),
    /// No params. Decodes as an all-defaults value, so a required field is
    /// reported as the same missing param an empty params object raises.
    None,
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

/// Turns a constructed system into an internal graph node via the right
/// type-erasure helper, and reports its static descriptor. The `Kind` parameter
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
    S::Output: LogOutput + BindPorts + 'static,
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
            .bare(),
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
        StaticParams::Value(value) => decode_value_params(value, name, "").map_err(LoadError::from),
        StaticParams::None => P::deserialize(NoParams).map_err(|e| {
            ParamErrorKind::ValueParams {
                system: name.to_string(),
                reason: e.to_string(),
            }
            .at(format!("system \"{name}\""), (0, name.len() + 9).into())
            .into()
        }),
    }
}

/// One registered type: its factory plus the static descriptor, available
/// without constructing the system so [`resolve`](super::resolve) can order
/// registrations by capability.
pub(super) struct RegistryEntry {
    pub(super) factory: SystemFactory,
    descriptor: EntryDescriptor,
    /// `true` for a pack entry that attaches to a pack-shared state by name
    /// ([`Pack::system_type_shared`](crate::Pack::system_type_shared)): the
    /// resolver requires its spec to carry an `attach`, and rejects an
    /// `attach` on any entry where this is `false`.
    pub(super) shared: bool,
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
    pub(super) states: HashMap<&'static str, std::cell::RefCell<metor_fsw_2_core::StateEntry>>,
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
    /// so a target's `msgs` list can name any of them out of the box. An
    /// app-built registry starts here and adds its own systems.
    pub fn with_builtins() -> Self {
        use crate::telemetry::{LinkParams, LinkState, TelemetrySystem, UplinkSystem};
        use metor_proto_wkt::{
            AlarmAck, AlarmCleared, AlarmDef, AlarmDefs, AlarmRaised, ReloadSequences,
            SequenceChannelEvent, SequenceCommand, SequenceRegistry,
        };

        let mut r = Self::new();
        r.register::<crate::AlarmSystem, _>("Alarms");
        r.register::<crate::PresetSystem, _>("Presets");
        let mut link_pack = crate::Pack::new();
        link_pack.shared_state("TcpServer", |p: LinkParams| {
            LinkState::bind(p.addr).map(|s| s.with_name(p.name))
        });
        // The `Downlink`/`Uplink` entries no longer capture the link token:
        // the resolver hands each `ctor` the `Shared<LinkState>` a target
        // named via `attach=`, and the ctor attaches it.
        let link_pack = link_pack
            .system_type_shared::<TelemetrySystem, LinkState>("Downlink", |p, link| {
                <TelemetrySystem as BuildSystem>::new(p).attach(link)
            })
            .system_type_shared::<UplinkSystem, LinkState>("Uplink", |p, link| {
                <UplinkSystem as BuildSystem>::new(p).attach(link)
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
            .register_msg::<metor_proto_wkt::LogEvent>()
            .register_msg::<metor_proto_wkt::PresetDefs>();
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
                shared: false,
            },
        );
        self
    }

    /// Register every entry of a pack under its entry name as the `type=`
    /// key, so the same `pack()` a cdylib exports serves a statically-linked
    /// target. Two instances of one entry construct through the same shared
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
            let shared = entry.shared;
            let entry = Rc::new(RefCell::new(entry));
            let factory = Box::new(move |ctx: &mut LoadCtx| {
                let mut entry = entry.borrow_mut();
                // A paramless surface conforms an empty object against the
                // entry's schema, so its declared defaults fill in — the pack
                // twin of the static `NoParams` decode.
                let empty = serde_json::Value::Object(serde_json::Map::new());
                let value = match &ctx.params {
                    StaticParams::Value(value) => *value,
                    StaticParams::None => &empty,
                };
                let params = metor_fsw_2_core::EntryParams::Value {
                    value,
                    src: "",
                    name: ctx.name,
                    msgs: ctx.msgs,
                    attach: ctx.attach,
                };
                // The create phase runs here (a bad config fails at registration);
                // the returned node rides the ordinary cyclic bind path.
                init::pending_node(ctx.name.to_string(), &mut entry, params).map_err(|e| match e {
                    // Preserve the params-specific error kind.
                    metor_fsw_2_core::MakeError::Params(e) => (*e).into(),
                    // A by-name attach that named a wrong-typed state: refine
                    // the generic create error into the attach diagnostic.
                    metor_fsw_2_core::MakeError::AttachTypeMismatch { system, state } => {
                        LoadErrorKind::AttachTypeMismatch {
                            system: system.to_string(),
                            attach: state.to_string(),
                        }
                        .bare()
                    }
                    // Reached only when a shared entry's create runs without a
                    // resolved attach; the resolver's pre-check normally
                    // pre-empts it with `MissingAttach`.
                    metor_fsw_2_core::MakeError::MissingAttach { system } => {
                        LoadErrorKind::MissingAttach {
                            system: system.to_string(),
                        }
                        .bare()
                    }
                    other => LoadErrorKind::PackCreate {
                        system: ctx.name.to_string(),
                        message: other.to_string(),
                    }
                    .bare(),
                })
            });
            self.factories.insert(
                name,
                RegistryEntry {
                    factory,
                    descriptor: EntryDescriptor::Value(descriptor),
                    shared,
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
