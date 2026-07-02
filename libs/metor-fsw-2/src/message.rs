//! The general **message channel** — the second payload kind beside frames
//! (`docs/messages.md` §1, §2).
//!
//! Where a [`Frame`](crate::Frame) is a fixed, vtable-announced component group, a
//! **message** is a self-describing `(PacketId, postcard-bytes)` record: the 2-byte
//! [`Msg::ID`] followed by the postcard-serialized payload, written verbatim onto a
//! byte ring. There is no announce and no vtable — the id *is* the schema identity, so
//! any consumer (the telemetry downlink, a recorder, the panel's sequence view)
//! decodes a record with nothing but the id. Messages are an event/command **log**:
//! every record is preserved in order, never coalesced like a latest-wins snapshot.
//!
//! [`MsgOut<M>`](MsgOut) is the emit port — the message twin of [`Output<F>`](crate::Output),
//! **typed on one [`Msg`] type `M`** so it can be a first-class wired port
//! (`docs/message-wiring.md` §2.1): the edge key is `M::ID`, exactly parallel to a frame
//! port's `F::FRAME_ID`. A channel that carries several `Msg` types becomes several ports.

use core::marker::PhantomData;

use metor_fsw_ring::{
    Backing, BoxBacking, NoWake, View, WakeSink, WakeSource, WriteError, Writer, frame_len,
};
use metor_proto::types::{Msg, PacketId};
use serde::de::DeserializeOwned;

use crate::binder::RingSource;
use crate::descriptor::PortDesc;

/// Default worst-case message payload size, the [`Frame::MAX_SIZE`](crate::Frame)
/// analogue for the variable-record ring sizing (`docs/messages.md` §2.1). Generous —
/// a message is a log entry (a `SequenceChannelEvent`, a `SequenceRegistry`), not a
/// telemetry snapshot.
pub const MAX_MSG_BYTES: usize = 4096;

/// Default in-flight record depth for a message ring (`docs/messages.md` §2.1). Deep
/// because messages are an every-record event/command log — a slow tap must not drop a
/// transition — not a one-deep snapshot like a component output.
pub const MSG_DEPTH: usize = 64;

/// Power-of-two ring capacity for `depth` message records each up to `max_msg_bytes`
/// payload bytes — the [`capacity_for`](crate::capacity_for) analogue for the variable
/// `(id, postcard)` record (`docs/messages.md` §2.1). `frame_len` adds the per-record
/// header + payload padding the ring stores around each write.
pub fn msg_capacity(max_msg_bytes: usize, depth: usize) -> usize {
    (frame_len(max_msg_bytes) * depth.max(2)).next_power_of_two()
}

/// Split a raw message record back into its `(id, payload)` halves — the inverse of an
/// [`emit`](MsgOut::emit) (the downlink tap's decode, W2). The first 2 bytes are the
/// [`Msg::ID`]; the rest is the postcard payload. `None` if the record is too short to
/// carry an id.
pub fn split_record(rec: &[u8]) -> Option<(PacketId, &[u8])> {
    if rec.len() < 2 {
        return None;
    }
    let id = [rec[0], rec[1]];
    Some((id, &rec[2..]))
}

// ---------------------------------------------------------------------------
// MsgOut
// ---------------------------------------------------------------------------

/// One owned **message** output: the single [`Writer`] into a byte ring carrying
/// `(id, postcard)` records, **typed on one [`Msg`] type `M`** (`docs/message-wiring.md`
/// §2.1). The message twin of [`Output<F>`](crate::Output) — same single-writer discipline,
/// same `BoxBacking`/`NoWake` cyclic default, and the same `descriptor()`/`bind()` port
/// contract, so it drops into a `SystemOutput` bundle beside frame ports with no macro
/// change. The edge key is `M::ID`.
pub struct MsgOut<M, B = BoxBacking, WD = NoWake, WS = NoWake>
where
    B: Backing,
    WD: WakeSource,
    WS: WakeSink,
{
    writer: Writer<B, WD, WS>,
    /// A reused record buffer (the 2-byte id prefix + the serialized payload), so a
    /// per-cycle emit grows in place rather than allocating a fresh `Vec` every call —
    /// exactly the [`Output::write_with`](crate::Output) scratch discipline.
    scratch: Vec<u8>,
    _m: PhantomData<fn() -> M>,
}

impl<M: Msg, B: Backing, WD: WakeSource, WS: WakeSink> MsgOut<M, B, WD, WS> {
    /// Wrap a writer the coordinator created over a [`msg_capacity`]-sized byte ring.
    pub fn new(writer: Writer<B, WD, WS>) -> Self {
        Self {
            writer,
            scratch: Vec::new(),
            _m: PhantomData,
        }
    }

