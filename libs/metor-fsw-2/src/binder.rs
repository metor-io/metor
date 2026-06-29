//! The `bind` contract that resolves a system's deferred port construction
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

use metor_fsw_ring::{Backing, BoxBacking, RingBuffer, WakeSink, WakeSource};

use crate::registry::{MessageRegistry, OutputRegistry};

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
    messages: Arc<MessageRegistry>,
}

impl<'a> Binder<'a> {
    pub(crate) fn new(
        outputs: &'a [BoundPort],
        inputs: &'a [BoundPort],
        registry: Arc<OutputRegistry>,
        messages: Arc<MessageRegistry>,
    ) -> Self {
        Self {
            outputs: outputs.iter(),
            inputs: inputs.iter(),
            registry,
            messages,
        }
    }
}

/// Where a bound port's pre-allocated ring comes from, abstracted over the ring
/// [`Backing`] so one generated bundle `bind` serves both providers: the host's
/// [`Binder`] (`B = BoxBacking`, over the coordinator's pre-allocated [`BoundPort`]s)
/// and a dlopen'd system's [`RawBinder`](crate::abi::RawBinder) (`B = RawBacking`, over
/// the host's raw regions). The positional contract is the same for both: `bind` pops
/// one ring per port via [`next_output`](Self::next_output)/[`next_input`](Self::next_input)
/// in `descriptors()` order.
pub trait RingSource {
    /// The ring backing this source hands out.
    type B: Backing;

    /// Pop the next output ring and its writer-side wake endpoints, of the concrete
    /// `WD`/`WS` the port type supplies.
    fn next_output<WD, WS>(&mut self) -> (RingBuffer<Self::B>, WD, WS)
    where
        WD: WakeSource + Default + Clone + 'static,
        WS: WakeSink + Default + Clone + 'static;

    /// Pop the next input ring and its reader-side wake endpoints.
    fn next_input<RD, RS>(&mut self) -> (RingBuffer<Self::B>, RD, RS)
    where
        RD: WakeSink + Default + Clone + 'static,
        RS: WakeSource + Default + Clone + 'static;

    /// The broad-access output registry (telemetry.md §2.4). A bundle that wants by-id
    /// access to *every* output (the telemetry downlink, a logger, a recorder) pulls
    /// this in its `BindPorts::bind`, exactly where it pulls its typed ports. Only the
    /// host [`Binder`] carries one; a system that needs it is therefore host-only
    /// (`B = BoxBacking`), so the default — used by any non-host source — panics rather
    /// than fabricate an empty registry.
    fn output_registry(&self) -> Arc<OutputRegistry> {
        panic!("this ring source carries no output registry (host-only capability)")
    }

    /// The broad-access **message** registry (`docs/messages.md` §2), the message twin of
    /// [`output_registry`](Self::output_registry). A bundle that taps every message channel
    /// (the telemetry downlink, W2) pulls this in its `BindPorts::bind` alongside its typed
    /// ports. Only the host [`Binder`] carries one; any non-host source panics rather than
    /// fabricate an empty registry, exactly as the output registry does.
    fn message_registry(&self) -> Arc<MessageRegistry> {
        panic!("this ring source carries no message registry (host-only capability)")
    }
}

/// The host's ring source: pops the coordinator's pre-allocated [`BoundPort`]s, in
/// `descriptors()` order, with their optional matched wake endpoints (the copy-in path).
impl<'a> RingSource for Binder<'a> {
    type B = BoxBacking;

    fn next_output<WD, WS>(&mut self) -> (RingBuffer<BoxBacking>, WD, WS)
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

    fn next_input<RD, RS>(&mut self) -> (RingBuffer<BoxBacking>, RD, RS)
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

    /// The registry is complete before the bind loop runs, so this is safe and needs no
    /// second phase. The returned `Arc` is cheap to clone and store.
    fn output_registry(&self) -> Arc<OutputRegistry> {
        self.registry.clone()
    }

    /// The message registry is frozen alongside the output registry before the bind loop
    /// runs (`docs/messages.md` §2), so this is safe and needs no second phase.
    fn message_registry(&self) -> Arc<MessageRegistry> {
        self.messages.clone()
    }
}

/// A port bundle (a `#[derive(SystemInput)]`/`#[derive(SystemOutput)]` struct, or
/// the framework [`Out`](crate::Out) wrapper) constructible from a [`RingSource`] over
/// the ring backing `B`. The derives generate this symmetrically to `descriptors()`;
/// a host bundle is `BindPorts<BoxBacking>`, a `Backing`-generic bundle is
/// `BindPorts<B>` for all `B`.
pub trait BindPorts<B: Backing>: Sized {
    /// Construct every port from the ring source, in `descriptors()` order.
    fn bind<S: RingSource<B = B>>(src: &mut S) -> Self;
}
