//! Static self-description a coordinator reads before any port exists (system.md
//! §5): the per-port [`PortDesc`], the [`SystemDescriptor`] bundle, and the
//! producer/consumer [`compatible`] check.
//!
//! One port concept, four orthogonal axes (`docs/design-port-unification.md`):
//!
//! ```text
//! schema     Table(VTable) | Postcard(PacketId)      what a record is
//! delivery   Snapshot | Log                          what a consumer reads
//! fan-in     One | Many                              how many producers an input takes
//! on-lap     Stop | Resync                           what a lap means for the reader
//! ```
//!
//! "Frame port" and "message port" are two *configurations* of this one concept:
//! [`PortDesc::of`] mints `Table × Snapshot × One × Stop`, [`PortDesc::msg`] mints
//! `Postcard × Log × Many × Resync`. Everything here is derived from static metadata
//! (`F::FRAME_ID`, `F::as_vtable()`, `M::ID`), so a system can be sized, allocated,
//! and wiring-validated without constructing it.

use std::collections::HashMap;
use std::sync::Arc;

use metor_fsw::{AsVTable, Metadatatize};
use metor_proto::types::{ComponentId, PacketId, PrimType};
use metor_proto::vtable::VTable;
use metor_proto::vtable::builder::vtable;
use metor_proto_wkt::ComponentMetadata;

use crate::frame::Frame;
use crate::message::{MAX_MSG_BYTES, NamedMsg};

/// A unit measured in Hertz.
pub type Hz = f64;

/// The type-erased prefix factory stored on a Table port's schema: given an instance
/// name it returns the prefixed announce vtable + component metadata. An [`Arc`] boxed
/// closure (not a bare `fn`) so a dlopen'd port — which has no static `F` — can
/// carry a closure capturing its metadata-derived prefix rewrite.
pub type AnnounceFn = Arc<dyn Fn(&str) -> (VTable, Vec<ComponentMetadata>) + Send + Sync>;

/// The edge key of a port. Two disjoint value spaces (a Table port keys on the
/// 8-byte frame [`ComponentId`], a Postcard port on the 2-byte [`PacketId`]), so a
/// mismatched pair can never accidentally satisfy an edge.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum PortId {
    /// `F::FRAME_ID` — a Table port's key.
    Component(ComponentId),
    /// `M::ID` — a Postcard port's key.
    Packet(PacketId),
}

impl PortId {
    /// The frame [`ComponentId`] of a Table port, or `None` on a Postcard port.
    /// Checked (S5): internal Table-only call sites `.expect("table port")` with the
    /// invariant stated; user-reachable paths surface an error instead of panicking.
    pub fn component(self) -> Option<ComponentId> {
        match self {
            PortId::Component(c) => Some(c),
            PortId::Packet(_) => None,
        }
    }

    /// The [`PacketId`] of a Postcard port, or `None` on a Table port.
    pub fn packet(self) -> Option<PacketId> {
        match self {
            PortId::Component(_) => None,
            PortId::Packet(p) => Some(p),
        }
    }
}

/// Axis 1 — what one record is and how it is described.
#[derive(Clone)]
pub enum PortSchema {
    /// A component-frame table: `#[repr(C)]` bytes described by a vtable. Carries the
    /// wiring-compatibility vtable and the telemetry announce factory.
    Table {
        /// `F::as_vtable()` — the **frame-relative** (unprefixed) vtable the wiring
        /// compatibility check uses; the prefixed vtable is produced on demand by the
        /// announce factory.
        vtable: VTable,
        /// Prefix factory (telemetry.md §6): given an instance name, it re-derives the
        /// **prefixed** announce vtable + component metadata. Type-erased ([`Arc`]
        /// boxed closure) so a dlopen'd port, which has no static `F`, can carry a
        /// closure capturing its metadata-derived prefix rewrite instead.
        announce: AnnounceFn,
    },
    /// A self-describing `(PacketId, postcard)` record. The 2-byte id *is* the
    /// schema; no vtable, no announce.
    Postcard,
}

/// Axis 2 — what a consumer is expected to read off the channel. Drives ring depth,
/// telemetry coalescing, cycle-detection membership, and whether `delayed` is
/// meaningful.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub enum Delivery {
    /// A state sample: readers coalesce to the newest record (latest-wins).
    Snapshot,
    /// An event/command log: every record matters, in order, never coalesced.
    Log,
}