    /// Emit one message: write the 2-byte [`Msg::ID`] then the postcard-serialized
    /// payload as a single ring record (`docs/messages.md` §1.2). Serialization runs
    /// into the reused `scratch` via the same `serialize_with_flavor` path the
    /// `IntoLenPacket` impl uses (`../metor-proto/src/types.rs:620`); only the ring
    /// `try_write` can fail, so this surfaces a [`WriteError`] and nothing else.
    pub fn emit(&mut self, msg: &M) -> Result<(), WriteError> {
        self.scratch.clear();
        self.scratch.extend_from_slice(&M::ID);
        postcard::serialize_with_flavor(msg, ScratchFlavor(&mut self.scratch))
            .expect("postcard serialization into an in-memory buffer is infallible");
        self.writer.try_write(&self.scratch)
    }

    /// This port's static descriptor — the message twin of
    /// [`Output::<F>::descriptor`](crate::Output) (`docs/message-wiring.md` §2.1).
    pub fn descriptor() -> PortDesc {
        PortDesc::msg::<M>()
    }
}

impl<M, B, WD, WS> MsgOut<M, B, WD, WS>
where
    M: Msg,
    B: Backing,
    WD: WakeSource + Default + Clone + 'static,
    WS: WakeSink + Default + Clone + 'static,
{
    /// Bind this port over the next ring the [`RingSource`] hands out, taking the matched
    /// writer-side wake endpoints — the [`Output::bind`](crate::Output) mirror. Walked by
    /// the derive when a `MsgOut<M>` is declared in a `SystemOutput` bundle.
    pub fn bind<S: RingSource<B = B>>(src: &mut S) -> Self {
        let (ring, data, space) = src.next_output::<WD, WS>();
        // Invariant: the coordinator allocates one ring per message output and
        // binds it exactly once, so the region's writer claim is always free here.
        let writer = ring
            .writer(data, space)
            .expect("message ring is bound to exactly one writer at build");
        MsgOut::new(writer)
    }
}

/// A **command-channel** message output: a [`MsgOut<M>`](MsgOut) whose descriptor marks the
/// channel **untelemetered** (`docs/message-wiring.md` §6.4), so the downlink / `AllOutputs`
/// never echo inbound commands back to the panel. Emit through it exactly like a `MsgOut<M>`
/// (it [`Deref`]s to one); only the port's telemetry flag differs. This is the opt-out spelling
/// the (type-blind) `SystemOutput` derive picks up for free via `descriptor()`/`bind()`.
pub struct CommandOut<M, B = BoxBacking, WD = NoWake, WS = NoWake>
where
    B: Backing,
    WD: WakeSource,
    WS: WakeSink,
{
    inner: MsgOut<M, B, WD, WS>,
}

impl<M: Msg, B: Backing, WD: WakeSource, WS: WakeSink> CommandOut<M, B, WD, WS> {
    /// Wrap a writer the coordinator created over a command message ring.
    pub fn new(writer: Writer<B, WD, WS>) -> Self {
        Self {
            inner: MsgOut::new(writer),
        }
    }

    /// The port descriptor — an **untelemetered** message port keyed on `M::ID`.
    pub fn descriptor() -> PortDesc {
        PortDesc::msg_untelemetered::<M>()
    }
}

impl<M, B, WD, WS> CommandOut<M, B, WD, WS>
where
    M: Msg,
    B: Backing,
    WD: WakeSource + Default + Clone + 'static,
    WS: WakeSink + Default + Clone + 'static,
{
    /// Bind this command output over the next ring the [`RingSource`] hands out — the
    /// [`MsgOut::bind`] wrapper. Walked by the derive like any output port.
    pub fn bind<S: RingSource<B = B>>(src: &mut S) -> Self {
        Self {
            inner: MsgOut::bind(src),
        }
    }
}

