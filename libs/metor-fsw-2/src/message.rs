//! Self-describing message records and the ports that carry them.
//!
//! A message is the second payload kind beside frames. Where a
//! [`Frame`](crate::Frame) is a fixed component group announced through a
//! vtable, a message record describes itself: the 2-byte [`Msg::ID`] followed
//! by the postcard-serialized payload, written verbatim onto a byte ring. The
//! id alone names the schema, so any consumer decodes a record from the bytes
//! in front of it. Message rings use log delivery, so readers drain each
//! producer's records in order instead of taking only the latest one. A full
//! ring makes [`emit`](MsgOut::emit) fail. [`publish`](MsgOut::publish) drops
//! the new record and counts the loss.
//!
//! [`MsgOut<M>`](MsgOut) and [`MsgIn<M>`](MsgIn) are the message twins of
//! [`Output<F>`](crate::Output) and [`Input<F>`](crate::Input). Each is typed
//! on a single [`Msg`] type `M`, and the wiring edge key is `M::ID`, exactly
//! parallel to a frame port keyed on its frame id. A channel that carries
//! several message types is simply several typed ports, possibly sharing a
//! ring; a consumer's [`drain`](MsgIn::drain) skips records whose id is not
//! `M::ID`. Message inputs also allow fan-in, so a [`MsgIn`] holds one
//! [`View`] per wired producer and drains them all.

use core::marker::PhantomData;

use metor_fsw_ring::{NoWake, View, WakeSink, WakeSource, WriteError, Writer};
use metor_proto::types::{Msg, PacketId};
use metor_proto_wkt::{
    AlarmAck, AlarmCleared, AlarmDef, AlarmDefs, AlarmRaised, LogEvent, ReloadSequences,
    SequenceChannelEvent, SequenceCommand, SequenceRegistry, WiringManifest,
};
use serde::de::DeserializeOwned;

use crate::binder::RingSource;
use crate::descriptor::PortDesc;

/// A message type a config file can select by a stable string token.
///
/// `NAME` is the token a config file writes to pick this message type and
/// the channel half of the registry key `<instance>.<NAME>`. It is declared
/// explicitly rather than derived from the Rust type name so that renaming
/// the type cannot silently change the wire token. Resolution from token to
/// type happens through a [`MsgTable`].
pub trait NamedMsg: Msg + postcard_schema::Schema {
    /// The stable wire, config, and registry token for this message type.
    const NAME: &'static str;
}

impl NamedMsg for SequenceCommand {
    const NAME: &'static str = "SequenceCommand";
}
impl NamedMsg for SequenceRegistry {
    const NAME: &'static str = "SequenceRegistry";
}
impl NamedMsg for SequenceChannelEvent {
    const NAME: &'static str = "SequenceChannelEvent";
}
impl NamedMsg for ReloadSequences {
    const NAME: &'static str = "ReloadSequences";
}
impl NamedMsg for AlarmDef {
    const NAME: &'static str = "AlarmDef";
}
impl NamedMsg for AlarmDefs {
    const NAME: &'static str = "AlarmDefs";
}
impl NamedMsg for AlarmRaised {
    const NAME: &'static str = "AlarmRaised";
}
impl NamedMsg for AlarmCleared {
    const NAME: &'static str = "AlarmCleared";
}
impl NamedMsg for AlarmAck {
    const NAME: &'static str = "AlarmAck";
}
impl NamedMsg for WiringManifest {
    const NAME: &'static str = "WiringManifest";
}
impl NamedMsg for LogEvent {
    const NAME: &'static str = "LogEvent";
}

/// A lookup from message name tokens to packet ids.
///
/// Config tokens resolve against this table, and every registry's table
/// starts with the well-known sequence and alarm messages whose [`NamedMsg`]
/// impls live in this module. The stored name is `&'static`, so a resolved
/// entry doubles as a minted port's [`PortDesc`] name without leaking a
/// string.
#[derive(Default, Clone)]
pub struct MsgTable {
    map: std::collections::HashMap<&'static str, PacketId>,
}

