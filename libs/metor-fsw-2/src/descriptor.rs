//! Static self-description of a system's ports, read before any port exists.
//!
//! A coordinator sizes rings, validates wiring, and allocates buffers from a
//! [`SystemDescriptor`] alone, so everything here is derived from static
//! metadata (`F::FRAME_ID`, `F::as_vtable()`, `M::ID`) and never needs a
//! constructed system.
//!
//! There is one port concept with three orthogonal axes:
//!
//! ```text
//! schema     Table(VTable) | Postcard(PacketId)      what a record is
//! delivery   Snapshot | Log                          what a consumer reads
//! fan-in     One | Many                              how many producers an input takes
//! ```
//!
//! A "frame port" and a "message port" are two configurations of that one
//! concept. [`PortDesc::of`] mints `Table × Snapshot × One` and
//! [`PortDesc::msg`] mints `Postcard × Log × Many`. Beside the axes sit two
//! more pieces of shape: [`PortConn`] names who provides the other end of a
//! port, and a [`Capability`] is a bind-time grant that is not a port at all.
//!
//! [`compatible`] is the one check that decides whether a producer output may
//! feed a consumer input.

use std::collections::HashMap;
use std::sync::Arc;

use metor_fsw::{AsVTable, Metadatatize};
use metor_proto::types::{ComponentId, PacketId, PrimType};
use metor_proto::vtable::VTable;
use metor_proto::vtable::builder::vtable;
use metor_proto_wkt::ComponentMetadata;

use crate::frame::Frame;
use crate::message::{MAX_MSG_BYTES, NamedMsg};

/// A frequency in cycles per second, the unit of the coordinator's cycle
/// rate.
pub type Hz = f64;

/// A closure that rebuilds a Table port's announce [`VTable`] and component
/// metadata with every leaf id nested under an instance-name prefix.
///
/// An [`Arc`]'d closure rather than a bare `fn` so that a port built from
/// runtime metadata, with no static frame type behind it, can capture the
/// prefix rewrite it needs.
pub type AnnounceFn = Arc<dyn Fn(&str) -> (VTable, Vec<ComponentMetadata>) + Send + Sync>;

/// The key that decides which producer output may feed which consumer input
/// when an edge is wired.
///
/// The two variants draw from disjoint value spaces (an 8-byte frame
/// [`ComponentId`] versus a 2-byte [`PacketId`]), so ports of different
/// schemas can never accidentally satisfy the same edge.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum PortId {
    /// The key of a Table port, `F::FRAME_ID`.
    Component(ComponentId),
    /// The key of a Postcard port, `M::ID`.
    Packet(PacketId),
}

impl PortId {
    /// The frame [`ComponentId`] of a Table port, or `None` for a Postcard port.
    pub fn component(self) -> Option<ComponentId> {
        match self {
            PortId::Component(c) => Some(c),
            PortId::Packet(_) => None,
        }
    }

    /// The [`PacketId`] of a Postcard port, or `None` for a Table port.
    pub fn packet(self) -> Option<PacketId> {
        match self {
            PortId::Component(_) => None,
            PortId::Packet(p) => Some(p),
        }
    }
}

/// What one record is and how it is described (the schema axis).
#[derive(Clone)]
pub enum PortSchema {
    /// A component-frame table of `#[repr(C)]` bytes described by a [`VTable`].
    Table {
        /// The frame-relative (unprefixed) vtable that wiring compatibility
        /// compares. The prefixed form is produced on demand by `announce`.
        vtable: VTable,
        /// Re-derives the prefixed announce vtable and component metadata for
        /// a given instance name. See [`AnnounceFn`] for why it is a closure.
        announce: AnnounceFn,
    },
    /// A self-describing postcard record. The 2-byte [`PacketId`] is the whole
    /// schema, so there is no vtable and nothing to announce.
    Postcard,
}

/// What a consumer is expected to read off the channel (the delivery axis).
///
/// Delivery drives ring depth, telemetry coalescing, and cycle-detection
/// membership.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub enum Delivery {
    /// A state sample. Readers coalesce to the newest record.
    Snapshot,
    /// An event or command log. Every record is read, in order, never
    /// coalesced.
    Log,
}