/// Axis 3 — how many producers may wire into this *input*. (Ignored on outputs;
/// fan-out is always unbounded.)
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub enum FanIn {
    /// Exactly one edge, required (the frame-input rule).
    One,
    /// Zero, one, or many edges (the message-input rule). Requires
    /// [`Delivery::Log`] — latest-wins across producers is ill-defined
    /// (`WireError::SnapshotFanIn`).
    Many,
}

/// Axis 4 — what a writer lap means for this *input*'s reader. (Ignored on outputs.)
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub enum OnLap {
    /// A lap is a hard fault: the framework telemeters it and permanently stops
    /// the consumer (the cyclic frame doctrine).
    Stop,
    /// A lap means skip to the live edge and continue (best-effort; the message
    /// and async-copy-in behavior).
    Resync,
}

/// Who provides the other end of this port (`docs/design-command-slots.md` §2.1) —
/// a fifth axis beside schema × delivery × fan-in × on-lap.
///
/// Not carried across the dl ABI: a dlopen'd system's ports are always
/// edge-connected (`Host`/`SelfTap` are host-runner constructs — the slot runner's
/// control/status/events ports, the coordinator's own bundle).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PortConn {
    /// Edge-connected (the default): wired by `connect` / KDL edges under the
    /// fan-in axis rules.
    Edge,
    /// Host-connected: the system's *runner* (the slot runner, or the coordinator
    /// itself) holds the port's counterpart — the writer of an input's dedicated
    /// ring, or the writer of an output ring the bind walk would otherwise hand to
    /// the system. A Host **output** is still ring-allocated and registry-tapped
    /// like any output (and may be consumed over ordinary edges); a Host **input**
    /// gets a dedicated ring, is exempt from `UnconnectedInput`, and rejects edges
    /// (`WireError::HostPort`).
    Host,
    /// A declared reader over one of this system's *own* outputs, named by
    /// [`PortId`]. Allocates no ring; adds +1 to that output's fan-out; the view
    /// goes to the runner. Inputs only; rejects edges (`WireError::HostPort`).
    SelfTap(PortId),
}

/// A non-port resource a system needs from the host at bind time
/// (`docs/design-port-unification.md` §2.5). Unlike a port it reserves no ring and
/// connects no edge — it is granted, not wired.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub enum Capability {
    /// A read view over every telemetered output in the graph
    /// ([`AllOutputs`](crate::AllOutputs)). Counts one reader slot on every buffer
    /// at sizing time. Host-only (a dlopen'd system cannot hold it).
    ReceiveAll,
}

/// What one bundle field contributes to the descriptor: an ordinary wired port, or
/// a bind-time [`Capability`]. The `SystemInput`/`SystemOutput` derives collect one
/// per field (`<Field>::decl()`), so a single type-blind walk covers both — the
/// bind cursor skips capability decls (they consume no ring).
#[derive(Clone, Debug)]
pub enum PortDecl {
    Port(PortDesc),
    Capability(Capability),
}

impl PortDecl {
    /// The derive-facing twin of [`PortDesc::untelemetered`]: applies to a
    /// [`Port`](PortDecl::Port) decl; a capability has no axes, so it passes
    /// through untouched.
    pub fn untelemetered(self) -> Self {
        match self {
            PortDecl::Port(p) => PortDecl::Port(p.untelemetered()),
            cap => cap,
        }
    }

    /// The derive-facing twin of [`PortDesc::with_on_lap`] (same port-only rule).
    pub fn with_on_lap(self, p: OnLap) -> Self {
        match self {
            PortDecl::Port(d) => PortDecl::Port(d.with_on_lap(p)),
            cap => cap,
        }
    }

    /// The port shape of a [`Port`](PortDecl::Port) decl, `None` for a capability.
    pub fn into_port(self) -> Option<PortDesc> {
        match self {
            PortDecl::Port(p) => Some(p),
            PortDecl::Capability(_) => None,
        }
    }
}

/// Split one direction's decls into its wired ports (order preserved — the bind
/// cursor contract) and its capabilities.
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

