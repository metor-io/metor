//! Static self-description a coordinator reads before any port exists (system.md
//! §5): the per-port [`PortDesc`], the [`SystemDescriptor`] bundle, and the
//! producer/consumer [`compatible`] check.
//!
//! Everything here is derived from a frame's metadata — `F::FRAME_ID`,
//! `F::as_vtable()`, `F::MAX_SIZE` — so a system can be sized, allocated, and
//! wiring-validated without constructing it.

use std::collections::HashMap;
use std::sync::Arc;

use metor_fsw::{AsVTable, Metadatatize};
use metor_proto::types::{ComponentId, Msg, PacketId, PrimType};
use metor_proto::vtable::VTable;
use metor_proto::vtable::builder::vtable;
use metor_proto_wkt::ComponentMetadata;

use crate::frame::Frame;
use crate::message::MAX_MSG_BYTES;

/// A unit measured in Hertz; an advisory rate hint for buffer depth / async pacing.
pub type Hz = f64;

/// The type-erased prefix factory stored on [`PortDesc::announce`]: given an instance
/// name it returns the prefixed announce vtable + component metadata. An [`Arc`] boxed
/// closure (not a bare `fn`) so a dlopen'd port — which has no static `F` — can
/// carry a closure capturing its metadata-derived prefix rewrite.
pub type AnnounceFn = Arc<dyn Fn(&str) -> (VTable, Vec<ComponentMetadata>) + Send + Sync>;

/// The edge key of a port: a frame id (component space) or a message id (the disjoint
/// 2-byte [`PacketId`] space). A frame port and a message port live in different value
/// spaces, so they can never accidentally match on an edge.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum PortId {
    /// `F::FRAME_ID`.
    Frame(ComponentId),
    /// `M::ID`.
    Msg(PacketId),
}

impl PortId {
    /// The frame [`ComponentId`] of a frame port. Frame-only paths (the registry, the
    /// ring table, the dl ABI, health/log/status, and today's wiring diagnostics) call
    /// this; a message port's id is a 2-byte [`PacketId`], not a `ComponentId`, so this
    /// panics on one.
    pub fn frame_id(self) -> ComponentId {
        match self {
            PortId::Frame(f) => f,
            PortId::Msg(_) => panic!("PortId::frame_id() on a message port"),
        }
    }
}

/// The kind-specific payload of a [`PortDesc`]. Only [`Frame`](PortKind::Frame) carries
/// the vtable + announce factory the wiring compatibility check and telemetry announce
/// need; a [`Message`](PortKind::Message) port is self-describing (its 2-byte id is its
/// schema), and [`ReceiveAll`](PortKind::ReceiveAll) is a broadcast registry tap that
/// reserves no ring and connects no edge.
#[derive(Clone)]
pub enum PortKind {
    /// A component-frame port. See [`PortDesc::announce`] for the prefix factory.
    Frame {
        /// `F::as_vtable()` — the **frame-relative** (unprefixed) vtable the wiring
        /// compatibility check uses; the prefixed vtable is produced on demand by the
        /// announce factory.
        vtable: VTable,
        /// Prefix factory (telemetry.md §6): given an instance name, it re-derives the
        /// **prefixed** announce vtable + component metadata. Type-erased ([`Arc`] boxed
        /// closure) so a dlopen'd port, which has no static `F`, can carry a closure
        /// capturing its metadata-derived prefix rewrite instead.
        announce: AnnounceFn,
    },
    /// A message-channel port (`docs/messages.md`). Self-describing — no vtable/announce.
    Message {
        /// Whether the telemetry downlink / `AllOutputs` taps this channel
        /// (`docs/message-wiring.md` §6.4). Command channels set this `false`.
        telemetered: bool,
    },
    /// A broadcast tap over every output + message channel (`docs/message-wiring.md` §4).
    ReceiveAll,
}

