//! Deferred port construction over rings chosen during graph build.
//!
//! Each output gets one [`RingBuffer`]. A frame input often gets a view into
//! its producer's output ring. A message input may get one ring per producer.
//! An async snapshot input gets a private copy-in ring. Binding walks each
//! bundle in descriptor order, so the ring plan and generated
//! [`BindPorts::bind`] code must use the same order.
//!
//! ## Matched wake endpoints
//!
//! A wake endpoint is `Arc`-backed, so a commit only wakes an awaiting reader
//! when the writer side and the reader side hold clones of the same endpoint.
//! Most edges never need the match. Cyclic consumers sample their inputs every
//! cycle and can use a fresh default endpoint, and every writer uses the
//! non-blocking `try_write`, so only the data direction ever wakes. The one
//! load-bearing match is the private copy-in buffer that feeds an async
//! input. There the builder pre-creates the data endpoint, stores it
//! type-erased on the port's [`BoundPort`], and the binder hands the matched
//! clone to the async view. Every other endpoint is default-constructed at
//! bind time.

use std::any::Any;
use std::slice;
use std::sync::Arc;

use metor_fsw_ring::{RingBuffer, WakeSink, WakeSource};

use crate::registry::Registry;

/// A pre-allocated [`RingBuffer`] plus an optional matched data-wake
/// endpoint.
///
/// `data` is `Some` only for the copy-in buffer feeding an async input, where
/// the reader must hold the writer's endpoint to be woken (see the module
/// doc). There is no space endpoint at all, since writers use the
/// non-blocking `try_write` and nothing ever listens for space.
pub struct BoundPort {
    ring: RingBuffer,
    data: Option<Box<dyn Any>>,
}

impl BoundPort {
    /// A port whose wake endpoints are default-constructed at bind time.
    pub(crate) fn new(ring: RingBuffer) -> Self {
        Self { ring, data: None }
    }

    /// A port carrying a pre-created, matched data-wake endpoint.
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

/// The pre-allocated ring, or set of per-producer rings, feeding one input
/// port.
///
/// A frame input, or a single-producer message input, binds
/// [`One`](BoundInput::One). A fanned-in message input binds
/// [`Many`](BoundInput::Many), with one [`BoundPort`] per producer and
/// possibly none.
pub enum BoundInput {
    One(BoundPort),
    Many(Vec<BoundPort>),
}

/// A positional cursor over one system's pre-allocated ports.
///
/// The generated [`BindPorts::bind`] pops one entry per port, in
/// `descriptors()` order.
pub struct Binder<'a> {
    outputs: slice::Iter<'a, BoundPort>,
    inputs: slice::Iter<'a, BoundInput>,
    registry: Arc<Registry>,
    instance: &'a str,
}

impl<'a> Binder<'a> {
    pub(crate) fn new(
        outputs: &'a [BoundPort],
        inputs: &'a [BoundInput],
        registry: Arc<Registry>,
        instance: &'a str,
    ) -> Self {
        Self {
            outputs: outputs.iter(),
            inputs: inputs.iter(),
            registry,
            instance,
        }
    }
}

/// A supplier of pre-allocated rings, popped one per port in `descriptors()`
/// order.
///
/// Rings are backing-erased, so one generated `bind` serves every supplier
/// with a single monomorphic code path. The host's [`Binder`] pops the
/// [`BoundPort`]s the builder reserved; a dynamically loaded system's
/// [`RawBinder`](crate::abi::RawBinder) pops non-owning attaches to the
/// host's regions.
pub trait RingSource {
    /// Pop the next output ring and its writer-side data-wake endpoint.
    fn next_output<WD>(&mut self) -> (RingBuffer, WD)
    where
        WD: WakeSource + Default + Clone + 'static;

    /// Pop the next input ring and its reader-side data-wake endpoint.
    fn next_input<RD>(&mut self) -> (RingBuffer, RD)
    where
        RD: WakeSink + Default + Clone + 'static;

    /// Pop the next output ring if one remains.
    ///
    /// A bundle whose trailing output count is decided by configuration
    /// ([`MsgFanOut`](crate::MsgFanOut)) drains the remainder with this.
    /// Suppliers that never carry such ports use the default, which yields
    /// none.
    fn try_next_output<WD>(&mut self) -> Option<(RingBuffer, WD)>
    where
        WD: WakeSource + Default + Clone + 'static,
    {
        None
    }

    /// Pop every producer ring wired to the next message input.
    ///
    /// An empty list is a legal, unconnected message input that reads
    /// nothing. Only [`MsgIn::bind`](crate::MsgIn) calls this; frame ports
    /// use [`next_input`](Self::next_input). Suppliers that never carry
    /// message ports use the default, which yields no producers.
    fn next_input_fanin<RD>(&mut self) -> Vec<(RingBuffer, RD)>
    where
        RD: WakeSink + Default + Clone + 'static,
    {
        Vec::new()
    }