impl<M, B: Backing, WD: WakeSource, WS: WakeSink> core::ops::Deref for CommandOut<M, B, WD, WS> {
    type Target = MsgOut<M, B, WD, WS>;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<M, B: Backing, WD: WakeSource, WS: WakeSink> core::ops::DerefMut for CommandOut<M, B, WD, WS> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

// ---------------------------------------------------------------------------
// MsgIn
// ---------------------------------------------------------------------------

/// One owned **message** input: a [`View`] over a byte ring carrying `(id, postcard)`
/// records, decoding each into the [`Msg`] type `M` — the in-FSW subscribe twin of
/// [`MsgOut`] (`docs/messages.md` §4). Where [`emit`](MsgOut::emit) writes the 2-byte
/// [`Msg::ID`] then the postcard payload, [`drain`](Self::drain) reads each committed
/// record, keeps only those whose id is `M::ID`, and postcard-decodes the payload —
/// reusing [`split_record`] (the downlink tap's decode). A record of another `Msg` type
/// on a heterogeneous channel is skipped, and a lapped view resyncs to the live edge (the
/// in-FSW analogue of the downlink message tap's `resync`, `src/telemetry/mod.rs`).
///
/// A message input may have **several producers** (`docs/message-wiring.md` §3.2/§3.3):
/// fan-in is legal (many emitters → one command consumer), so the port holds **K views**,
/// one per wired producer edge, and [`drain`](Self::drain) drains them all. A cyclic
/// consumer holds the producer views directly (generalizing the coordinator's
/// `command_sources: Vec<MsgIn>`); an async consumer holds one view over a private merge
/// ring (WP4). K = 1 is the common single-producer case; K = 0 is a legal unconnected input.
pub struct MsgIn<M, B = BoxBacking, RD = NoWake, RS = NoWake>
where
    B: Backing,
    RD: WakeSink,
    RS: WakeSource,
{
    views: Vec<View<B, RD, RS>>,
    /// A reused record buffer, so a per-cycle drain grows in place rather than allocating —
    /// the [`MsgOut`] `scratch` mirror.
    scratch: Vec<u8>,
    _marker: PhantomData<fn() -> M>,
}

impl<M, B, RD, RS> MsgIn<M, B, RD, RS>
where
    M: Msg + DeserializeOwned,
    B: Backing,
    RD: WakeSink,
    RS: WakeSource,
{
    /// Wrap a single [`View`] the coordinator/registry claimed over a [`msg_capacity`]-sized
    /// byte ring (the message twin of [`Input::new`](crate::Input), the K = 1 case).
    pub fn new(view: View<B, RD, RS>) -> Self {
        Self::from_views(vec![view])
    }

    /// Wrap K producer [`View`]s (the fan-in case — many emitters into one input).
    pub fn from_views(views: Vec<View<B, RD, RS>>) -> Self {
        Self {
            views,
            scratch: Vec::new(),
            _marker: PhantomData,
        }
    }

    /// This port's static descriptor — same edge key as the [`MsgOut<M>`](MsgOut) it consumes.
    pub fn descriptor() -> PortDesc {
        PortDesc::msg::<M>()
    }

    /// Message inputs are a **best-effort log**: a lapped view resyncs to the live edge inside
    /// [`drain`](Self::drain) rather than hard-stopping the consumer (unlike a frame
    /// [`Input`](crate::Input), whose lap is a fatal `LappedInput`). So a message input never
    /// reports lapped to the framework's per-cycle check — it always returns `false`.
    pub fn is_lapped(&self) -> bool {
        false
    }

    /// Drain every record committed since the last call across **all** producer views,
    /// decoding each `M::ID` record and handing the decoded payload to `f`. Per-producer
    /// order is preserved; the cross-producer interleave is arbitrary (and irrelevant — each
    /// record self-addresses). Records of a different id (a heterogeneous channel) and records
    /// that fail to postcard-decode are skipped. A lapped view resyncs to the live edge and
    /// that view's drain stops for this pass (best-effort, like the message-log downlink —
    /// `docs/messages.md` §3).
    pub fn drain(&mut self, mut f: impl FnMut(M)) {
        for view in &mut self.views {
            loop {
                match view.try_read_into(&mut self.scratch) {
                    Ok(true) => {
                        if let Some((id, payload)) = split_record(&self.scratch)
                            && id == M::ID
                            && let Ok(msg) = postcard::from_bytes::<M>(payload)
                        {
                            f(msg);
                        }
                    }
                    Ok(false) => break,
                    Err(_) => {
                        view.resync();
                        break;
                    }
                }
            }
        }
    }
}

impl<M, B, RD, RS> MsgIn<M, B, RD, RS>
where
    M: Msg + DeserializeOwned,
    B: Backing,
    RD: WakeSink + Default + Clone + 'static,
    RS: WakeSource + Default + Clone + 'static,
{
    /// Bind this port over **every** producer ring wired to the next message input — the
    /// [`Input::bind`](crate::Input) mirror, but claiming the fan-in list
    /// ([`next_input_fanin`](RingSource::next_input_fanin)). An empty list is a legal,
    /// unconnected message input (drains nothing). Walked by the derive when a `MsgIn<M>` is
    /// declared in a `SystemInput` bundle.
    pub fn bind<S: RingSource<B = B>>(src: &mut S) -> Self {
        let rings = src.next_input_fanin::<RD, RS>();
        let views = rings
            .into_iter()
            .map(|(ring, data, space)| {
                ring.view(data, space)
                    .expect("message input reader slot (sized at build)")
            })
            .collect();
        Self::from_views(views)
    }
}

/// A postcard serialization sink that appends into a borrowed, reused byte buffer — the
/// `&mut LenPacket` flavor's twin (`../metor-proto/src/types.rs:747`), so each
/// [`emit`](MsgOut::emit) grows `scratch` in place rather than allocating.
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
    use metor_fsw_ring::{Config, NoWake, Overrun, RingBuffer};
    use metor_proto::types::Msg;
    use metor_proto_wkt::{
        SequenceChannelSpec, SequenceCommand, SequenceCommandKind, SequenceRegistry,
    };

