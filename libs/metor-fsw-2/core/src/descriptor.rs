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
//! Descriptors are plain serializable data, so the same [`SystemDescriptor`]
//! a statically linked system derives is what a pack manifest carries across
//! the load boundary and what a describe worker ships between processes. A
//! Table port holds its frame-relative (unprefixed) vtable and metadata; the
//! instance-prefixed announce form is re-derived on demand by
//! [`PortDesc::announce`], the one prefixing path for static and loaded ports
//! alike.
//!
//! [`compatible`] is the one check that decides whether a producer output may
//! feed a consumer input.

use std::collections::HashMap;

use metor_component::Metadatatize;
use metor_proto::types::{ComponentId, PacketId, PrimType};
use metor_proto::vtable::{Op, VTable};
use metor_proto_wkt::ComponentMetadata;
use postcard_schema::schema::owned::OwnedNamedType;
use serde::{Deserialize, Serialize};

use crate::frame::Frame;
use crate::message::{MAX_MSG_BYTES, NamedMsg};

/// A frequency in cycles per second, the unit of the coordinator's cycle
/// rate.
pub type Hz = f64;

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
}

/// What one record is and how it is described (the schema axis).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum PortSchema {
    /// A component-frame table of `#[repr(C)]` bytes described by a [`VTable`].
    Table {
        /// The edge key, `F::FRAME_ID`.
        frame_id: ComponentId,
        /// The frame-relative (unprefixed) vtable that wiring compatibility
        /// compares. The prefixed form is produced on demand by
        /// [`PortDesc::announce`].
        vtable: VTable,
        /// The unprefixed component metadata, parallel to `vtable`, from
        /// which [`PortDesc::announce`] derives the instance-prefixed ids.
        metadata: Vec<ComponentMetadata>,
    },
    /// A self-describing postcard record: the 2-byte [`PacketId`] keys the
    /// wire, and `schema` describes the payload so the downlink can announce
    /// it and ground tools decode records generically.
    Postcard {
        /// The edge key, `M::ID`.
        id: PacketId,
        /// The payload's postcard schema, announced to the db as
        /// [`MsgMetadata`](metor_proto_wkt::MsgMetadata). `None` only for
        /// value-minted dynamic ports, which have no static type.
        schema: Option<Box<OwnedNamedType>>,
    },
}

/// What a consumer is expected to read off the channel (the delivery axis).
///
/// Delivery drives ring depth, telemetry coalescing, and cycle-detection
/// membership.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Delivery {
    /// A state sample. Readers coalesce to the newest record.
    Snapshot,
    /// An event or command log. Every record is read, in order, never
    /// coalesced.
    Log,
}

/// How many producers may wire into an input (the fan-in axis). Outputs
/// ignore it; fan-out is always unbounded.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
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
/// other than [`Edge`](PortConn::Edge). This axis is host-only shape: it is
/// skipped by serialization, so a descriptor decoded from a manifest always
/// carries the default `Edge`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum PortConn {
    /// Wired by ordinary edges under the fan-in rules. The default.
    #[default]
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
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Capability {
    /// A read view over every telemetered output in the graph
    /// ([`AllOutputs`](crate::AllOutputs)). Counts one reader slot on every
    /// buffer at sizing time. Only a host-built system can hold it.
    ReceiveAll,
}

/// The ports and bind-time capabilities contributed by a bundle.
#[derive(Clone, Debug, Default)]
pub struct Declarations {
    pub ports: Vec<PortDesc>,
    pub capabilities: Vec<Capability>,
}

impl Declarations {
    /// Adds either a port or a capability declaration.
    pub fn push(&mut self, declaration: impl Into<Self>) {
        let mut declaration = declaration.into();
        self.ports.append(&mut declaration.ports);
        self.capabilities.append(&mut declaration.capabilities);
    }
}

impl From<PortDesc> for Declarations {
    fn from(port: PortDesc) -> Self {
        Self {
            ports: vec![port],
            capabilities: Vec::new(),
        }
    }
}

impl From<Vec<PortDesc>> for Declarations {
    fn from(ports: Vec<PortDesc>) -> Self {
        Self {
            ports,
            capabilities: Vec::new(),
        }
    }
}

impl From<Capability> for Declarations {
    fn from(capability: Capability) -> Self {
        Self {
            ports: Vec::new(),
            capabilities: vec![capability],
        }
    }
}

/// The static shape of one port, everything the coordinator needs to size a
/// ring and check an edge before the owning system is constructed.
///
/// The same struct describes an output (a produced record stream) and an
/// input (a required shape); the direction is which list of a
/// [`SystemDescriptor`] it sits in. `fan_in` has no effect on outputs, and
/// `telemetered` has no effect on inputs.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PortDesc {
    /// Display, config-token, and registry-key name (`F::NAME` or `M::NAME`).
    /// The coordinator joins it with an instance name to form the qualified
    /// registry key `ComponentId::new("<instance>.<name>")` without needing a
    /// static type.
    pub name: String,
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
    /// Who provides the other end. Host-only shape, so it never rides the
    /// wire; a decoded descriptor's ports are always edge-connected.
    #[serde(skip, default)]
    pub conn: PortConn,
}