/// How many producers may wire into an input (the fan-in axis). Outputs
/// ignore it; fan-out is always unbounded.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub enum FanIn {
    /// Exactly one edge, and it must be present.
    One,
    /// Zero, one, or many edges. Requires [`Delivery::Log`], since latest-wins
    /// across several producers is ill-defined; a Snapshot input declaring
    /// `Many` fails wiring with
    /// [`WireError::SnapshotFanIn`](crate::WireError::SnapshotFanIn).
    Many,
}

/// Who provides the other end of a port.
///
/// Only the coordinator's and the slot runner's own bundles use anything
/// other than [`Edge`](PortConn::Edge). A dynamically loaded system's ports
/// are always edge-connected, so this axis never crosses the load boundary.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PortConn {
    /// Wired by ordinary edges under the fan-in rules. The default.
    Edge,
    /// The system's runner holds the port's counterpart, writing a Host
    /// input's dedicated ring or the output ring that would otherwise be
    /// handed to the system. A Host output is still ring-allocated and
    /// registry-tapped like any output and may be consumed over ordinary
    /// edges. A Host input is exempt from the unconnected-input check and
    /// rejects edges with [`WireError::HostPort`](crate::WireError::HostPort).
    Host,
    /// A declared reader over one of this system's own outputs, named by
    /// [`PortId`]. Allocates no ring, counts one extra reader on that output,
    /// and hands the read view to the runner. Inputs only; edges are rejected
    /// with [`WireError::HostPort`](crate::WireError::HostPort).
    SelfTap(PortId),
}

/// A non-port resource granted to a system at bind time. Unlike a port it
/// reserves no ring and connects no edge.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub enum Capability {
    /// A read view over every telemetered output in the graph
    /// ([`AllOutputs`](crate::AllOutputs)). Counts one reader slot on every
    /// buffer at sizing time. Only a host-built system can hold it.
    ReceiveAll,
}

/// What one bundle field contributes to a descriptor, either a wired port or
/// a bind-time [`Capability`]. Collecting one decl per field lets a single
/// walk cover both; the bind cursor skips capability decls since they consume
/// no ring.
#[derive(Clone, Debug)]
pub enum PortDecl {
    Port(PortDesc),
    Capability(Capability),
}

impl PortDecl {
    /// Applies [`PortDesc::untelemetered`] to a port decl; a capability has
    /// no axes and passes through unchanged.
    pub fn untelemetered(self) -> Self {
        match self {
            PortDecl::Port(p) => PortDecl::Port(p.untelemetered()),
            cap => cap,
        }
    }

    /// The port of a [`Port`](PortDecl::Port) decl, or `None` for a capability.
    pub fn into_port(self) -> Option<PortDesc> {
        match self {
            PortDecl::Port(p) => Some(p),
            PortDecl::Capability(_) => None,
        }
    }
}

/// Splits one direction's decls into its wired ports and its capabilities,
/// preserving port order for the bind cursor.
pub fn split_decls(decls: Vec<PortDecl>) -> (Vec<PortDesc>, Vec<Capability>) {
    let mut ports = Vec::with_capacity(decls.len());
    let mut caps = Vec::new();
    for d in decls {
        match d {
            PortDecl::Port(p) => ports.push(p),
            PortDecl::Capability(c) => caps.push(c),
        }
    }
    (ports, caps)
}

/// The static shape of one port, everything the coordinator needs to size a
/// ring and check an edge before the owning system is constructed.
///
/// The same struct describes an output (a produced record stream) and an
/// input (a required shape); the direction is which list of a
/// [`SystemDescriptor`] it sits in. `fan_in` has no effect on outputs, and
/// `telemetered` has no effect on inputs.
#[derive(Clone)]
pub struct PortDesc {
    /// Edge key, derived from the schema by the constructors.
    pub id: PortId,
    /// Display, config-token, and registry-key name (`F::NAME` or `M::NAME`).
    /// The coordinator joins it with an instance name to form the qualified
    /// registry key `ComponentId::new("<instance>.<name>")` without needing a
    /// static type.
    pub name: &'static str,
    /// Worst-case record bytes (`F::MAX_SIZE` or [`MAX_MSG_BYTES`]), the
    /// input to [`crate::capacity_for`] when sizing a ring.
    pub max_size: usize,
    /// What a record is and how it is described.
    pub schema: PortSchema,
    /// Latest-wins snapshot or every-record log.
    pub delivery: Delivery,
    /// Producer cardinality for an input.
    pub fan_in: FanIn,
    /// Whether the downlink taps this output. Every port carries the flag, so
    /// frame outputs can opt out too.
    pub telemetered: bool,
    /// Who provides the other end.
    pub conn: PortConn,
}