impl MsgTable {
    /// Register `M` under [`NamedMsg::NAME`]. Idempotent.
    pub fn insert<M: NamedMsg>(&mut self) {
        self.map.insert(M::NAME, M::ID);
    }

    /// Resolve a token to its `(name, id)` pair. The returned name is the
    /// stored `&'static` token, not the argument, so it can name a port.
    pub fn get(&self, name: &str) -> Option<(&'static str, PacketId)> {
        self.map.get_key_value(name).map(|(n, id)| (*n, *id))
    }

    /// Every registered name, sorted, for error listings.
    pub fn names(&self) -> Vec<&'static str> {
        let mut names: Vec<_> = self.map.keys().copied().collect();
        names.sort_unstable();
        names
    }
}

/// Default worst-case message payload size, used to size a message ring the
/// way [`Frame::MAX_SIZE`](crate::Frame) sizes a frame ring.
pub const MAX_MSG_BYTES: usize = 4096;

/// Default in-flight record depth for a [`Delivery::Log`](crate::Delivery)
/// ring. Deep, because a log ring must hold every record its slowest reader
/// has not yet seen, unlike a one-deep snapshot. Size a ring by hand via
/// [`capacity_for`](crate::capacity_for).
pub(crate) const LOG_DEPTH: usize = 64;

/// Split a raw message record into its `(id, payload)` halves, the inverse
/// of an [`emit`](MsgOut::emit). Returns `None` if the record is too short
/// to carry the 2-byte id.
pub fn split_record(rec: &[u8]) -> Option<(PacketId, &[u8])> {
    let (id, payload) = rec.split_first_chunk::<2>()?;
    Some((*id, payload))
}

// ---------------------------------------------------------------------------
// MsgOut
// ---------------------------------------------------------------------------

/// A [`Writer`] that serializes values of a single [`Msg`] type `M` onto a
/// byte ring, one self-describing record per emit.
///
/// The message twin of [`Output<F>`](crate::Output), with the same
/// single-writer discipline and the same `descriptor()`/`bind()` port
/// contract, so it sits in a `SystemOutput` bundle beside frame ports. The
/// wiring edge key is `M::ID`.
pub struct MsgOut<M, WD = NoWake>
where
    WD: WakeSource,
{
    writer: Writer<WD>,
    /// Reused record buffer (the 2-byte id prefix plus the serialized
    /// payload), so a per-cycle emit grows in place instead of allocating.
    scratch: Vec<u8>,
    /// Records dropped by the infallible [`publish`](Self::publish) path,
    /// folded into health by the runner via `SystemOutput::take_dropped` or a
    /// future-owning driver's shared cell.
    dropped: crate::port::Drops,
    _m: PhantomData<fn() -> M>,
}

impl<M: Msg, WD: WakeSource> MsgOut<M, WD> {
    /// Wrap a writer over a log-sized byte ring.
    pub fn new(writer: Writer<WD>) -> Self {
        Self {
            writer,
            scratch: Vec::new(),
            dropped: crate::port::Drops::Local(0),
            _m: PhantomData,
        }
    }

    /// Emit one message, counting a failed write instead of returning it.
    ///
    /// A write fails on `InsufficientCapacity` (a sizing bug) or `WouldBlock`
    /// (a slow reader backpressuring the ring); either way the record is
    /// dropped and counted for the runner to fold into health. Callers that
    /// want to see the error use [`emit`](Self::emit).
    pub fn publish(&mut self, msg: &M) {
        if self.emit(msg).is_err() {
            self.dropped.bump();
        }
    }

    /// Take and clear the [`publish`](Self::publish) drop counter.
    pub fn take_dropped(&mut self) -> u64 {
        self.dropped.take_local()
    }