/// Re-derives `F`'s vtable and metadata under the dotted `prefix` (the
/// instance name), so the leaves roll the same ids as
/// `ComponentId::new("<prefix>.<frame>.<field>")`, the static-path oracle
/// the announce-equivalence tests compare [`PortDesc::announce`] against.
#[cfg(test)]
pub(crate) fn announce_of<F: Frame>(prefix: &str) -> (VTable, Vec<ComponentMetadata>) {
    use metor_component::AsVTable;
    let vt = metor_proto::vtable::builder::vtable(<F as AsVTable>::vtable_fields(prefix));
    let metadata = <F as Metadatatize>::metadata(prefix).collect();
    (vt, metadata)
}

impl PortDesc {
    /// Derives the descriptor for a [`Frame`] type, `Table × Snapshot × One`,
    /// telemetered.
    pub fn of<F: Frame>() -> Self {
        Self {
            name: F::NAME.into(),
            max_size: F::MAX_SIZE,
            schema: PortSchema::Table {
                frame_id: F::FRAME_ID,
                vtable: F::as_vtable(),
                metadata: <F as Metadatatize>::metadata("").collect(),
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
            name: M::NAME.into(),
            max_size: MAX_MSG_BYTES,
            schema: PortSchema::Postcard {
                id: M::ID,
                schema: Some(Box::new(OwnedNamedType::from(
                    <M as postcard_schema::Schema>::SCHEMA,
                ))),
            },
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
    pub fn msg_named<M: NamedMsg>(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::msg::<M>()
        }
    }

    /// [`msg`](Self::msg) at the value level, minting a Postcard port from a
    /// runtime `(name, id)` pair for a system whose message ports are chosen
    /// by configuration and so have no static `M`. `name` must be the stable
    /// [`NamedMsg::NAME`] token the configuration resolved, so `msg="<name>"`
    /// edges resolve identically.
    pub fn msg_dynamic(name: impl Into<String>, id: PacketId) -> Self {
        Self {
            name: name.into(),
            max_size: MAX_MSG_BYTES,
            schema: PortSchema::Postcard { id, schema: None },
            delivery: Delivery::Log,
            fan_in: FanIn::Many,
            telemetered: true,
            conn: PortConn::Edge,
        }
    }

    /// The edge key, derived from the schema arm.
    pub fn id(&self) -> PortId {
        match &self.schema {
            PortSchema::Table { frame_id, .. } => PortId::Component(*frame_id),
            PortSchema::Postcard { id, .. } => PortId::Packet(*id),
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

    /// Overrides the delivery semantics, for example an every-record
    /// `Table × Log` frame log.
    pub fn with_delivery(mut self, d: Delivery) -> Self {
        self.delivery = d;
        self
    }

    /// The announce form of a Table port under the `instance` name: the
    /// vtable and metadata with every leaf id nested under the instance
    /// prefix, the ids telemetry keys the port's components by. `None` for a
    /// Postcard port, whose records are self-describing. An empty instance is
    /// the unprefixed identity.
    pub fn announce(&self, instance: &str) -> Option<(VTable, Vec<ComponentMetadata>)> {
        match &self.schema {
            PortSchema::Table {
                vtable, metadata, ..
            } => {
                if instance.is_empty() {
                    return Some((vtable.clone(), metadata.clone()));
                }
                let vt = prefix_vtable(vtable, metadata, instance);
                // Not `with_prefix`, which resets the metadata map and would
                // drop element names and enum variants on the way.
                let meta = metadata
                    .iter()
                    .map(|m| {
                        let name = format!("{instance}.{}", m.name);
                        ComponentMetadata {
                            component_id: ComponentId::new(&name),
                            name,
                            metadata: m.metadata.clone(),
                        }
                    })
                    .collect();
                Some((vt, meta))
            }
            PortSchema::Postcard { .. } => None,
        }
    }
}

/// Rewrite a Table port's unprefixed vtable into its instance-prefixed
/// announce form.
///
/// The port carries the frame-relative vtable plus per-component metadata,
/// and the prefixed ids are reconstructed from that metadata with no static
/// frame type, the same path whether the port was derived in-process or
/// decoded from a pack manifest.
///
/// Each leaf component id is baked as a standalone 8-byte `Op::Data` blob, so
/// this builds an unprefixed-to-prefixed id map from the metadata (a leaf's
/// unprefixed id is `ComponentId::new(meta.name)`, and its prefixed id hashes
/// `"<prefix>.<meta.name>"`, which is exactly what `with_prefix` produces) and
/// rewrites every 8-byte `Op::Data` whose value is a known leaf id. The frame
/// tag id is never prefixed and the schema type/dim blobs are absent from the
/// map, so both are left untouched.
///
/// A dynamic terminal (`Op::List`/`Op::Map`) bakes no leaf id; its members'
/// ids compose at realize time from the terminal's dotted name string. A
/// top-level terminal's name carries the full path from the frame root
/// (`alarm_defs.limits`, say), so the instance prefix belongs on it; a nested
/// terminal inside a member template keeps its element-relative name. Each
/// top-level terminal's name blob is repointed at a prefixed copy appended to
/// the data buffer, which shifts no existing offset. The result realizes
/// identically to what a static frame type bakes under the same prefix; the
/// byte layout of the appended strings differs, which no consumer observes.
pub(crate) fn prefix_vtable(
    vtable: &VTable,
    metadata: &[ComponentMetadata],
    prefix: &str,
) -> VTable {
    let mut vt = vtable.clone();
    if prefix.is_empty() {
        // An empty prefix is the unprefixed identity, since `PathHasher` skips
        // empty segments. Every announce caller supplies a real instance name,
        // but stay total.
        return vt;
    }
    // Unprefixed leaf id to prefixed leaf id, from the carried metadata.
    let map: HashMap<u64, u64> = metadata
        .iter()
        .map(|m| {
            let unprefixed = ComponentId::new(&m.name).0;
            let prefixed = ComponentId::new(&format!("{prefix}.{}", m.name)).0;
            (unprefixed, prefixed)
        })
        .collect();
    // Collect the rewrites first, since the `ops` borrow and the `data` read
    // overlap on `vt`, then apply them to a fresh data buffer.
    let data = vt.data.as_slice();
    let mut rewrites: Vec<(usize, u64)> = Vec::new();
    for op in vt.ops.iter() {
        if let Op::Data { offset, len } = op
            && *len as usize == core::mem::size_of::<u64>()
            && let Some(slot) = data.get(offset.to_index()..offset.to_index() + 8)
        {
            let val = u64::from_le_bytes(slot.try_into().expect("8-byte slice"));
            if let Some(&prefixed) = map.get(&val) {
                rewrites.push((offset.to_index(), prefixed));
            }
        }
    }
    if !rewrites.is_empty() {
        let mut new_data = data.to_vec();
        for (off, prefixed) in rewrites {
            new_data[off..off + 8].copy_from_slice(&prefixed.to_le_bytes());
        }
        vt.data = new_data;
    }
    prefix_dynamic_names(&mut vt, prefix);
    vt
}

/// Repoint every top-level dynamic terminal's name blob at an
/// instance-prefixed copy appended to the data buffer.
///
/// A field is top-level iff no `List`/`Map` op claims its index as a member
/// template (the same rule realization's `is_template_field` applies). Each
/// such field's op chain is walked through the metadata continuations to its
/// terminal; a `List`/`Map` terminal names the dotted path to prefix. The
/// name's `Op::Data` is mutated to point at the appended bytes rather than
/// being rewritten in place, so any other op still reading the old range is
/// unaffected; ops are the aliasing unit, and a name op reached from two
/// top-level fields (rewritten once) wants the same prefix from both.
fn prefix_dynamic_names(vt: &mut VTable, prefix: &str) {
    let claimed: Vec<(usize, usize)> = vt
        .ops
        .iter()
        .filter_map(|op| match op {
            Op::List { members, .. } | Op::Map { members, .. } => Some((
                members.start as usize,
                members.start as usize + members.count as usize,
            )),
            _ => None,
        })
        .collect();
    let is_claimed = |i: usize| claimed.iter().any(|&(s, e)| i >= s && i < e);

    // The name-op indices of the top-level dynamic terminals, deduplicated.
    let mut name_ops: Vec<usize> = Vec::new();
    for (i, field) in vt.fields.iter().enumerate() {
        if is_claimed(i) {
            continue;
        }
        let mut cur = field.arg;
        // A well-formed chain never revisits an op; bound the walk anyway.
        for _ in 0..vt.ops.len() {
            match vt.get_op(cur) {
                Ok(Op::Schema { arg, .. })
                | Ok(Op::Timestamp { arg, .. })
                | Ok(Op::Frame { arg, .. })
                | Ok(Op::Ext { arg, .. }) => cur = *arg,
                Ok(Op::List { name, .. }) | Ok(Op::Map { name, .. }) => {
                    let idx = name.to_index();
                    if !name_ops.contains(&idx) {
                        name_ops.push(idx);
                    }
                    break;
                }
                _ => break,
            }
        }
    }

    for idx in name_ops {
        let Some(Op::Data { offset, len }) = vt.ops.as_slice().get(idx) else {
            continue;
        };
        let (off, len) = (offset.to_index(), *len as usize);
        let Some(bytes) = vt.data.as_slice().get(off..off + len) else {
            continue;
        };
        let Ok(name) = core::str::from_utf8(bytes) else {
            continue;
        };
        let prefixed = format!("{prefix}.{name}");
        let new_offset = vt.data.len() as u32;
        vt.data.extend_from_slice(prefixed.as_bytes());
        vt.ops[idx] = Op::Data {
            offset: new_offset.into(),
            len: prefixed.len() as u32,
        };
    }
}

/// How the coordinator drives a system. Carried on the descriptor as
/// metadata; the trait a system implements
/// ([`CyclicSystem`](crate::CyclicSystem) or the host crate's `AsyncSystem`)
/// is the real distinction.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
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
/// host at bind time. Plain serializable data: the same struct rides the pack
/// manifest across the load boundary.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SystemDescriptor {
    pub name: String,
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
    if producer.id() != consumer.id() || producer.delivery != consumer.delivery {
        return false;
    }
    match (&producer.schema, &consumer.schema) {
        (PortSchema::Table { vtable: pv, .. }, PortSchema::Table { vtable: cv, .. }) => {
            let prod = realize_set(pv);
            let cons = realize_set(cv);
            cons.iter().all(|(id, want)| prod.get(id) == Some(want))
        }
        (PortSchema::Postcard { .. }, PortSchema::Postcard { .. }) => true,
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

    /// The constructors set the documented axis defaults, and each `id()` sits
    /// in its schema's value space.
    #[test]
    fn constructor_axis_defaults() {
        let f = PortDesc::of::<AxisProbe>();
        assert_eq!(f.id(), PortId::Component(AxisProbe::FRAME_ID));
        assert_eq!(f.name, "axis_probe");
        assert!(matches!(f.schema, PortSchema::Table { .. }));
        assert_eq!(f.delivery, Delivery::Snapshot);
        assert_eq!(f.fan_in, FanIn::One);
        assert!(f.telemetered);

        let m = PortDesc::msg::<SequenceCommand>();
        assert_eq!(m.id(), PortId::Packet(SequenceCommand::ID));
        assert_eq!(m.name, "SequenceCommand");
        assert!(matches!(m.schema, PortSchema::Postcard { .. }));
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

        // The fourth axis combination, an every-record frame log.
        let d = PortDesc::of::<AxisProbe>().with_delivery(Delivery::Log);
        assert_eq!(d.delivery, Delivery::Log);
        assert!(matches!(d.schema, PortSchema::Table { .. }));
    }

    /// Schema and id variants agree for both port families.
    #[test]
    fn schema_and_id_variants_agree() {
        let m = PortDesc::msg::<SequenceCommand>();
        assert!(matches!(m.schema, PortSchema::Postcard { .. }));
        assert!(m.announce("inst").is_none());
        assert!(m.id().component().is_none());
        assert!(matches!(m.id(), PortId::Packet(_)));

        let f = PortDesc::of::<AxisProbe>();
        assert!(matches!(f.schema, PortSchema::Table { .. }));
        assert!(f.announce("inst").is_some());
        assert!(f.id().component().is_some());
        assert!(matches!(f.id(), PortId::Component(_)));
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
        assert!(compatible(&log_f, &log_f));
    }

    /// A descriptor round-trips through postcard, with the host-only `conn`
    /// axis folding back to `Edge` and the announce form surviving the trip.
    #[test]
    fn port_desc_serde_round_trip() {
        let d = PortDesc::of::<AxisProbe>()
            .untelemetered()
            .with_conn(PortConn::Host);
        let bytes = postcard::to_allocvec(&d).expect("encodes");
        let back: PortDesc = postcard::from_bytes(&bytes).expect("decodes");
        assert_eq!(back.id(), d.id());
        assert_eq!(back.name, d.name);
        assert_eq!(back.max_size, d.max_size);
        assert_eq!(back.delivery, d.delivery);
        assert_eq!(back.fan_in, d.fan_in);
        assert!(!back.telemetered, "the opt-out survived the wire");
        assert_eq!(back.conn, PortConn::Edge, "conn is host-only shape");
        assert!(compatible(&back, &d) && compatible(&d, &back));

        let (vt, meta) = back.announce("inst").expect("table port announces");
        let (evt, emeta) = d.announce("inst").expect("table port announces");
        assert_eq!(meta, emeta);
        assert_eq!(
            postcard::to_allocvec(&vt).unwrap(),
            postcard::to_allocvec(&evt).unwrap(),
            "announce is derived from carried data, so the round-trip is exact"
        );
    }

    /// The well-known message name tokens are frozen; target configs and
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