/// One port's static shape: its edge key, display name, worst-case size, and the
/// four behavior axes.
///
/// Used both for an output (a produced record stream) and an input (a required
/// shape) — the two are structurally identical; the direction is which list of a
/// [`SystemDescriptor`] it sits in. `fan_in`/`on_lap` are documented no-ops on
/// outputs; `telemetered` is a no-op on inputs.
#[derive(Clone)]
pub struct PortDesc {
    /// Edge key, derived from schema by the constructors:
    /// `Component(F::FRAME_ID)` for Table, `Packet(M::ID)` for Postcard.
    pub id: PortId,
    /// Display / KDL-token / registry-key name: `F::NAME` or `M::NAME` — kept so the
    /// coordinator can compute the instance-qualified registry key
    /// `ComponentId::new("<instance>.<name>")` without a static type.
    pub name: &'static str,
    /// Worst-case record bytes: `F::MAX_SIZE` or [`MAX_MSG_BYTES`]; size a ring via
    /// [`crate::capacity_for`].
    pub max_size: usize,
    /// Axis 1 — what a record is (Table bytes + vtable, or self-describing postcard).
    pub schema: PortSchema,
    /// Axis 2 — latest-wins snapshot vs every-record log.
    pub delivery: Delivery,
    /// Axis 3 — producer cardinality for an input (no-op on outputs).
    pub fan_in: FanIn,
    /// Axis 4 — lap policy for an input's reader (no-op on outputs).
    pub on_lap: OnLap,
    /// Output-side: whether the downlink / `AllOutputs` taps this port (A6). A plain
    /// field on *every* port — frames get the opt-out too.
    pub telemetered: bool,
    /// Axis 5 — who provides the other end ([`PortConn::Edge`] everywhere except the
    /// slot runner's and coordinator's own bundles).
    pub conn: PortConn,
}

impl std::fmt::Debug for PortDesc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `announce` (inside `PortSchema::Table`) is a boxed closure (no `Debug`); the
        // manual impl renders the schema without it.
        let mut s = f.debug_struct("PortDesc");
        s.field("id", &self.id)
            .field("name", &self.name)
            .field("max_size", &self.max_size);
        match &self.schema {
            PortSchema::Table { vtable, .. } => {
                s.field("schema", &"Table").field("vtable", vtable)
            }
            PortSchema::Postcard => s.field("schema", &"Postcard"),
        };
        s.field("delivery", &self.delivery)
            .field("fan_in", &self.fan_in)
            .field("on_lap", &self.on_lap)
            .field("telemetered", &self.telemetered)
            .field("conn", &self.conn);
        s.finish()
    }
}

/// The prefix factory stored on a Table port's schema: re-derive `F`'s vtable +
/// metadata under the dotted `prefix` (the instance name). A `&str` is a
/// [`ComponentPath`](metor_fsw::path::ComponentPath), so the leaves roll the same
/// ids as `ComponentId::new("<prefix>.<frame>.<field>")`.
fn announce_of<F: Frame>(prefix: &str) -> (VTable, Vec<ComponentMetadata>) {
    let vt = vtable(<F as AsVTable>::vtable_fields(prefix));
    let metadata = <F as Metadatatize>::metadata(prefix).collect();
    (vt, metadata)
}

impl PortDesc {
    /// Derives the descriptor for a frame type: `Table × Snapshot × One × Stop`,
    /// telemetered. Pure metadata — no instance needed.
    pub fn of<F: Frame>() -> Self {
        Self {
            id: PortId::Component(F::FRAME_ID),
            name: F::NAME,
            max_size: F::MAX_SIZE,
            schema: PortSchema::Table {
                vtable: F::as_vtable(),
                // Coerce the `F`-closing fn item to a plain fn pointer first (erasing
                // `F`, so no `F: 'static` bound is needed), then box it as the
                // type-erased `Arc<dyn Fn>`.
                announce: Arc::new(
                    announce_of::<F> as fn(&str) -> (VTable, Vec<ComponentMetadata>),
                ),
            },
            delivery: Delivery::Snapshot,
            fan_in: FanIn::One,
            on_lap: OnLap::Stop,
            telemetered: true,
            conn: PortConn::Edge,
        }
    }

    /// Derives the descriptor for a message type: `Postcard × Log × Many × Resync`,
    /// telemetered. The edge key is `M::ID`; the name is the explicit, stable
    /// [`NamedMsg::NAME`] token (A10 — never the Rust type path).
    pub fn msg<M: NamedMsg>() -> Self {
        Self {
            id: PortId::Packet(M::ID),
            name: M::NAME,
            max_size: MAX_MSG_BYTES,
            schema: PortSchema::Postcard,
            delivery: Delivery::Log,
            fan_in: FanIn::Many,
            on_lap: OnLap::Resync,
            telemetered: true,
            conn: PortConn::Edge,
        }
    }