    /// The registry over every registered buffer, for a bundle that wants
    /// by-id access to all outputs (a downlink, a logger, a recorder) rather
    /// than typed ports.
    ///
    /// A bundle pulls this in its `bind`, exactly where it pulls its typed
    /// ports; the registry is already complete by then, so the handle is
    /// usable immediately. Only the host [`Binder`] carries one, so a bundle
    /// that needs it can only be bound on the host; the default panics rather
    /// than fabricate an empty registry.
    fn registry(&self) -> Arc<Registry> {
        panic!("this ring source carries no registry (host-only capability)")
    }

    /// The bound system's instance name, stamped into its implicit log port's
    /// [`LogEvent`](crate::LogEvent)s as their `source`. Empty when the
    /// supplier carries none.
    fn instance_name(&self) -> &str {
        ""
    }
}

/// The ring sources a pack entry can be bound over, as one concrete type so
/// the type-erased entry constructor (a boxed closure) can take it by `&mut`
/// without giving up [`RingSource`]'s generic methods. The host [`Binder`] is
/// the static-path variant; a loaded entry binds over the
/// [`RawBinder`](crate::abi::RawBinder) cursor of host-provided ring handles.
pub enum AnySource<'a, 'b> {
    /// The host builder's positional cursor over pre-allocated rings.
    Host(&'a mut Binder<'b>),
    /// The `.so`-side cursor over the host's raw ring handles.
    Raw(&'a mut crate::abi::RawBinder<'b>),
}

impl RingSource for AnySource<'_, '_> {
    fn next_output<WD>(&mut self) -> (RingBuffer, WD)
    where
        WD: WakeSource + Default + Clone + 'static,
    {
        match self {
            Self::Host(b) => b.next_output::<WD>(),
            Self::Raw(b) => b.next_output::<WD>(),
        }
    }

    fn next_input<RD>(&mut self) -> (RingBuffer, RD)
    where
        RD: WakeSink + Default + Clone + 'static,
    {
        match self {
            Self::Host(b) => b.next_input::<RD>(),
            Self::Raw(b) => b.next_input::<RD>(),
        }
    }

    fn try_next_output<WD>(&mut self) -> Option<(RingBuffer, WD)>
    where
        WD: WakeSource + Default + Clone + 'static,
    {
        match self {
            Self::Host(b) => b.try_next_output::<WD>(),
            Self::Raw(b) => b.try_next_output::<WD>(),
        }
    }

    fn next_input_fanin<RD>(&mut self) -> Vec<(RingBuffer, RD)>
    where
        RD: WakeSink + Default + Clone + 'static,
    {
        match self {
            Self::Host(b) => b.next_input_fanin::<RD>(),
            Self::Raw(b) => b.next_input_fanin::<RD>(),
        }
    }

    fn registry(&self) -> Arc<Registry> {
        match self {
            Self::Host(b) => b.registry(),
            // A loaded entry can never hold a broad-access capability; the
            // loader rejects them, so this is unreachable in practice.
            Self::Raw(_) => panic!("a loaded pack entry carries no registry"),
        }
    }

    fn instance_name(&self) -> &str {
        match self {
            Self::Host(b) => b.instance_name(),
            Self::Raw(b) => b.instance_name(),
        }
    }
}

impl<'a> RingSource for Binder<'a> {
    fn next_output<WD>(&mut self) -> (RingBuffer, WD)
    where
        WD: WakeSource + Default + Clone + 'static,
    {
        let p = self
            .outputs
            .next()
            .expect("bind() walks output ports in descriptors() order");
        (p.ring.clone(), BoundPort::wake(&p.data))
    }

    fn try_next_output<WD>(&mut self) -> Option<(RingBuffer, WD)>
    where
        WD: WakeSource + Default + Clone + 'static,
    {
        let p = self.outputs.next()?;
        Some((p.ring.clone(), BoundPort::wake(&p.data)))
    }

    fn next_input<RD>(&mut self) -> (RingBuffer, RD)
    where
        RD: WakeSink + Default + Clone + 'static,
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
        (p.ring.clone(), BoundPort::wake(&p.data))
    }

    fn next_input_fanin<RD>(&mut self) -> Vec<(RingBuffer, RD)>
    where
        RD: WakeSink + Default + Clone + 'static,
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
            .map(|p| (p.ring.clone(), BoundPort::wake(&p.data)))
            .collect()
    }

    fn registry(&self) -> Arc<Registry> {
        self.registry.clone()
    }

    fn instance_name(&self) -> &str {
        self.instance
    }
}

/// A port bundle constructible from a [`RingSource`].
///
/// The `SystemInput`/`SystemOutput` derives (and the framework
/// [`Out`](crate::Out) wrapper) generate this symmetrically to
/// `descriptors()`, so one impl binds over any supplier.
pub trait BindPorts: Sized {
    /// Construct every port from the ring source, in `descriptors()` order.
    fn bind<S: RingSource>(src: &mut S) -> Self;
}