/// One port's static shape: its edge key ([`id`](PortDesc::id)), display name, worst-case
/// size, advisory rate, and [`kind`](PortDesc::kind)-specific payload.
///
/// Used both for an output (a produced frame/message) and an input (a required shape) —
/// the two are structurally identical, the direction is which list of a
/// [`SystemDescriptor`] it sits in.
#[derive(Clone)]
pub struct PortDesc {
    /// The edge key: `PortId::Frame(F::FRAME_ID)` or `PortId::Msg(M::ID)`.
    pub id: PortId,
    /// `F::NAME` / the message schema name / `""` for a receive-all tap — kept so the
    /// coordinator can compute the instance-qualified registry key
    /// `ComponentId::new("<instance>.<name>")` without a static type (telemetry.md §2.2/§6).
    pub name: &'static str,
    /// `F::MAX_SIZE` / `MAX_MSG_BYTES` (worst-case bytes); size a ring via
    /// [`crate::buffer_capacity`]. `0` for a receive-all tap.
    pub max_size: usize,
    /// Advisory rate, for buffer depth / async pacing. `None` ⇒ use a global default.
    pub rate_hint: Option<Hz>,
    /// The kind-specific payload (frame vtable/announce, message telemetered-flag, or tap).
    pub kind: PortKind,
}

impl std::fmt::Debug for PortDesc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `announce` (inside `PortKind::Frame`) is a boxed closure (no `Debug`); the
        // manual impl renders the kind without it.
        let mut s = f.debug_struct("PortDesc");
        s.field("id", &self.id)
            .field("name", &self.name)
            .field("max_size", &self.max_size)
            .field("rate_hint", &self.rate_hint);
        match &self.kind {
            PortKind::Frame { vtable, .. } => s.field("kind", &"Frame").field("vtable", vtable),
            PortKind::Message { telemetered } => {
                s.field("kind", &"Message").field("telemetered", telemetered)
            }
            PortKind::ReceiveAll => s.field("kind", &"ReceiveAll"),
        };
        s.finish()
    }
}

/// The prefix factory stored on [`PortDesc::announce`]: re-derive `F`'s vtable +
/// metadata under the dotted `prefix` (the instance name). A `&str` is a
/// [`ComponentPath`](metor_fsw::path::ComponentPath), so the leaves roll the same
/// ids as `ComponentId::new("<prefix>.<frame>.<field>")`.
fn announce_of<F: Frame>(prefix: &str) -> (VTable, Vec<ComponentMetadata>) {
    let vt = vtable(<F as AsVTable>::vtable_fields(prefix));
    let metadata = <F as Metadatatize>::metadata(prefix).collect();
    (vt, metadata)
}

/// The display / KDL-addressing name of a message port: the message type's own name (the
/// last `::` segment of its type path). Unlike a frame's `F::NAME`, a [`Msg`] carries no
/// name constant — many wkt `Msg`s (e.g. `SequenceCommand`) hand-assign [`Msg::ID`] and do
/// not implement `postcard_schema::Schema` — so the type name is the stable,
/// zero-boilerplate token a user writes as `msg="SequenceCommand"`
/// (`docs/message-wiring.md` §3.4). The real edge key is [`Msg::ID`]; this is only for
/// addressing / telemetry keying.
fn msg_name<M: Msg>() -> &'static str {
    let path = std::any::type_name::<M>();
    path.rsplit("::").next().unwrap_or(path)
}

impl PortDesc {
    /// Derives the descriptor for a frame type. Pure metadata — no instance needed.
    pub fn of<F: Frame>() -> Self {
        Self {
            id: PortId::Frame(F::FRAME_ID),
            name: F::NAME,
            max_size: F::MAX_SIZE,
            rate_hint: None,
            kind: PortKind::Frame {
                vtable: F::as_vtable(),
                // Coerce the `F`-closing fn item to a plain fn pointer first (erasing `F`,
                // so no `F: 'static` bound is needed), then box it as the type-erased
                // `Arc<dyn Fn>`.
                announce: Arc::new(
                    announce_of::<F> as fn(&str) -> (VTable, Vec<ComponentMetadata>),
                ),
            },
        }
    }

    /// As [`PortDesc::of`] but carrying an advisory rate hint.
    pub fn of_at<F: Frame>(rate_hint: Hz) -> Self {
        Self {
            rate_hint: Some(rate_hint),
            ..Self::of::<F>()
        }
    }