    use super::{MAX_MSG_BYTES, MSG_DEPTH, MsgIn, MsgOut, msg_capacity, split_record};
    use crate::PortId;
    use crate::registry::MessageEntry;

    /// A heterogeneous channel is now N typed ports (`docs/message-wiring.md` §2.1): mint a
    /// `MsgOut<SequenceRegistry>` and a `MsgOut<SequenceCommand>` over one ring, emit one of
    /// each, read them back via a `MessageEntry` `View`, and assert `split_record` recovers
    /// each id and the payloads postcard-round-trip. Also checks `MsgOut::descriptor`.
    #[test]
    fn msg_out_emits_and_round_trips() {
        let ring = RingBuffer::create_in_memory(Config {
            capacity: msg_capacity(MAX_MSG_BYTES, MSG_DEPTH),
            max_readers: 4,
            overrun: Overrun::Overwrite,
        });
        let entry = MessageEntry {
            key: metor_proto::types::ComponentId::new("coordinator.sequences"),
            instance: std::sync::Arc::from("coordinator"),
            channel: std::sync::Arc::from("sequences"),
            telemetered: true,
            ring: ring.clone(),
        };

        // The descriptor carries the port's edge key.
        assert_eq!(
            MsgOut::<SequenceCommand>::descriptor().id,
            PortId::Msg(SequenceCommand::ID)
        );

        // Claim the tap before any write — a view on an overwrite ring starts at the
        // live edge, so the tap must exist before data flows (the telemetry-tap order).
        let mut view = entry.view().expect("reader slot");

        // A typed port per Msg type (each keyed on its own `M::ID`). The ring enforces a
        // single live writer, so the ports take it in turn (drop frees the claim).
        // First record: a whole-registry declaration.
        let registry = SequenceRegistry {
            channels: vec![SequenceChannelSpec {
                id: 0,
                name: "mode".to_string(),
                available: vec!["commissioning".to_string(), "safe_mode".to_string()],
            }],
        };
        {
            let mut reg_out: MsgOut<SequenceRegistry> =
                MsgOut::new(ring.writer(NoWake, NoWake).expect("first writer"));
            reg_out.emit(&registry).expect("emit registry");
        }

        // Second record: a *different* Msg type, its own typed port on the same ring.
        let mut cmd_out: MsgOut<SequenceCommand> =
            MsgOut::new(ring.writer(NoWake, NoWake).expect("claim freed on drop"));
        let command = SequenceCommand {
            channel_id: 0,
            command: SequenceCommandKind::Start,
        };
        cmd_out.emit(&command).expect("emit command");

        let mut buf = Vec::new();

        // Record 1 — SequenceRegistry.
        assert!(view.try_read_into(&mut buf).expect("read record 1"));
        let (id, payload) = split_record(&buf).expect("split record 1");
        assert_eq!(id, SequenceRegistry::ID);
        let decoded: SequenceRegistry =
            postcard::from_bytes(payload).expect("round-trip registry");
        assert_eq!(decoded.channels.len(), 1);
        assert_eq!(decoded.channels[0].name, "mode");
        assert_eq!(decoded.channels[0].available, vec!["commissioning", "safe_mode"]);

        // Record 2 — SequenceCommand, a distinct id from the same writer.
        assert!(view.try_read_into(&mut buf).expect("read record 2"));
        let (id, payload) = split_record(&buf).expect("split record 2");
        assert_eq!(id, SequenceCommand::ID);
        assert_ne!(SequenceCommand::ID, SequenceRegistry::ID);
        let decoded: SequenceCommand =
            postcard::from_bytes(payload).expect("round-trip command");
        assert_eq!(decoded.channel_id, 0);
        assert!(matches!(decoded.command, SequenceCommandKind::Start));

        // No third record.
        assert!(!view.try_read_into(&mut buf).expect("no more records"));
    }