    /// Count drops through `cell` instead of locally, for a port about to
    /// move into a future (whose driver folds the cell into health).
    pub(crate) fn share_drops(&mut self, cell: std::sync::Arc<core::sync::atomic::AtomicU64>) {
        self.dropped.share(cell);
    }

    /// Emit one message as a single ring record, the 2-byte [`Msg::ID`]
    /// followed by the postcard payload. Serialization into the reused
    /// scratch buffer cannot fail, so the only error is the ring write's
    /// [`WriteError`].
    pub fn emit(&mut self, msg: &M) -> Result<(), WriteError> {
        self.scratch.clear();
        self.scratch.extend_from_slice(&M::ID);
        postcard::serialize_with_flavor(msg, ScratchFlavor(&mut self.scratch))
            .expect("postcard serialization into an in-memory buffer is infallible");
        self.writer.try_write(&self.scratch)
    }
}

impl<M: NamedMsg, WD: WakeSource> MsgOut<M, WD> {
    /// This port's static descriptor, keyed on `M::ID` and named by
    /// [`NamedMsg::NAME`].
    pub fn descriptor() -> PortDesc {
        PortDesc::msg::<M>()
    }

    /// The port declaration this field contributes to a bundle's `decls`
    /// walk, an ordinary wired port.
    pub fn decl() -> crate::PortDesc {
        Self::descriptor()
    }
}

impl<M, WD> MsgOut<M, WD>
where
    M: Msg,
    WD: WakeSource + Default + Clone + 'static,
{
    /// Bind this port over the next ring the [`RingSource`] hands out, taking
    /// the matched writer-side data-wake endpoint. Called by the derive for a
    /// `MsgOut<M>` field in a `SystemOutput` bundle.
    pub fn bind<S: RingSource>(src: &mut S) -> Self {
        let (ring, data) = src.next_output::<WD>();
        let writer = ring
            .writer(data)
            .expect("message ring is bound to exactly one writer at build");
        MsgOut::new(writer)
    }
}

/// A [`MsgOut`] whose port is marked untelemetered, so telemetry taps never
/// echo inbound commands back out.
///
/// This is an alias, not a distinct type; the flag lives on the descriptor.
/// The `SystemInput`/`SystemOutput` derives and `#[system]` recognize the
/// `CommandOut` token and lower it to a `MsgOut` whose [`PortDesc`] is
/// [`untelemetered`](PortDesc::untelemetered), identical to spelling
/// `#[fsw(telemetered = false)]` on a `MsgOut` field. A hand-written
/// `SystemOutput` impl must build that descriptor itself
/// (`PortDesc::msg::<M>().untelemetered()`), because `CommandOut::descriptor()`
/// resolves through the alias to the plain, telemetered `MsgOut` descriptor.
pub type CommandOut<M, WD = NoWake> = MsgOut<M, WD>;

// ---------------------------------------------------------------------------
// MsgFanOut
// ---------------------------------------------------------------------------

/// A bank of raw ring writers, one per message output minted from an
/// instance's config rather than declared in its bundle type.
///
/// The dynamic-count sibling of [`MsgOut`]. The minted
/// ports must be the trailing outputs of the instance descriptor, because
/// [`bind`](Self::bind) drains every ring the [`RingSource`] still holds; a
/// bundle binds its static ports first and the fan-out last. The writer at
/// index k is the descriptor's k-th minted port. The owner keys both off the
/// same config list, so no per-writer id is stored here.
///
/// [`write_raw`](Self::write_raw) is the id-erased [`emit`](MsgOut::emit).
/// It writes an already-encoded `id ++ payload` record verbatim, with no
/// typed `M` and no postcard step; validation happens where a typed consumer
/// [`drain`](MsgIn::drain)s the record.
pub struct MsgFanOut {
    writers: Vec<Writer<NoWake>>,
    /// Reused record buffer, as in [`MsgOut`].
    scratch: Vec<u8>,
}