impl std::fmt::Debug for PortDesc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The announce closure has no `Debug` impl, so render the schema
        // without it.
        let mut s = f.debug_struct("PortDesc");
        s.field("id", &self.id)
            .field("name", &self.name)
            .field("max_size", &self.max_size);
        match &self.schema {
            PortSchema::Table { vtable, .. } => s.field("schema", &"Table").field("vtable", vtable),
            PortSchema::Postcard => s.field("schema", &"Postcard"),
        };
        s.field("delivery", &self.delivery)
            .field("fan_in", &self.fan_in)
            .field("telemetered", &self.telemetered)
            .field("conn", &self.conn);
        s.finish()
    }
}

/// Re-derives `F`'s vtable and metadata under the dotted `prefix` (the
/// instance name), so the leaves roll the same ids as
/// `ComponentId::new("<prefix>.<frame>.<field>")`.
fn announce_of<F: Frame>(prefix: &str) -> (VTable, Vec<ComponentMetadata>) {
    let vt = vtable(<F as AsVTable>::vtable_fields(prefix));
    let metadata = <F as Metadatatize>::metadata(prefix).collect();
    (vt, metadata)
}

impl PortDesc {
    /// Derives the descriptor for a [`Frame`] type, `Table × Snapshot × One`,
    /// telemetered.
    pub fn of<F: Frame>() -> Self {
        Self {
            id: PortId::Component(F::FRAME_ID),
            name: F::NAME,
            max_size: F::MAX_SIZE,
            schema: PortSchema::Table {
                vtable: F::as_vtable(),
                // Coerce the fn item to a plain fn pointer first, erasing `F`
                // so no `F: 'static` bound is needed, then box it as the
                // type-erased `Arc<dyn Fn>`.
                announce: Arc::new(
                    announce_of::<F> as fn(&str) -> (VTable, Vec<ComponentMetadata>),
                ),
            },
            delivery: Delivery::Snapshot,
            fan_in: FanIn::One,
            telemetered: true,
            conn: PortConn::Edge,
        }
    }

    /// Derives the descriptor for a [`NamedMsg`] type, `Postcard × Log × Many`,
    /// telemetered. The name is the stable [`NamedMsg::NAME`] token, never
    /// the Rust type path.
    pub fn msg<M: NamedMsg>() -> Self {
        Self {
            id: PortId::Packet(M::ID),
            name: M::NAME,
            max_size: MAX_MSG_BYTES,
            schema: PortSchema::Postcard,
            delivery: Delivery::Log,
            fan_in: FanIn::Many,
            telemetered: true,
            conn: PortConn::Edge,
        }
    }

    /// [`msg`](Self::msg) with an explicit name override. `name` becomes the
    /// display, config, and registry token while the edge key stays `M::ID`, so
    /// a channel keyed `"<instance>.commands"` keeps its key and
    /// `msg="<M::NAME>"` edges still resolve by packet id.
    pub fn msg_named<M: NamedMsg>(name: &'static str) -> Self {
        Self {
            name,
            ..Self::msg::<M>()
        }
    }

    /// [`msg`](Self::msg) at the value level, minting a Postcard port from a
    /// runtime `(name, id)` pair for a system whose message ports are chosen
    /// by configuration and so have no static `M`. `name` must be the stable
    /// [`NamedMsg::NAME`] token the configuration resolved, so `msg="<name>"`
    /// edges resolve identically.
    pub fn msg_dynamic(name: &'static str, id: PacketId) -> Self {
        Self {
            id: PortId::Packet(id),
            name,
            max_size: MAX_MSG_BYTES,
            schema: PortSchema::Postcard,
            delivery: Delivery::Log,
            fan_in: FanIn::Many,
            telemetered: true,
            conn: PortConn::Edge,
        }
    }

    /// Opts this output out of the downlink tap. The port stays a first-class
    /// registered buffer, visible by key, but is never downlinked.
    pub fn untelemetered(mut self) -> Self {
        self.telemetered = false;
        self
    }

    /// Overrides the connection axis. Only the slot runner's and the
    /// coordinator's own bundle derivations use this; user ports stay
    /// [`PortConn::Edge`].
    pub fn with_conn(mut self, c: PortConn) -> Self {
        self.conn = c;
        self
    }