    /// `MsgIn` drains a heterogeneous channel, decoding only the records whose id matches
    /// its `Msg` type and skipping the rest — the command-bus drain shape. Emit a
    /// `SequenceRegistry` then two `SequenceCommand`s through one `MsgOut`; a
    /// `MsgIn<SequenceCommand>` yields exactly the two commands, in order.
    #[test]
    fn msg_in_drains_and_id_filters() {
        let ring = RingBuffer::create_in_memory(Config {
            capacity: msg_capacity(MAX_MSG_BYTES, MSG_DEPTH),
            max_readers: 4,
            overrun: Overrun::Overwrite,
        });
        // Claim the read view before any write (overwrite ring starts at the live edge).
        let mut inbox: MsgIn<SequenceCommand> = MsgIn::new(ring.view(NoWake, NoWake).expect("slot"));

        // Two typed ports take the ring's single writer in turn (drop frees the claim),
        // so the drained view sees a heterogeneous stream.
        // A different Msg type ahead of the commands — it must be skipped.
        {
            let mut reg_out: MsgOut<SequenceRegistry> =
                MsgOut::new(ring.writer(NoWake, NoWake).expect("first writer"));
            reg_out
                .emit(&SequenceRegistry {
                    channels: vec![SequenceChannelSpec {
                        id: 0,
                        name: "mode".to_string(),
                        available: vec![],
                    }],
                })
                .expect("emit registry");
        }
        let mut cmd_out: MsgOut<SequenceCommand> =
            MsgOut::new(ring.writer(NoWake, NoWake).expect("claim freed on drop"));
        cmd_out
            .emit(&SequenceCommand {
                channel_id: 1,
                command: SequenceCommandKind::Start,
            })
            .expect("emit start");
        cmd_out
            .emit(&SequenceCommand {
                channel_id: 2,
                command: SequenceCommandKind::Stop,
            })
            .expect("emit stop");

        let mut got: Vec<SequenceCommand> = Vec::new();
        inbox.drain(|c| got.push(c));
        assert_eq!(got.len(), 2, "the SequenceRegistry record is filtered out");
        assert_eq!(got[0].channel_id, 1);
        assert!(matches!(got[0].command, SequenceCommandKind::Start));
        assert_eq!(got[1].channel_id, 2);
        assert!(matches!(got[1].command, SequenceCommandKind::Stop));

        // Drained dry — a second drain yields nothing.
        let mut again = 0;
        inbox.drain(|_| again += 1);
        assert_eq!(again, 0);
    }

    /// A fan-in `MsgIn` (`from_views`) drains **every** producer view: two emitters on two
    /// rings, one `MsgIn<SequenceCommand>` over both views, yields all records
    /// (`docs/message-wiring.md` §3.3). This is the multi-view path WP4 wires as message edges.
    #[test]
    fn msg_in_fans_in_multiple_views() {
        let mk = || {
            RingBuffer::create_in_memory(Config {
                capacity: msg_capacity(MAX_MSG_BYTES, MSG_DEPTH),
                max_readers: 4,
                overrun: Overrun::Overwrite,
            })
        };
        let ring_a = mk();
        let ring_b = mk();
        let mut out_a: MsgOut<SequenceCommand> =
            MsgOut::new(ring_a.writer(NoWake, NoWake).expect("one writer per ring"));
        let mut out_b: MsgOut<SequenceCommand> =
            MsgOut::new(ring_b.writer(NoWake, NoWake).expect("one writer per ring"));
        // Two producer views into one input (the fan-in shape).
        let mut inbox: MsgIn<SequenceCommand> = MsgIn::from_views(vec![
            ring_a.view(NoWake, NoWake).expect("slot a"),
            ring_b.view(NoWake, NoWake).expect("slot b"),
        ]);

        out_a
            .emit(&SequenceCommand {
                channel_id: 1,
                command: SequenceCommandKind::Start,
            })
            .expect("emit a");
        out_b
            .emit(&SequenceCommand {
                channel_id: 2,
                command: SequenceCommandKind::Stop,
            })
            .expect("emit b");

        let mut got: Vec<u64> = Vec::new();
        inbox.drain(|c| got.push(c.channel_id));
        got.sort_unstable();
        assert_eq!(got, vec![1, 2], "both producers' records are drained");

        // An empty fan-in (unconnected input) drains nothing.
        let mut empty: MsgIn<SequenceCommand> = MsgIn::from_views(vec![]);
        let mut n = 0;
        empty.drain(|_| n += 1);
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