    /// [`msg`](Self::msg) with an explicit channel-name override: `name` (not
    /// `M::NAME`) becomes the display/KDL/registry token, so a coordinator-minted
    /// channel keyed `"<instance>.sequences"` or `"<instance>.commands"` survives the
    /// move from hand-built registry entries to descriptor-driven allocation. The
    /// edge key stays `M::ID`, so `connect`/`msg="<M::NAME>"` edges still resolve to
    /// it by packet id.
    pub fn msg_named<M: NamedMsg>(name: &'static str) -> Self {
        Self {
            name,
            ..Self::msg::<M>()
        }
    }

    /// Opt this output out of the downlink / `AllOutputs` tap (A6): the port stays a
    /// first-class registered buffer (debugger/test visible by key) but is never
    /// downlinked. Command channels use this; frame outputs may too.
    pub fn untelemetered(mut self) -> Self {
        self.telemetered = false;
        self
    }

    /// Override the lap policy (axis 4).
    pub fn with_on_lap(mut self, p: OnLap) -> Self {
        self.on_lap = p;
        self
    }

    /// Override the port-connection axis (axis 5) — used only by the slot runner's
    /// and coordinator's own bundle derivations; user ports stay [`PortConn::Edge`].
    pub fn with_conn(mut self, c: PortConn) -> Self {
        self.conn = c;
        self
    }

    /// Override the input fan-in rule (axis 3).
    pub fn with_fan_in(mut self, f: FanIn) -> Self {
        self.fan_in = f;
        self
    }

    /// Override the delivery semantics (axis 2) — e.g. a future `Table × Log`
    /// every-record frame log.
    pub fn with_delivery(mut self, d: Delivery) -> Self {
        self.delivery = d;
        self
    }

    /// The frame-relative vtable of a Table port (wiring compatibility / telemetry
    /// announce), or `None` on a Postcard port (checked — S5).
    pub fn vtable(&self) -> Option<&VTable> {
        match &self.schema {
            PortSchema::Table { vtable, .. } => Some(vtable),
            PortSchema::Postcard => None,
        }
    }

    /// The prefix-announce factory of a Table port, or `None` on a Postcard port
    /// (checked — S5).
    pub fn announce(&self) -> Option<&AnnounceFn> {
        match &self.schema {
            PortSchema::Table { announce, .. } => Some(announce),
            PortSchema::Postcard => None,
        }
    }
}

/// How the coordinator drives a system. Carried on the descriptor as metadata; the
/// trait a user implements ([`CyclicSystem`](crate::CyclicSystem) vs
/// [`AsyncSystem`](crate::AsyncSystem)) is the real distinction.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub enum SystemKind {
    /// Coordinator-driven: `execute` once per cycle.
    Cyclic,
    /// Self-driven: the system owns its own `run` loop.
    Async,
}

/// A system's full self-description: its name, driving kind, the static shapes of
/// every input and output port, and the non-port [`Capability`] set it needs from
/// the host at bind time (§2.5 — e.g. the downlink's `ReceiveAll`).
#[derive(Clone, Debug)]
pub struct SystemDescriptor {
    pub name: &'static str,
    pub kind: SystemKind,
    pub inputs: Vec<PortDesc>,
    pub outputs: Vec<PortDesc>,
    pub capabilities: Vec<Capability>,
}

/// A `(component_id, ty, shape)` triple — the unit a compatibility check compares.
fn realize_set(vtable: &VTable) -> HashMap<ComponentId, (PrimType, Vec<usize>)> {
    let mut set = HashMap::new();
    // Registration mode (`table = None`): every component, including dynamic member
    // templates, is surfaced with its ty/shape (the dynamic-registration-mode
    // contract). Malformed fields are skipped — a real frame's vtable never errors here.
    for field in vtable.realize_fields(None).flatten() {
        set.insert(field.component_id, (field.ty, field.shape.to_vec()));
    }
    set
}