impl MsgFanOut {
    /// Bind one writer per output ring the source still holds. Must run after
    /// the bundle's static ports have bound, in `descriptors()` order.
    pub fn bind<S: RingSource>(src: &mut S) -> Self {
        let mut writers = Vec::new();
        while let Some((ring, data)) = src.try_next_output::<NoWake>() {
            let writer = ring
                .writer(data)
                .expect("message ring is bound to exactly one writer at build");
            writers.push(writer);
        }
        Self {
            writers,
            scratch: Vec::new(),
        }
    }

    /// The bound writer count, one per port the owner's config minted.
    pub fn len(&self) -> usize {
        self.writers.len()
    }

    /// Whether the config minted no ports.
    pub fn is_empty(&self) -> bool {
        self.writers.is_empty()
    }

    /// Write one already-encoded record, `id` then `payload` verbatim, onto
    /// writer `idx`. Only the ring write can fail, on backpressure or an
    /// oversized record.
    pub fn write_raw(
        &mut self,
        idx: usize,
        id: PacketId,
        payload: &[u8],
    ) -> Result<(), WriteError> {
        self.scratch.clear();
        self.scratch.extend_from_slice(&id);
        self.scratch.extend_from_slice(payload);
        self.writers[idx].try_write(&self.scratch)
    }
}

// ---------------------------------------------------------------------------
// MsgIn
// ---------------------------------------------------------------------------

/// The consuming half of a message channel, decoding committed ring records
/// into values of the [`Msg`] type `M`.
///
/// [`drain`](Self::drain) reads each committed record, keeps those whose id
/// is `M::ID`, and postcard-decodes the payload, so records of other message
/// types on a shared channel are skipped. A lapped view resyncs to the live
/// edge.
///
/// Message inputs allow fan-in, so the port holds one [`View`] per wired
/// producer and [`drain`](Self::drain) drains them all. One view is the
/// common single-producer case, and an empty list is a legal unconnected
/// input.
pub struct MsgIn<M, RD = NoWake>
where
    RD: WakeSink,
{
    views: Vec<View<RD>>,
    _marker: PhantomData<fn() -> M>,
}

impl<M, RD> MsgIn<M, RD>
where
    M: Msg + DeserializeOwned,
    RD: WakeSink,
{
    /// Wrap a single producer [`View`].
    pub fn new(view: View<RD>) -> Self {
        Self::from_views(vec![view])
    }

    /// Wrap one [`View`] per wired producer, the fan-in case.
    pub fn from_views(views: Vec<View<RD>>) -> Self {
        Self {
            views,
            _marker: PhantomData,
        }
    }

    /// Drain every record committed since the last call across all producer
    /// views, handing each decoded `M` to `f`.
    ///
    /// Per-producer order is preserved; the interleave across producers is
    /// arbitrary, and irrelevant since each record describes itself. Records
    /// with a different id and records that fail to decode are skipped. Each
    /// record is borrowed in place and freed for the writer as soon as `f`
    /// returns. Ring corruption is returned to the caller.
    pub fn drain(&mut self, mut f: impl FnMut(M)) -> Result<(), crate::ReadError> {
        for view in &mut self.views {
            crate::port::drain_view(view, |rec| {
                if let Some((id, payload)) = split_record(rec)
                    && id == M::ID
                    && let Ok(msg) = postcard::from_bytes::<M>(payload)
                {
                    f(msg);
                }
            })?;
        }
        Ok(())
    }
}

impl<M, RD> MsgIn<M, RD>
where
    M: NamedMsg + DeserializeOwned,
    RD: WakeSink,
{
    /// This port's static descriptor, sharing its edge key with the
    /// [`MsgOut<M>`](MsgOut) it consumes.
    pub fn descriptor() -> PortDesc {
        PortDesc::msg::<M>()
    }

    /// The port declaration this field contributes to a bundle's `decls`
    /// walk, an ordinary wired port.
    pub fn decl() -> crate::PortDesc {
        Self::descriptor()
    }
}

