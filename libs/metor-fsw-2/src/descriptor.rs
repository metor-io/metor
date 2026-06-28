//! Static self-description a coordinator reads before any port exists (system.md
//! §5): the per-port [`PortDesc`], the [`SystemDescriptor`] bundle, and the
//! producer/consumer [`compatible`] check.
//!
//! Everything here is derived from the frame metadata WP2/WP3 already provide —
//! `F::FRAME_ID`, `F::as_vtable()`, `F::MAX_SIZE` — so a system can be sized,
//! allocated, and wiring-validated without constructing it.

use std::collections::HashMap;
use std::sync::Arc;

use metor_fsw::{AsVTable, Metadatatize};
use metor_proto::types::{ComponentId, PrimType};
use metor_proto::vtable::VTable;
use metor_proto::vtable::builder::vtable;
use metor_proto_wkt::ComponentMetadata;

use crate::frame::Frame;

/// A unit measured in Hertz; an advisory rate hint for buffer depth / async pacing.
pub type Hz = f64;

/// The type-erased prefix factory stored on [`PortDesc::announce`]: given an instance
/// name it returns the prefixed announce vtable + component metadata. An [`Arc`] boxed
/// closure (not a bare `fn`) so a future dlopen'd port — which has no static `F` — can
/// carry a closure capturing its metadata-derived prefix rewrite (dl-open.md §7).
pub type AnnounceFn = Arc<dyn Fn(&str) -> (VTable, Vec<ComponentMetadata>) + Send + Sync>;

/// One port's static shape: the frame identity, its vtable (the authoritative
/// component layout), its worst-case table size, and an advisory rate.
///
/// Used both for an output (a produced frame) and an input (a required frame
/// shape) — the two are structurally identical, the direction is which list of a
/// [`SystemDescriptor`] it sits in.
#[derive(Clone)]
pub struct PortDesc {
    /// `F::FRAME_ID`.
    pub frame_id: ComponentId,
    /// `F::NAME` — the unprefixed frame name, kept so the coordinator can compute
    /// the instance-qualified registry key `ComponentId::new("<instance>.<frame>")`
    /// without a static `F` (telemetry.md §2.2/§6).
    pub frame_name: &'static str,
    /// `F::as_vtable()` — enumerated in registration mode for compatibility. This is
    /// the **frame-relative** (unprefixed) vtable the wiring compatibility check uses;
    /// the telemetry-facing prefixed vtable is produced on demand by [`announce`](Self::announce).
    pub vtable: VTable,
    /// `F::MAX_SIZE` (worst-case table bytes); size a ring via [`crate::buffer_capacity`].
    pub max_size: usize,
    /// Advisory rate, for buffer depth / async pacing. `None` ⇒ use a global default.
    pub rate_hint: Option<Hz>,
    /// Prefix factory (telemetry.md §6): given an instance name, it re-derives the
    /// **prefixed** announce vtable + component metadata (`<instance>.<frame>.<field>`
    /// ids/names). A statically-linked port captures `F` here by wrapping
    /// [`announce_of::<F>`] (reusing `AsVTable::vtable_fields(prefix)` /
    /// `Metadatatize::metadata(prefix)`). The coordinator calls it once per buffer at
    /// `build()` and stores the result as the registry entry's canonical external schema.
    /// `F` is gone by build time (everything is `PortDesc`-erased), which is why this is
    /// type-erased — and it is an [`Arc`] boxed closure (not a bare `fn`) so a future
    /// dlopen'd port, which has no static `F`, can carry a closure capturing its
    /// metadata-derived prefix rewrite instead (dl-open.md §7, Q-announce).
    pub announce: AnnounceFn,
}

impl std::fmt::Debug for PortDesc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `announce` is a boxed closure (no `Debug`); render it as an opaque marker.
        f.debug_struct("PortDesc")
            .field("frame_id", &self.frame_id)
            .field("frame_name", &self.frame_name)
            .field("vtable", &self.vtable)
            .field("max_size", &self.max_size)
            .field("rate_hint", &self.rate_hint)
            .field("announce", &"<closure>")
            .finish()
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

impl PortDesc {
    /// Derives the descriptor for a frame type. Pure metadata — no instance needed.
    pub fn of<F: Frame>() -> Self {
        Self {
            frame_id: F::FRAME_ID,
            frame_name: F::NAME,
            vtable: F::as_vtable(),
            max_size: F::MAX_SIZE,
            rate_hint: None,
            // Coerce the `F`-closing fn item to a plain fn pointer first (erasing `F`, so
            // no `F: 'static` bound is needed), then box it as the type-erased `Arc<dyn Fn>`.
            announce: Arc::new(announce_of::<F> as fn(&str) -> (VTable, Vec<ComponentMetadata>)),
        }
    }

    /// As [`PortDesc::of`] but carrying an advisory rate hint.
    pub fn of_at<F: Frame>(rate_hint: Hz) -> Self {
        Self {
            rate_hint: Some(rate_hint),
            ..Self::of::<F>()
        }
    }
}

/// How the coordinator drives a system. Carried on the descriptor as metadata for
/// WP5; the trait a user implements ([`CyclicSystem`](crate::CyclicSystem) vs
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
    // templates, is surfaced with its ty/shape (the WP2 `test_dynamic_registration_mode`
    // contract). Malformed fields are skipped — a real frame's vtable never errors here.
    for field in vtable.realize_fields(None).flatten() {
        set.insert(field.component_id, (field.ty, field.shape.to_vec()));
    }
    set
}

/// Whether a `producer` output satisfies a `consumer` input (system.md §5.2):
/// same `frame_id`, and the consumer's component set is a **subset** of the
/// producer's with matching `ty`/`shape`. Subset (not equality) lets a producer
/// emit extra fields a consumer ignores (forward-compatible wiring).
pub fn compatible(producer: &PortDesc, consumer: &PortDesc) -> bool {
    if producer.frame_id != consumer.frame_id {
        return false;
    }
    let prod = realize_set(&producer.vtable);
    let cons = realize_set(&consumer.vtable);
    cons.iter().all(|(id, want)| match prod.get(id) {
        Some(have) => have == want,
        None => false,
    })
}
