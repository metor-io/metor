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
//! [`MsgOut`] is the emit port — the message twin of [`Output`](crate::Output). It is
//! **type-erased** over the payload type ([`emit`](MsgOut::emit) is generic per call),
//! so one writer carries a heterogeneous stream of `Msg`s onto a single ring. v1 mints
//! `MsgOut`s coordinator-side (the `msg_writer` helper); the user-bundle/binder path is
//! deferred (`docs/messages.md` Q4).

use metor_fsw_ring::{Backing, BoxBacking, NoWake, WakeSink, WakeSource, WriteError, Writer, frame_len};
use metor_proto::types::{Msg, PacketId};

use crate::binder::RingSource;

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
/// `(id, postcard)` records, type-erased over the payload (`docs/messages.md` §1.2).
/// The message twin of [`Output`](crate::Output) — same single-writer discipline, same
/// `BoxBacking`/`NoWake` cyclic default — but [`emit`](Self::emit) is generic per call,
/// so one port carries a heterogeneous stream of [`Msg`] types onto a single ring.
pub struct MsgOut<B = BoxBacking, WD = NoWake, WS = NoWake>
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
}

impl<B: Backing, WD: WakeSource, WS: WakeSink> MsgOut<B, WD, WS> {
    /// Wrap a writer the coordinator created over a [`msg_capacity`]-sized byte ring.
    pub fn new(writer: Writer<B, WD, WS>) -> Self {
        Self {
            writer,
            scratch: Vec::new(),
        }
    }

    /// Emit one message: write the 2-byte [`Msg::ID`] then the postcard-serialized
    /// payload as a single ring record (`docs/messages.md` §1.2). Serialization runs
    /// into the reused `scratch` via the same `serialize_with_flavor` path the
    /// `IntoLenPacket` impl uses (`../metor-proto/src/types.rs:620`); only the ring
    /// `try_write` can fail, so this surfaces a [`WriteError`] and nothing else.
    pub fn emit<M: Msg>(&mut self, msg: &M) -> Result<(), WriteError> {
        self.scratch.clear();
        self.scratch.extend_from_slice(&M::ID);
        postcard::serialize_with_flavor(msg, ScratchFlavor(&mut self.scratch))
            .expect("postcard serialization into an in-memory buffer is infallible");
        self.writer.try_write(&self.scratch)
    }
}

impl<B, WD, WS> MsgOut<B, WD, WS>
where
    B: Backing,
    WD: WakeSource + Default + Clone + 'static,
    WS: WakeSink + Default + Clone + 'static,
{
    /// Bind this port over the next ring the [`RingSource`] hands out, taking the
    /// matched writer-side wake endpoints — the [`Output::bind`](crate::Output) mirror
    /// for the future user-bundle path. v1 mints `MsgOut`s coordinator-side via the
    /// `msg_writer` helper, so this is the not-yet-walked binder seam
    /// (`docs/messages.md` Q4).
    pub fn bind<S: RingSource<B = B>>(src: &mut S) -> Self {
        let (ring, data, space) = src.next_output::<WD, WS>();
        MsgOut::new(ring.writer(data, space))
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

    use super::{MAX_MSG_BYTES, MSG_DEPTH, MsgOut, msg_capacity, split_record};
    use crate::registry::MessageEntry;

    /// Mint a `MsgOut` over a fresh byte ring, emit two *different* `Msg` types through
    /// the one type-erased port, read them back via a `MessageEntry` `View`, and assert
    /// `split_record` recovers each id and that the payloads postcard-round-trip.
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
            ring: ring.clone(),
        };

        let mut out: MsgOut = MsgOut::new(ring.writer(NoWake, NoWake));

        // Claim the tap before any write — a view on an overwrite ring starts at the
        // live edge, so the tap must exist before data flows (the telemetry-tap order).
        let mut view = entry.view().expect("reader slot");

        // First record: a whole-registry declaration.
        let registry = SequenceRegistry {
            channels: vec![SequenceChannelSpec {
                id: 0,
                name: "mode".to_string(),
                available: vec!["commissioning".to_string(), "safe_mode".to_string()],
            }],
        };
        out.emit(&registry).expect("emit registry");

        // Second record: a *different* Msg type on the same port — the type-erasure proof.
        let command = SequenceCommand {
            channel_id: 0,
            command: SequenceCommandKind::Start,
        };
        out.emit(&command).expect("emit command");

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

    /// `split_record` rejects a record too short to carry a 2-byte id.
    #[test]
    fn split_record_rejects_short() {
        assert!(split_record(&[]).is_none());
        assert!(split_record(&[7]).is_none());
        assert_eq!(split_record(&[1, 2]), Some(([1, 2], &[][..])));
        assert_eq!(split_record(&[1, 2, 9]), Some(([1, 2], &[9][..])));
    }
}