    /// Derives the descriptor for a message type `M` (`docs/message-wiring.md` §2.2). The
    /// edge key is `M::ID`; the name is the type's own name (see [`msg_name`]). Telemetered —
    /// a command channel opts out via [`PortDesc::msg_untelemetered`] (a [`crate::CommandOut`]).
    pub fn msg<M: Msg>() -> Self {
        Self::msg_with::<M>(true)
    }

    /// As [`PortDesc::msg`] but **not** telemetered: the channel is a first-class wired
    /// message port yet is never tapped by the downlink / `AllOutputs`
    /// (`docs/message-wiring.md` §6.4). Used for command channels (inbound control).
    pub fn msg_untelemetered<M: Msg>() -> Self {
        Self::msg_with::<M>(false)
    }

    fn msg_with<M: Msg>(telemetered: bool) -> Self {
        Self {
            id: PortId::Msg(M::ID),
            name: msg_name::<M>(),
            max_size: MAX_MSG_BYTES,
            rate_hint: None,
            kind: PortKind::Message { telemetered },
        }
    }

    /// The descriptor of a receive-all tap port (`docs/message-wiring.md` §4): it reserves no
    /// ring and connects no edge, so its `id` is a never-matched sentinel and its size is 0.
    pub fn receive_all() -> Self {
        Self {
            id: PortId::Frame(ComponentId::new("")),
            name: "",
            max_size: 0,
            rate_hint: None,
            kind: PortKind::ReceiveAll,
        }
    }

    /// The frame [`ComponentId`] of a frame port — see [`PortId::frame_id`]. The
    /// registry / ring-table / dl-ABI / health paths (all frame-only) call this; it
    /// panics on a message or receive-all port.
    pub fn frame_id(&self) -> ComponentId {
        self.id.frame_id()
    }

    /// The frame-relative vtable of a frame port (wiring compatibility / telemetry
    /// announce). Panics on a non-frame port.
    pub fn vtable(&self) -> &VTable {
        match &self.kind {
            PortKind::Frame { vtable, .. } => vtable,
            _ => panic!("PortDesc::vtable() on a non-frame port"),
        }
    }

    /// The prefix-announce factory of a frame port. Panics on a non-frame port.
    pub fn announce(&self) -> &AnnounceFn {
        match &self.kind {
            PortKind::Frame { announce, .. } => announce,
            _ => panic!("PortDesc::announce() on a non-frame port"),
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

/// A system's full self-description: its name, driving kind, and the static shapes
/// of every input and output port.
#[derive(Clone, Debug)]
pub struct SystemDescriptor {
    pub name: &'static str,
    pub kind: SystemKind,
    pub inputs: Vec<PortDesc>,
    pub outputs: Vec<PortDesc>,
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

/// Whether a `producer` output satisfies a `consumer` input:
///
/// - **Frame ports** (system.md §5.2): same `frame_id`, and the consumer's component set
///   is a **subset** of the producer's with matching `ty`/`shape`. Subset (not equality)
///   lets a producer emit extra fields a consumer ignores (forward-compatible wiring).
/// - **Message ports** (`docs/message-wiring.md` §3.5): pure [`PacketId`] equality —
///   messages are opaque postcard blobs with no component structure, so there is no
///   subset relation.
/// - A frame port and a message port (or a receive-all tap) never satisfy an edge.
pub fn compatible(producer: &PortDesc, consumer: &PortDesc) -> bool {
    match (&producer.kind, &consumer.kind) {
        (PortKind::Frame { vtable: pv, .. }, PortKind::Frame { vtable: cv, .. }) => {
            if producer.id != consumer.id {
                return false;
            }
            let prod = realize_set(pv);
            let cons = realize_set(cv);
            cons.iter().all(|(id, want)| match prod.get(id) {
                Some(have) => have == want,
                None => false,
            })
        }
        (PortKind::Message { .. }, PortKind::Message { .. }) => producer.id == consumer.id,
        _ => false,
    }
}