    /// Overrides the input fan-in rule.
    pub fn with_fan_in(mut self, f: FanIn) -> Self {
        self.fan_in = f;
        self
    }

    /// Overrides the delivery semantics, for example an every-record
    /// `Table × Log` frame log.
    pub fn with_delivery(mut self, d: Delivery) -> Self {
        self.delivery = d;
        self
    }

    /// The frame-relative vtable of a Table port, or `None` for a Postcard
    /// port.
    pub fn vtable(&self) -> Option<&VTable> {
        match &self.schema {
            PortSchema::Table { vtable, .. } => Some(vtable),
            PortSchema::Postcard => None,
        }
    }

    /// The announce factory of a Table port, or `None` for a Postcard port.
    pub fn announce(&self) -> Option<&AnnounceFn> {
        match &self.schema {
            PortSchema::Table { announce, .. } => Some(announce),
            PortSchema::Postcard => None,
        }
    }
}

/// How the coordinator drives a system. Carried on the descriptor as
/// metadata; the trait a system implements
/// ([`CyclicSystem`](crate::CyclicSystem) or
/// [`AsyncSystem`](crate::AsyncSystem)) is the real distinction.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub enum SystemKind {
    /// Driven by the coordinator, `execute` once per cycle.
    Cyclic,
    /// Self-driven; the system owns its own `run` loop.
    Async,
}

/// The complete static shape of one system, read by the coordinator to size
/// rings, validate wiring, and allocate buffers before the system is
/// constructed.
///
/// It carries the system's name, its driving [`SystemKind`], a [`PortDesc`]
/// for every input and output, and the [`Capability`] set it needs from the
/// host at bind time.
#[derive(Clone, Debug)]
pub struct SystemDescriptor {
    pub name: &'static str,
    pub kind: SystemKind,
    pub inputs: Vec<PortDesc>,
    pub outputs: Vec<PortDesc>,
    pub capabilities: Vec<Capability>,
}

/// Collects a vtable's components into the `(ty, shape)` map a compatibility
/// check compares.
fn realize_set(vtable: &VTable) -> HashMap<ComponentId, (PrimType, Vec<usize>)> {
    let mut set = HashMap::new();
    // Realizing with no table surfaces every component, including dynamic
    // member templates, with its ty and shape. Malformed fields are skipped;
    // a real frame's vtable never produces them.
    for field in vtable.realize_fields(None).flatten() {
        set.insert(field.component_id, (field.ty, field.shape.to_vec()));
    }
    set
}