/// Whether a `producer` output satisfies a `consumer` input — the one compatibility
/// path for every port:
///
/// - The edge keys must match (`PortId` equality — Table ids and Postcard ids live in
///   disjoint spaces, so a cross-schema pair can never match).
/// - **Delivery must match across an edge**: a Log consumer of a Snapshot ring would
///   silently see coalesced gaps; a Snapshot consumer of a Log ring would silently
///   discard records. The facades make agreement automatic; a hand-modified desc
///   that disagrees is a build error.
/// - **Table/Table** (system.md §5.2): the consumer's component set is a **subset**
///   of the producer's with matching `ty`/`shape`. Subset (not equality) lets a
///   producer emit extra fields a consumer ignores (forward-compatible wiring).
/// - **Postcard/Postcard**: pure [`PacketId`] equality (already checked) — records
///   are opaque postcard blobs with no component structure.
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
    use metor_proto_wkt::{SequenceChannelEvent, SequenceCommand, SequenceRegistry};
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

    /// Constructor axis defaults match the facade table (§2.2): `of` mints
    /// `Table × Snapshot × One × Stop`, telemetered; `msg` mints
    /// `Postcard × Log × Many × Resync`, telemetered — and each `id` is consistent
    /// with its schema's value space.
    #[test]
    fn constructor_axis_defaults() {
        let f = PortDesc::of::<AxisProbe>();
        assert_eq!(f.id, PortId::Component(AxisProbe::FRAME_ID));
        assert_eq!(f.name, "axis_probe");
        assert!(matches!(f.schema, PortSchema::Table { .. }));
        assert_eq!(f.delivery, Delivery::Snapshot);
        assert_eq!(f.fan_in, FanIn::One);
        assert_eq!(f.on_lap, OnLap::Stop);
        assert!(f.telemetered);

        let m = PortDesc::msg::<SequenceCommand>();
        assert_eq!(m.id, PortId::Packet(SequenceCommand::ID));
        assert_eq!(m.name, "SequenceCommand");
        assert!(matches!(m.schema, PortSchema::Postcard));
        assert_eq!(m.delivery, Delivery::Log);
        assert_eq!(m.fan_in, FanIn::Many);
        assert_eq!(m.on_lap, OnLap::Resync);
        assert!(m.telemetered);
    }

    /// Each modifier overrides exactly one axis.
    #[test]
    fn modifiers_override_one_axis() {
        let d = PortDesc::of::<AxisProbe>().untelemetered();
        assert!(!d.telemetered);
        assert_eq!(d.delivery, Delivery::Snapshot);

        let d = PortDesc::msg::<SequenceCommand>().with_on_lap(OnLap::Stop);
        assert_eq!(d.on_lap, OnLap::Stop);
        assert_eq!(d.fan_in, FanIn::Many);

        let d = PortDesc::of::<AxisProbe>().with_fan_in(FanIn::Many);
        assert_eq!(d.fan_in, FanIn::Many);
        assert_eq!(d.on_lap, OnLap::Stop);

        // The generic fourth combination: an every-record frame log.
        let d = PortDesc::of::<AxisProbe>().with_delivery(Delivery::Log);
        assert_eq!(d.delivery, Delivery::Log);
        assert!(matches!(d.schema, PortSchema::Table { .. }));
    }

    /// Checked accessors return `None` off-schema — no panic reachable from the
    /// public API (S5).
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

    /// The `compatible` matrix: Table/Table subset rule, Postcard/Postcard id
    /// equality, cross-schema never, and **delivery mismatch never** (§2.6).
    #[test]
    fn compatible_matrix() {
        let f = PortDesc::of::<AxisProbe>();
        let m = PortDesc::msg::<SequenceCommand>();

        assert!(compatible(&f, &PortDesc::of::<AxisProbe>()));
        assert!(compatible(&m, &PortDesc::msg::<SequenceCommand>()));
        // Distinct Postcard ids never match.
        assert!(!compatible(&m, &PortDesc::msg::<SequenceRegistry>()));
        // Cross-schema never matches (disjoint id spaces already guarantee it).
        assert!(!compatible(&f, &m));
        assert!(!compatible(&m, &f));
        // NEW: delivery must match across an edge — a hand-modified desc that
        // disagrees is incompatible even with identical id + schema.
        let log_f = PortDesc::of::<AxisProbe>().with_delivery(Delivery::Log);
        assert!(!compatible(&f, &log_f));
        assert!(!compatible(&log_f, &f));
        assert!(compatible(&log_f, &log_f.clone()));
    }

    /// The wkt `NamedMsg` tokens are frozen to today's names (KDL/registry compat).
    #[test]
    fn wkt_named_msg_tokens_frozen() {
        assert_eq!(<SequenceCommand as NamedMsg>::NAME, "SequenceCommand");
        assert_eq!(<SequenceRegistry as NamedMsg>::NAME, "SequenceRegistry");
        assert_eq!(
            <SequenceChannelEvent as NamedMsg>::NAME,
            "SequenceChannelEvent"
        );
    }
}
