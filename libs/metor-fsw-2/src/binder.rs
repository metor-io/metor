//! The `bind` contract that resolves WP4's deferred port construction
//! (coordinator.md §1.4).
//!
//! The build phase pre-allocates one [`RingBuffer`] per port and walks each
//! system's port bundle to hand every typed [`Output`](crate::Output)/
//! [`Input`](crate::Input) its ring. The walk is positional: [`BindPorts::bind`]
//! visits port fields in the *same order* as `descriptors()`, so a [`Binder`]
//! cursor lines each port up with the ring the builder reserved for it.
//!
//! ## Matched wake endpoints
//!
//! A [`Notifier`](metor_fsw_ring::Notifier) is `Arc`-backed: a commit only wakes
//! an awaiting reader when the writer side and the view side hold the *same*
//! clone. Every cross-system edge in v1 is sampled by a polling consumer (a cyclic
//! system each cycle, or a copy-in job each cycle), so its view can use a fresh
//! default wake and the match is moot. The one place a matched clone is
//! load-bearing is the private copy-in buffer feeding an async input: there the
//! coordinator pre-creates the `Notifier` pair, stores it type-erased on the
//! port's [`BoundPort`], and hands the matched clone to the async view. Every
//! other port leaves the endpoints empty and the binder default-constructs.

use std::any::Any;
use std::slice;
use std::sync::Arc;

use metor_fsw_ring::{BoxBacking, RingBuffer, WakeSink, WakeSource};

use crate::registry::OutputRegistry;

/// One pre-allocated ring plus its optional matched wake endpoints, in
/// `descriptors()` order. `data`/`space` are `Some` only for the copy-in private
/// buffer that feeds an async input (where the view must share the writer's
/// `Notifier`); otherwise the binder default-constructs the wake.
pub struct BoundPort {
    ring: RingBuffer<BoxBacking>,
    data: Option<Box<dyn Any>>,
    space: Option<Box<dyn Any>>,
}

impl BoundPort {
    /// A port whose wake endpoints are default-constructed at bind time.
    pub(crate) fn new(ring: RingBuffer<BoxBacking>) -> Self {
        Self {
            ring,
            data: None,
            space: None,
        }
    }

    /// A port carrying pre-created, matched wake endpoints (the copy-in path).
    pub(crate) fn matched(ring: RingBuffer<BoxBacking>, data: Box<dyn Any>, space: Box<dyn Any>) -> Self {
        Self {
            ring,
            data: Some(data),
            space: Some(space),
        }
    }

    fn wake<T: Default + Clone + 'static>(slot: &Option<Box<dyn Any>>) -> T {
        match slot {
            Some(b) => b
                .downcast_ref::<T>()
                .expect("wake endpoint type matches the port")
                .clone(),
            None => T::default(),
        }
    }
}

/// A positional cursor over one system's pre-allocated ports. The generated
/// [`BindPorts::bind`] pops one ring per port via [`next_output`](Self::next_output)
/// / [`next_input`](Self::next_input) in `descriptors()` order.
pub struct Binder<'a> {
    outputs: slice::Iter<'a, BoundPort>,
    inputs: slice::Iter<'a, BoundPort>,
    registry: Arc<OutputRegistry>,
}

impl<'a> Binder<'a> {
    pub(crate) fn new(
        outputs: &'a [BoundPort],
        inputs: &'a [BoundPort],
        registry: Arc<OutputRegistry>,
    ) -> Self {
        Self {
            outputs: outputs.iter(),
            inputs: inputs.iter(),
            registry,
        }
    }

    /// The broad-access output registry (telemetry.md §2.4). A system whose bundle
    /// wants by-id access to *every* output (the telemetry downlink, a logger, a
    /// recorder) pulls this in its `BindPorts::bind`, exactly where it pulls its typed
    /// ports. The registry is complete before the bind loop runs, so this is safe and
    /// needs no second phase. The returned `Arc` is cheap to clone and store.
    pub fn output_registry(&self) -> Arc<OutputRegistry> {
        self.registry.clone()
    }

    /// Pop the next output ring and its writer-side wake endpoints, downcast to
    /// the concrete `WD`/`WS` the port type supplies.
    pub fn next_output<WD, WS>(&mut self) -> (RingBuffer<BoxBacking>, WD, WS)
    where
        WD: WakeSource + Default + Clone + 'static,
        WS: WakeSink + Default + Clone + 'static,
    {
        let p = self
            .outputs
            .next()
            .expect("bind() walks output ports in descriptors() order");
        (
            p.ring.clone(),
            BoundPort::wake(&p.data),
            BoundPort::wake(&p.space),
        )
    }

    /// Pop the next input ring and its reader-side wake endpoints.
    pub fn next_input<RD, RS>(&mut self) -> (RingBuffer<BoxBacking>, RD, RS)
    where
        RD: WakeSink + Default + Clone + 'static,
        RS: WakeSource + Default + Clone + 'static,
    {
        let p = self
            .inputs
            .next()
            .expect("bind() walks input ports in descriptors() order");
        (
            p.ring.clone(),
            BoundPort::wake(&p.data),
            BoundPort::wake(&p.space),
        )
    }
}

/// A port bundle (a `#[derive(SystemInput)]`/`#[derive(SystemOutput)]` struct, or
/// the framework [`Out`](crate::Out) wrapper) constructible from a [`Binder`].
/// The derives generate this symmetrically to `descriptors()`.
pub trait BindPorts: Sized {
    /// Construct every port from the binder, in `descriptors()` order.
    fn bind(binder: &mut Binder) -> Self;
}