/// Whether a `producer` output satisfies a `consumer` input.
///
/// The edge keys must match, and since Table and Postcard ids live in
/// disjoint spaces a cross-schema pair never can. Delivery must also match
/// across an edge; a Log consumer of a Snapshot ring would silently see
/// coalesced gaps, and a Snapshot consumer of a Log ring would silently
/// discard records. For a Table pair the consumer's component set must be a
/// subset of the producer's with matching type and shape, so a producer may
/// emit extra fields a consumer ignores. A Postcard pair needs nothing beyond
/// the id equality already checked, since its records are opaque postcard
/// blobs with no component structure.
pub fn compatible(producer: &PortDesc, consumer: &PortDesc) -> bool {
    if producer.id != consumer.id || producer.delivery != consumer.delivery {
        return false;
    }
    match (&producer.schema, &consumer.schema) {
        (PortSchema::Table { vtable: pv, .. }, PortSchema::Table { vtable: cv, .. }) => {
            let prod = realize_set(pv);
            let cons = realize_set(cv);
            cons.iter().all(|(id, want)| match prod.get(id) {
                Some(have) => have == want,
                None => false,
            })
        }
        (PortSchema::Postcard, PortSchema::Postcard) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use metor_proto::types::{Msg, Timestamp};
    use metor_proto_wkt::{
        AlarmAck, AlarmCleared, AlarmDef, AlarmRaised, SequenceChannelEvent, SequenceCommand,
        SequenceRegistry,
    };
    use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

    use super::*;
    use crate::message::NamedMsg;

    #[derive(crate::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Default)]
    #[repr(C)]
    #[metor_fsw(name = "axis_probe")]
    struct AxisProbe {
        #[metor_fsw(timestamp)]
        timestamp: Timestamp,
        value: f64,
    }

    /// The constructors set the documented axis defaults, and each `id` sits
    /// in its schema's value space.
    #[test]
    fn constructor_axis_defaults() {
        let f = PortDesc::of::<AxisProbe>();
        assert_eq!(f.id, PortId::Component(AxisProbe::FRAME_ID));
        assert_eq!(f.name, "axis_probe");
        assert!(matches!(f.schema, PortSchema::Table { .. }));
        assert_eq!(f.delivery, Delivery::Snapshot);
        assert_eq!(f.fan_in, FanIn::One);
        assert!(f.telemetered);

        let m = PortDesc::msg::<SequenceCommand>();
        assert_eq!(m.id, PortId::Packet(SequenceCommand::ID));
        assert_eq!(m.name, "SequenceCommand");
        assert!(matches!(m.schema, PortSchema::Postcard));
        assert_eq!(m.delivery, Delivery::Log);
        assert_eq!(m.fan_in, FanIn::Many);
        assert!(m.telemetered);
    }

    /// Each modifier overrides exactly one axis.
    #[test]
    fn modifiers_override_one_axis() {
        let d = PortDesc::of::<AxisProbe>().untelemetered();
        assert!(!d.telemetered);
        assert_eq!(d.delivery, Delivery::Snapshot);

        let d = PortDesc::of::<AxisProbe>().with_fan_in(FanIn::Many);
        assert_eq!(d.fan_in, FanIn::Many);
        assert_eq!(d.delivery, Delivery::Snapshot);

        // The fourth axis combination, an every-record frame log.
        let d = PortDesc::of::<AxisProbe>().with_delivery(Delivery::Log);
        assert_eq!(d.delivery, Delivery::Log);
        assert!(matches!(d.schema, PortSchema::Table { .. }));
    }

    /// The checked accessors return `None` off-schema rather than panicking.
    #[test]
    fn checked_accessors_none_off_schema() {
        let m = PortDesc::msg::<SequenceCommand>();
        assert!(m.vtable().is_none());
        assert!(m.announce().is_none());
        assert!(m.id.component().is_none());
        assert!(m.id.packet().is_some());

        let f = PortDesc::of::<AxisProbe>();
        assert!(f.vtable().is_some());
        assert!(f.announce().is_some());
        assert!(f.id.component().is_some());
        assert!(f.id.packet().is_none());
    }

    /// Table pairs follow the subset rule, Postcard pairs match by id, and
    /// neither a cross-schema pair nor a delivery mismatch ever matches.
    #[test]
    fn compatible_matrix() {
        let f = PortDesc::of::<AxisProbe>();
        let m = PortDesc::msg::<SequenceCommand>();

        assert!(compatible(&f, &PortDesc::of::<AxisProbe>()));
        assert!(compatible(&m, &PortDesc::msg::<SequenceCommand>()));
        // Distinct Postcard ids never match.
        assert!(!compatible(&m, &PortDesc::msg::<SequenceRegistry>()));
        // Cross-schema never matches; the disjoint id spaces already
        // guarantee it.
        assert!(!compatible(&f, &m));
        assert!(!compatible(&m, &f));
        // A delivery mismatch is incompatible even with identical id and
        // schema.
        let log_f = PortDesc::of::<AxisProbe>().with_delivery(Delivery::Log);
        assert!(!compatible(&f, &log_f));
        assert!(!compatible(&log_f, &f));
        assert!(compatible(&log_f, &log_f.clone()));
    }

    /// The well-known message name tokens are frozen; mission configs and
    /// registry keys depend on them.
    #[test]
    fn wkt_named_msg_tokens_frozen() {
        assert_eq!(<SequenceCommand as NamedMsg>::NAME, "SequenceCommand");
        assert_eq!(<SequenceRegistry as NamedMsg>::NAME, "SequenceRegistry");
        assert_eq!(
            <SequenceChannelEvent as NamedMsg>::NAME,
            "SequenceChannelEvent"
        );
        assert_eq!(<AlarmDef as NamedMsg>::NAME, "AlarmDef");
        assert_eq!(<AlarmRaised as NamedMsg>::NAME, "AlarmRaised");
        assert_eq!(<AlarmCleared as NamedMsg>::NAME, "AlarmCleared");
        assert_eq!(<AlarmAck as NamedMsg>::NAME, "AlarmAck");
    }
}