impl<M, RD> MsgIn<M, RD>
where
    M: Msg + DeserializeOwned,
    RD: WakeSink + Default + Clone + 'static,
{
    /// Bind this port over every producer ring wired to the next message
    /// input, claiming the whole fan-in list
    /// ([`next_input_fanin`](RingSource::next_input_fanin)). An empty list is
    /// a legal, unconnected input that drains nothing. Called by the derive
    /// for a `MsgIn<M>` field in a `SystemInput` bundle.
    pub fn bind<S: RingSource>(src: &mut S) -> Self {
        let rings = src.next_input_fanin::<RD>();
        let views = rings
            .into_iter()
            .map(|(ring, data)| {
                ring.view(data)
                    .expect("message input reader slot (sized at build)")
            })
            .collect();
        Self::from_views(views)
    }
}

/// A postcard serialization flavor that appends into a borrowed, reused
/// buffer, so an [`emit`](MsgOut::emit) grows its scratch in place instead
/// of allocating.
struct ScratchFlavor<'a>(&'a mut Vec<u8>);

impl postcard::ser_flavors::Flavor for ScratchFlavor<'_> {
    type Output = ();

    fn try_push(&mut self, data: u8) -> postcard::Result<()> {
        self.0.push(data);
        Ok(())
    }

    fn try_extend(&mut self, b: &[u8]) -> postcard::Result<()> {
        self.0.extend_from_slice(b);
        Ok(())
    }

    fn finalize(self) -> postcard::Result<Self::Output> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use metor_fsw_ring::{Config, NoWake, RingBuffer};
    use metor_proto::types::Msg;
    use metor_proto_wkt::{
        SequenceChannelSpec, SequenceCommand, SequenceCommandKind, SequenceRegistry,
    };

    use super::{LOG_DEPTH, MAX_MSG_BYTES, MsgFanOut, MsgIn, MsgOut, split_record};
    use crate::PortId;
    use crate::port::capacity_for;
    use crate::registry::RegistryEntry;

    /// Two typed ports emit distinct message types onto one ring; a raw view
    /// reads the records back, `split_record` recovers each id, and the
    /// payloads postcard-round-trip. Also checks `MsgOut::descriptor`.
    #[test]
    fn msg_out_emits_and_round_trips() {
        let ring = RingBuffer::create_in_memory(Config {
            capacity: capacity_for(MAX_MSG_BYTES, LOG_DEPTH),
            max_readers: 4,
        });
        let entry = RegistryEntry {
            key: metor_proto::types::ComponentId::new("coordinator.sequences"),
            instance: std::sync::Arc::from("coordinator"),
            desc: crate::PortDesc::msg_named::<SequenceRegistry>("sequences"),
            ring: ring.clone(),
        };

        // The descriptor carries the port's edge key.
        assert_eq!(
            MsgOut::<SequenceCommand>::descriptor().id(),
            PortId::Packet(SequenceCommand::ID)
        );

        // Claim the view before any write; a view starts at the live edge,
        // so it must exist before data flows.
        let mut view = entry.view().expect("reader slot");

        // One typed port per Msg type, each keyed on its own `M::ID`. The
        // ring enforces a single live writer, so the ports take it in turn
        // (drop frees the claim). First record: a whole-registry declaration.
        let registry = SequenceRegistry {
            channels: vec![SequenceChannelSpec {
                name: "mode".to_string(),
                available: vec!["commissioning".to_string(), "safe_mode".to_string()],
            }],
        };
        {
            let mut reg_out: MsgOut<SequenceRegistry> =
                MsgOut::new(ring.writer(NoWake).expect("first writer"));
            reg_out.emit(&registry).expect("emit registry");
        }

        // Second record: a different Msg type, its own typed port on the
        // same ring.
        let mut cmd_out: MsgOut<SequenceCommand> =
            MsgOut::new(ring.writer(NoWake).expect("claim freed on drop"));
        let command = SequenceCommand {
            channel: "mode".to_string(),
            command: SequenceCommandKind::Start,
        };
        cmd_out.emit(&command).expect("emit command");

        let mut buf = Vec::new();

        // Record 1, the SequenceRegistry.
        assert!(view.try_read_into(&mut buf).expect("read record 1"));
        let (id, payload) = split_record(&buf).expect("split record 1");
        assert_eq!(id, SequenceRegistry::ID);
        let decoded: SequenceRegistry = postcard::from_bytes(payload).expect("round-trip registry");
        assert_eq!(decoded.channels.len(), 1);
        assert_eq!(decoded.channels[0].name, "mode");
        assert_eq!(
            decoded.channels[0].available,
            vec!["commissioning", "safe_mode"]
        );

        // Record 2, the SequenceCommand, a distinct id from the same writer.
        assert!(view.try_read_into(&mut buf).expect("read record 2"));
        let (id, payload) = split_record(&buf).expect("split record 2");
        assert_eq!(id, SequenceCommand::ID);
        assert_ne!(SequenceCommand::ID, SequenceRegistry::ID);
        let decoded: SequenceCommand = postcard::from_bytes(payload).expect("round-trip command");
        assert_eq!(decoded.channel, "mode");
        assert!(matches!(decoded.command, SequenceCommandKind::Start));

        // No third record.
        assert!(!view.try_read_into(&mut buf).expect("no more records"));
    }

    /// A `MsgIn` drains a shared channel, decoding only the records whose id
    /// matches its type. Emit a `SequenceRegistry` then two `SequenceCommand`s;
    /// a `MsgIn<SequenceCommand>` yields exactly the two commands, in order.
    #[test]
    fn msg_in_drains_and_id_filters() {
        let ring = RingBuffer::create_in_memory(Config {
            capacity: capacity_for(MAX_MSG_BYTES, LOG_DEPTH),
            max_readers: 4,
        });
        // Claim the read view before any write (a view starts at the live edge).
        let mut inbox: MsgIn<SequenceCommand> = MsgIn::new(ring.view(NoWake).expect("slot"));

        // Two typed ports take the ring's single writer in turn (drop frees
        // the claim), so the drained view sees a mixed stream. A different
        // Msg type ahead of the commands must be skipped.
        {
            let mut reg_out: MsgOut<SequenceRegistry> =
                MsgOut::new(ring.writer(NoWake).expect("first writer"));
            reg_out
                .emit(&SequenceRegistry {
                    channels: vec![SequenceChannelSpec {
                        name: "mode".to_string(),
                        available: vec![],
                    }],
                })
                .expect("emit registry");
        }
        let mut cmd_out: MsgOut<SequenceCommand> =
            MsgOut::new(ring.writer(NoWake).expect("claim freed on drop"));
        cmd_out
            .emit(&SequenceCommand {
                channel: "adcs".to_string(),
                command: SequenceCommandKind::Start,
            })
            .expect("emit start");
        cmd_out
            .emit(&SequenceCommand {
                channel: "recovery".to_string(),
                command: SequenceCommandKind::Stop,
            })
            .expect("emit stop");

        let mut got: Vec<SequenceCommand> = Vec::new();
        inbox.drain(|c| got.push(c)).unwrap();
        assert_eq!(got.len(), 2, "the SequenceRegistry record is filtered out");
        assert_eq!(got[0].channel, "adcs");
        assert!(matches!(got[0].command, SequenceCommandKind::Start));
        assert_eq!(got[1].channel, "recovery");
        assert!(matches!(got[1].command, SequenceCommandKind::Stop));

        // Drained dry, so a second drain yields nothing.
        let mut again = 0;
        inbox.drain(|_| again += 1).unwrap();
        assert_eq!(again, 0);
    }

    /// `MsgFanOut::write_raw` relays an already-encoded `(id ++ payload)`
    /// record verbatim; a typed `MsgIn` over the same ring decodes it exactly
    /// as if a `MsgOut<M>` had emitted it.
    #[test]
    fn msg_fan_out_write_raw_round_trips() {
        let ring = RingBuffer::create_in_memory(Config {
            capacity: capacity_for(MAX_MSG_BYTES, LOG_DEPTH),
            max_readers: 4,
        });
        let mut inbox: MsgIn<SequenceCommand> = MsgIn::new(ring.view(NoWake).expect("slot"));
        let mut fan = MsgFanOut {
            writers: vec![ring.writer(NoWake).expect("one writer")],
            scratch: Vec::new(),
        };
        assert_eq!(fan.len(), 1);

        let payload = postcard::to_allocvec(&SequenceCommand {
            channel: "adcs".to_string(),
            command: SequenceCommandKind::Start,
        })
        .expect("encode");
        fan.write_raw(0, SequenceCommand::ID, &payload)
            .expect("relay");

        let mut got: Vec<SequenceCommand> = Vec::new();
        inbox.drain(|c| got.push(c)).unwrap();
        assert_eq!(
            got.len(),
            1,
            "the relayed record decodes at the typed consumer"
        );
        assert_eq!(got[0].channel, "adcs");
        assert!(matches!(got[0].command, SequenceCommandKind::Start));
    }

    /// A fan-in `MsgIn` (`from_views`) drains every producer view: two
    /// emitters on two rings, one input over both views, yields all records.
    /// An empty fan-in drains nothing.
    #[test]
    fn msg_in_fans_in_multiple_views() {
        let mk = || {
            RingBuffer::create_in_memory(Config {
                capacity: capacity_for(MAX_MSG_BYTES, LOG_DEPTH),
                max_readers: 4,
            })
        };
        let ring_a = mk();
        let ring_b = mk();
        let mut out_a: MsgOut<SequenceCommand> =
            MsgOut::new(ring_a.writer(NoWake).expect("one writer per ring"));
        let mut out_b: MsgOut<SequenceCommand> =
            MsgOut::new(ring_b.writer(NoWake).expect("one writer per ring"));
        // Two producer views into one input (the fan-in shape).
        let mut inbox: MsgIn<SequenceCommand> = MsgIn::from_views(vec![
            ring_a.view(NoWake).expect("slot a"),
            ring_b.view(NoWake).expect("slot b"),
        ]);

        out_a
            .emit(&SequenceCommand {
                channel: "adcs".to_string(),
                command: SequenceCommandKind::Start,
            })
            .expect("emit a");
        out_b
            .emit(&SequenceCommand {
                channel: "recovery".to_string(),
                command: SequenceCommandKind::Stop,
            })
            .expect("emit b");

        let mut got: Vec<String> = Vec::new();
        inbox.drain(|c| got.push(c.channel)).unwrap();
        got.sort_unstable();
        assert_eq!(
            got,
            vec!["adcs", "recovery"],
            "both producers' records are drained"
        );

        // An empty fan-in (unconnected input) drains nothing.
        let mut empty: MsgIn<SequenceCommand> = MsgIn::from_views(vec![]);
        let mut n = 0;
        empty.drain(|_| n += 1).unwrap();
        assert_eq!(n, 0);
    }

    /// `split_record` rejects a record too short to carry a 2-byte id.
    #[test]
    fn split_record_rejects_short() {
        assert!(split_record(&[]).is_none());
        assert!(split_record(&[7]).is_none());
        assert_eq!(split_record(&[1, 2]), Some(([1, 2], &[][..])));
        assert_eq!(split_record(&[1, 2, 9]), Some(([1, 2], &[9][..])));
    }
}
