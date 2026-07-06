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
//! coordinator pre-creates the data `Notifier`, stores it type-erased on the
//! port's [`BoundPort`], and hands the matched clone to the async view. Every
//! other endpoint is default-constructed by the binder (cyclic writers use the
//! non-blocking `try_write`, so no framework writer ever awaits a space wake).

use std::any::Any;
use std::slice;
use std::sync::Arc;

use metor_fsw_ring::{RingBuffer, WakeSink, WakeSource};

use crate::registry::Registry;

/// One pre-allocated ring plus its optional matched data-wake endpoint, in
/// `descriptors()` order. `data` is `Some` only for the copy-in private buffer that
/// feeds an async input (where the view must share the writer's `Notifier` to be
/// woken); otherwise the binder default-constructs the wake. There is no matched
/// *space* endpoint: framework writers use the non-blocking `try_write`, so no
/// write ever suspends for space and the reader-side space notification has no
/// listener.
pub struct BoundPort {
    ring: RingBuffer,
    data: Option<Box<dyn Any>>,
}

impl BoundPort {
    /// A port whose wake endpoints are default-constructed at bind time.
    pub(crate) fn new(ring: RingBuffer) -> Self {
        Self { ring, data: None }
    }

    /// A port carrying the pre-created, matched data-wake endpoint (the copy-in path).
    pub(crate) fn matched(ring: RingBuffer, data: Box<dyn Any>) -> Self {
        Self {
            ring,
            data: Some(data),
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

/// One input port's bound ring(s), in `descriptors()` order. A frame input (or a
/// single-producer message input) is [`One`](BoundInput::One); a fanned-in message input
/// (`docs/message-wiring.md` §3.3) is [`Many`](BoundInput::Many) — zero, one, or many
/// producer rings. Frame ports always bind `One`.
pub enum BoundInput {
    One(BoundPort),
    Many(Vec<BoundPort>),
}

/// A positional cursor over one system's pre-allocated ports. The generated
/// [`BindPorts::bind`] pops one ring per port via [`next_output`](Self::next_output)
/// / [`next_input`](Self::next_input) in `descriptors()` order.
pub struct Binder<'a> {
    outputs: slice::Iter<'a, BoundPort>,
    inputs: slice::Iter<'a, BoundInput>,
    registry: Arc<Registry>,
}

impl<'a> Binder<'a> {
    pub(crate) fn new(
        outputs: &'a [BoundPort],
        inputs: &'a [BoundInput],
        registry: Arc<Registry>,
    ) -> Self {
        Self {
            outputs: outputs.iter(),
            inputs: inputs.iter(),
            registry,
        }
    }
}

/// Where a bound port's pre-allocated ring comes from. Rings are backing-erased,
/// so one generated bundle `bind` serves both providers with one monomorphic
/// code path: the host's [`Binder`] (over the coordinator's pre-allocated
/// [`BoundPort`]s) and a dlopen'd system's [`RawBinder`](crate::abi::RawBinder)
/// (over non-owning attaches to the host's regions). The positional contract is
/// the same for both: `bind` pops one ring per port via
/// [`next_output`](Self::next_output)/[`next_input`](Self::next_input) in
/// `descriptors()` order.
pub trait RingSource {
    /// Pop the next output ring and its writer-side wake endpoints, of the concrete
    /// `WD`/`WS` the port type supplies.
    fn next_output<WD, WS>(&mut self) -> (RingBuffer, WD, WS)
    where
        WD: WakeSource + Default + Clone + 'static,
        WS: WakeSink + Default + Clone + 'static;

    /// Pop the next input ring and its reader-side wake endpoints (a frame port, or a
    /// single-producer message port).
    fn next_input<RD, RS>(&mut self) -> (RingBuffer, RD, RS)
    where
        RD: WakeSink + Default + Clone + 'static,
        RS: WakeSource + Default + Clone + 'static;

    /// Pop **every** producer ring wired to the next (message) input port — the fan-in list
    /// (`docs/message-wiring.md` §3.3). `Vec::new()` is a legal, unconnected message input
    /// (reads nothing). Frame ports call [`next_input`](Self::next_input) instead; only
    /// [`MsgIn::bind`](crate::MsgIn) calls this. The default (non-host sources, which never
    /// carry message ports) yields no producers.
    fn next_input_fanin<RD, RS>(&mut self) -> Vec<(RingBuffer, RD, RS)>
    where
        RD: WakeSink + Default + Clone + 'static,
        RS: WakeSource + Default + Clone + 'static,
    {
        Vec::new()
    }

    /// The broad-access registry over every registered buffer (telemetry.md §2.4).
    /// A bundle that wants by-id access to *every* output (the telemetry downlink, a
    /// logger, a recorder) pulls this in its `BindPorts::bind`, exactly where it
    /// pulls its typed ports. Only the host [`Binder`] carries one; a system that
    /// needs it is therefore host-only, so the default — used by any non-host
    /// source — panics rather than fabricate an empty registry.
    fn registry(&self) -> Arc<Registry> {
        panic!("this ring source carries no registry (host-only capability)")
    }
}

/// The host's ring source: pops the coordinator's pre-allocated [`BoundPort`]s, in
/// `descriptors()` order, with their optional matched wake endpoints (the copy-in path).
impl<'a> RingSource for Binder<'a> {
    fn next_output<WD, WS>(&mut self) -> (RingBuffer, WD, WS)
    where
        WD: WakeSource + Default + Clone + 'static,
        WS: WakeSink + Default + Clone + 'static,
    {
        let p = self
            .outputs
            .next()
            .expect("bind() walks output ports in descriptors() order");
        (p.ring.clone(), BoundPort::wake(&p.data), WS::default())
    }

    fn next_input<RD, RS>(&mut self) -> (RingBuffer, RD, RS)
    where
        RD: WakeSink + Default + Clone + 'static,
        RS: WakeSource + Default + Clone + 'static,
    {
        let p = match self
            .inputs
            .next()
            .expect("bind() walks input ports in descriptors() order")
        {
            BoundInput::One(p) => p,
            BoundInput::Many(_) => {
                panic!("a frame input was laid out as a message fan-in (BoundInput::Many)")
            }
        };
        (p.ring.clone(), BoundPort::wake(&p.data), RS::default())
    }

    fn next_input_fanin<RD, RS>(&mut self) -> Vec<(RingBuffer, RD, RS)>
    where
        RD: WakeSink + Default + Clone + 'static,
        RS: WakeSource + Default + Clone + 'static,
    {
        let ports: &[BoundPort] = match self
            .inputs
            .next()
            .expect("bind() walks input ports in descriptors() order")
        {
            BoundInput::One(p) => slice::from_ref(p),
            BoundInput::Many(v) => v,
        };
        ports
            .iter()
            .map(|p| (p.ring.clone(), BoundPort::wake(&p.data), RS::default()))
            .collect()
    }

    /// The registry is complete before the bind loop runs, so this is safe and needs no
    /// second phase. The returned `Arc` is cheap to clone and store.
    fn registry(&self) -> Arc<Registry> {
        self.registry.clone()
    }
}

/// A port bundle (a `#[derive(SystemInput)]`/`#[derive(SystemOutput)]` struct, or
/// the framework [`Out`](crate::Out) wrapper) constructible from a [`RingSource`].
/// The derives generate this symmetrically to `descriptors()`; rings are
/// backing-erased, so the same impl binds over the host's heap rings and a
/// dlopen'd system's raw attaches.
pub trait BindPorts: Sized {
    /// Construct every port from the ring source, in `descriptors()` order.
    fn bind<S: RingSource>(src: &mut S) -> Self;
}
