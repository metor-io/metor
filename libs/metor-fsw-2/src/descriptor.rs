//! Static self-description a coordinator reads before any port exists (system.md
//! §5): the per-port [`PortDesc`], the [`SystemDescriptor`] bundle, and the
//! producer/consumer [`compatible`] check.
//!
//! Everything here is derived from the frame metadata WP2/WP3 already provide —
//! `F::FRAME_ID`, `F::as_vtable()`, `F::MAX_SIZE` — so a system can be sized,
//! allocated, and wiring-validated without constructing it.

use std::collections::HashMap;

use metor_proto::types::{ComponentId, PrimType};
use metor_proto::vtable::VTable;

use crate::frame::Frame;

/// A unit measured in Hertz; an advisory rate hint for buffer depth / async pacing.
pub type Hz = f64;

/// One port's static shape: the frame identity, its vtable (the authoritative
/// component layout), its worst-case table size, and an advisory rate.
///
/// Used both for an output (a produced frame) and an input (a required frame
/// shape) — the two are structurally identical, the direction is which list of a
/// [`SystemDescriptor`] it sits in.
#[derive(Clone, Debug)]
pub struct PortDesc {
    /// `F::FRAME_ID`.
    pub frame_id: ComponentId,
    /// `F::as_vtable()` — enumerated in registration mode for compatibility.
    pub vtable: VTable,
    /// `F::MAX_SIZE` (worst-case table bytes); size a ring via [`crate::buffer_capacity`].
    pub max_size: usize,
    /// Advisory rate, for buffer depth / async pacing. `None` ⇒ use a global default.
    pub rate_hint: Option<Hz>,
}

impl PortDesc {
    /// Derives the descriptor for a frame type. Pure metadata — no instance needed.
    pub fn of<F: Frame>() -> Self {
        Self {
            frame_id: F::FRAME_ID,
            vtable: F::as_vtable(),
            max_size: F::MAX_SIZE,
            rate_hint: None,
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
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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
