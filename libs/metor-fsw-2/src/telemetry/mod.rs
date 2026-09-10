//! Downlink framing and command ingest over a shared [`LinkState`].
//!
//! [`UplinkSystem`] runs before command consumers. [`TelemetrySystem`] runs
//! last and batches the graph's output records for the link's socket tasks.
//! Snapshot taps contribute their newest record; log taps drain every record.
//! Table packets carry frame bytes, while message packets carry postcard data.
//!
//! Taps drain even without connections so producers are not backpressured.
//! Slow connections drop whole batches at their byte limit. See [`link`] for
//! queue and replay policies.

pub(crate) mod discovery;
mod link;
mod subscribe;
mod taps;
mod uplink;

pub use subscribe::{PeerStatus, SubscribeOut, SubscribeSystem};
pub use uplink::{UplinkParams, UplinkSystem};

pub use link::{LinkParams, LinkState, LinkStats};

use taps::{Tap, Taps, TelemetryMode, Wire, collect_taps};

use metor_fsw_2_core::log::LogLevel;
use metor_fsw_2_core::{
    AllOutputs, BuildCtx, BuildSystem, ConfigureError, CyclicSystem, Out, Shared, System,
    split_record,
};
use metor_proto::types::{PACKET_HEADER_LEN, PacketId, PacketTy, Timestamp};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

/// Wiring parameters for the built-in downlink (`type="Downlink"`): an
/// optional tap subset. With both lists absent every entry is tapped; with
/// either present an entry is tapped when its instance or its frame/channel
/// name is listed. The link itself is the target's `TcpServer` state
/// declaration, not a per-system address.
///
/// ```python
/// # optional; omit both lists to tap everything
/// m.add("telemetry", Downlink(instances=["nav", "imu"], frames=["gyro_b"]))
/// ```
#[derive(serde::Serialize, serde::Deserialize, postcard_schema::Schema, Debug, Clone, Default)]
pub struct DownlinkParams {
    /// Bare instance names to tap; the target namespace is applied for you.
    /// `None` (with `frames` also `None`) taps everything.
    #[serde(default)]
    pub instances: Option<Vec<String>>,
    /// Frame/channel names to tap.
    #[serde(default)]
    pub frames: Option<Vec<String>>,
}

impl BuildSystem for TelemetrySystem {
    type Params = DownlinkParams;

    /// Construct detached: the builtin link pack's ctor attaches the shared
    /// server ([`attach`](TelemetrySystem::attach)) right after.
    fn new(params: DownlinkParams) -> Self {
        let mode = match (params.instances, params.frames) {
            (None, None) => TelemetryMode::All,
            (instances, frames) => TelemetryMode::Subset {
                instances: instances.unwrap_or_default(),
                frames: frames.unwrap_or_default(),
            },
        };
        Self {
            link: None,
            mode,
            taps: Vec::new(),
            batch: Vec::new(),
            retain_scratch: Vec::new(),
            status: LinkStatus::default(),
        }
    }

    /// Prefix each authored instance name with the target namespace so the
    /// subset filter matches [`RegistryEntry::instance`], which the
    /// coordinator qualifies. `frames` is left alone: it matches
    /// [`RegistryEntry::name`], which is never qualified.
    fn configure(&mut self, ctx: &BuildCtx) -> Result<(), ConfigureError> {
        if let (Some(ns), TelemetryMode::Subset { instances, .. }) = (ctx.namespace, &mut self.mode)
        {
            for instance in instances {
                *instance = format!("{ns}.{instance}");
            }
        }
        Ok(())
    }
}

/// Link status and access to the output registry.
///
/// [`AllOutputs`] grants a reader slot on every registered output.
#[derive(crate::SystemOutput)]
pub struct TelemetryPorts {
    status: metor_fsw_2_core::Output<LinkStatus>,
    all: AllOutputs,
}

/// The link gauge the downlink publishes when it changes: who is connected
/// and what the link has dropped, on the wire like any frame.
#[derive(crate::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Default, Clone, PartialEq)]
#[repr(C)]
#[metor_fsw(name = "link_status")]
pub struct LinkStatus {
    #[metor_fsw(timestamp)]
    pub timestamp: Timestamp,
    /// Live connections.
    pub connections: u64,
    /// Connections accepted over the run.
    pub accepted: u64,
    /// Whole batches dropped for one connection over its pending cap.
    pub dropped: u64,
}

/// A [`CyclicSystem`] that frames every tapped output buffer's pending
/// records into one batch per cycle and hands it to the shared [`LinkState`]
/// for fan-out. Register it after every other cyclic system, or let the
/// wiring resolver defer it there; its `ReceiveAll` capability is what the
/// build-time ordering check keys on.
///
/// Read views are claimed in `init`, which runs after earlier-registered
/// systems' `init`s, so a frame or message emitted during another system's
/// `init` is not downlinked (the view starts at the live edge past it). Values
/// that must reach the ground should be published from the first `execute`
/// onward.
pub struct TelemetrySystem {
    /// The shared link server. `None` on a detached instance
    /// ([`BuildSystem::new`]); the builtin link pack's ctor attaches it.
    link: Option<Shared<LinkState>>,
    mode: TelemetryMode,
    /// The resolved taps; each carries its own delivery axis and wire framing.
    taps: Vec<Tap>,
    /// The cycle's batch scratch, cleared and refilled in place.
    batch: Vec<u8>,
    /// One framed record, reused across retained-tap updates.
    retain_scratch: Vec<u8>,
    /// The last published gauge, so the frame goes out on change only.
    status: LinkStatus,
}

impl TelemetrySystem {
    /// Attach the shared link server this downlink streams through.
    pub fn attach(mut self, link: Shared<LinkState>) -> Self {
        self.link = Some(link);
        self
    }
}

impl System for TelemetrySystem {
    type Input = ();
    type Output = Out<TelemetryPorts>;
    const NAME: &'static str = "telemetry";

    /// Resolve the tap set, claim one read `View` per tapped buffer, and hand
    /// the announce set to the link server, whose accepted connections replay
    /// it before any data.
    fn init(&mut self, output: &mut Self::Output) {
        let Taps {
            taps,
            announces,
            retained,
            exhausted,
            collisions,
        } = collect_taps(&output.all, &self.mode);

        for (refused, kept) in &collisions {
            output.log().fault(
                LogLevel::Error,
                "telemetry_table_id_collision",
                "two tables hash to one packet id; the later is not downlinked",
                &[("refused", refused), ("kept", kept)],
            );
        }

        for key in &exhausted {
            output.log().fault(
                LogLevel::Warn,
                "telemetry_reader_slot",
                &format!("no reader slot left on `{key}` — raise CoordinatorConfig::reader_slack"),
                &[],
            );
        }

        let link = self
            .link
            .as_ref()
            .expect("downlink attached to a TcpServer state (the builtin link pack's ctor)");
        if link.get().set_announces(&announces).is_err() {
            // A second downlink on one server would corrupt the replay every
            // connection decodes against.
            output.log().fault(
                LogLevel::Warn,
                "link_announce_conflict",
                "another downlink already announced on this link; this instance streams nothing",
                &[],
            );
            self.link = None;
            return;
        }
        link.get().set_retained_slots(retained);
        self.taps = taps;
    }
}

/// Append one length-prefixed packet to `batch`: the framing [`LenPacket`]
/// builds (`metor_proto::types::LenPacket`), minus the intermediate
/// allocation.
fn append_packet(batch: &mut Vec<u8>, ty: PacketTy, id: PacketId, payload: &[u8]) {
    batch.extend_from_slice(&((PACKET_HEADER_LEN + payload.len()) as u32).to_le_bytes());
    batch.push(ty as u8);
    batch.extend_from_slice(&id);
    batch.push(0); // req_id
    batch.extend_from_slice(payload);
}

/// Frame one drained record onto `batch` per the tap's [`Wire`]: a `Table`
/// packet under the announce-assigned id, or a self-describing `Msg` packet
/// keyed by the record's own leading id (skipped if the record is too short
/// to carry one).
fn append_record(batch: &mut Vec<u8>, wire: &Wire, rec: &[u8]) {
    match wire {
        Wire::Table { packet_id } => append_packet(batch, PacketTy::Table, *packet_id, rec),
        Wire::Msg => {
            if let Some((id, payload)) = split_record(rec) {
                append_packet(batch, PacketTy::Msg, id, payload);
            }
        }
    }
}

impl CyclicSystem for TelemetrySystem {
    fn execute(&mut self, now: Timestamp, _input: &mut Self::Input, output: &mut Self::Output) {
        // An announce conflict detached this instance; taps were never taken,
        // so there is nothing to drain.
        let Some(link_token) = &self.link else {
            return;
        };
        let mut link = link_token.get();

        // Report the server's counters on this cycle's log and the gauge.
        let stats = link.take_stats();
        if stats.closed > 0 {
            output.log().fault(
                LogLevel::Info,
                "link_disconnect",
                "link connections closed",
                &[("closed", &stats.closed)],
            );
        }
        if stats.conn_dropped > 0 {
            output.log().fault(
                LogLevel::Warn,
                "link_conn_dropped",
                "client batches dropped",
                &[("dropped", &stats.conn_dropped)],
            );
        }
        if stats.inbound_dropped > 0 {
            output.log().fault(
                LogLevel::Warn,
                "link_inbound_dropped",
                "inbound command queue overflowed",
                &[("dropped", &stats.inbound_dropped)],
            );
        }
        let connections = link.connections();
        let status = LinkStatus {
            timestamp: now,
            connections: connections as u64,
            accepted: self.status.accepted + stats.accepted,
            dropped: self.status.dropped + stats.conn_dropped,
        };
        if status.connections != self.status.connections
            || status.accepted != self.status.accepted
            || status.dropped != self.status.dropped
        {
            output.status.publish(&status);
            self.status = status;
        }

        // With no connections the batch is skipped entirely, but the taps
        // still drain below (records are consumed and DISCARDED) because an
        // undrained tap view stalls its producer's ring and freezes every
        // consumer of that output, not just telemetry.
        link.prepare_batch(&mut self.batch);
        let Self {
            taps,
            batch,
            retain_scratch,
            ..
        } = self;
        let mut batch = (connections != 0).then_some(batch);
        let corrupt = taps::drain_taps(taps, |wire, retain_slot, rec| match retain_slot {
            // A retained tap frames once and the bytes go both ways:
            // appended to this cycle's batch and held for future
            // connections' replays, even with no connection live right now.
            Some(slot) => {
                retain_scratch.clear();
                append_record(retain_scratch, wire, rec);
                if let Some(batch) = &mut batch {
                    batch.extend_from_slice(retain_scratch);
                }
                link.retain(slot, retain_scratch);
            }
            None => {
                if let Some(batch) = &mut batch {
                    append_record(batch, wire, rec);
                }
            }
        });
        if corrupt > 0 {
            output.log().fault(
                LogLevel::Error,
                "telemetry_input_corrupt",
                "tap ring read corrupt",
                &[("taps", &corrupt)],
            );
        }
        if let Some(batch) = batch
            && !batch.is_empty()
        {
            link.broadcast_buffer(batch);
        }
        link.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use metor_fsw_2_core::{MsgTable, PortDesc, RegistryEntry};
    use metor_fsw_ring::{Config, RingBuffer};
    use metor_proto::types::ComponentId;
    use metor_proto::types::table_id;

    fn entry(instance: &str, name: &str) -> RegistryEntry {
        let desc = PortDesc::msg_dynamic(name, PacketId::from([0, 7]));
        RegistryEntry::new(
            ComponentId::new(&format!("{instance}.{name}")),
            instance.into(),
            desc,
            RingBuffer::create_in_memory(Config {
                capacity: 1024,
                max_readers: 4,
            }),
        )
    }

    fn downlink(namespace: Option<&str>) -> TelemetrySystem {
        let mut sys = TelemetrySystem::new(DownlinkParams {
            instances: Some(vec!["nav".into()]),
            frames: None,
        });
        sys.configure(&BuildCtx {
            msgs: &MsgTable::default(),
            namespace,
        })
        .unwrap();
        sys
    }

    /// A frame producer, the one table a served target announces beyond the
    /// coordinator's own.
    #[derive(crate::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Default)]
    #[repr(C)]
    #[metor_fsw(name = "beat")]
    struct Beat {
        #[metor_fsw(timestamp)]
        timestamp: Timestamp,
        n: u64,
    }

    #[derive(crate::SystemOutput)]
    struct BeaterOut {
        beat: metor_fsw_2_core::Output<Beat>,
    }

    struct Beater(u64);

    impl System for Beater {
        type Input = ();
        type Output = Out<BeaterOut>;
        const NAME: &'static str = "beater";
    }

    impl CyclicSystem for Beater {
        fn execute(&mut self, now: Timestamp, _input: &mut (), output: &mut Self::Output) {
            self.0 += 1;
            let _ = output.beat.write(&Beat {
                timestamp: now,
                n: self.0,
            });
        }
    }

    impl BuildSystem for Beater {
        type Params = ();
        fn new(_params: ()) -> Self {
            Beater(0)
        }
    }

    /// A port nothing else claims, so the ephemeral bind is free when the
    /// target takes it.
    fn free_port() -> u16 {
        std::net::TcpListener::bind(("127.0.0.1", 0))
            .expect("a free port")
            .local_addr()
            .expect("bound")
            .port()
    }

    /// Every announced table is keyed by the hash of the vtable it carries,
    /// never by its position in the announce set.
    #[cfg(not(miri))]
    #[stellarator::test]
    async fn table_ids_are_hashes_of_the_announce() {
        use crate::wiring::{Registry, WiringBuilder, resolve};
        use metor_proto::types::{Msg, OwnedPacket};
        use metor_proto_wkt::VTableMsg;

        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], free_port()));
        let wiring = WiringBuilder::new()
            .coordinator(200.0, crate::ClockSpec::Wall)
            .system("beat")
            .ty("Beat")
            .end()
            .serve(addr)
            .build();
        let mut registry = Registry::with_builtins();
        registry.register::<Beater, _>("Beat");
        let mut coord = resolve(&wiring, &registry).expect("the served target resolves");

        let client = async {
            let metor_proto_stellar::Peer::Fsw { mut rx, .. } = metor_proto_stellar::identify(addr)
                .await
                .expect("the link answers")
            else {
                panic!("a downlink identifies as an fsw link")
            };
            let mut buf = vec![0u8; 64 * 1024];
            let mut seen = 0usize;
            loop {
                let pkt = rx.next_grow(buf).await.expect("the link stays up");
                if let OwnedPacket::Msg(m) = &pkt
                    && m.id == VTableMsg::ID
                {
                    let msg = m.parse::<VTableMsg>().expect("a vtable announce");
                    assert_ne!(msg.id, [0, 0], "a table id is never a sequence number");
                    assert_eq!(msg.id, table_id(&msg.vtable));
                    seen += 1;
                    if seen == 2 {
                        return;
                    }
                }
                buf = pkt.into_buf().into_inner();
            }
        };
        futures_lite::future::race(coord.run_for(4000), client).await;
    }

    /// Two instances of one frame whose prefixed vtables fold to the same
    /// packet id, found by search over the instance suffix.
    const COLLIDING: (&str, &str) = ("beat47", "beat335");

    /// The later of two tables hashing alike is refused, and the fault names
    /// both, so a rename is the visible fix.
    #[cfg(not(miri))]
    #[stellarator::test]
    async fn colliding_tables_fault_and_drop_the_later() {
        use crate::wiring::{Registry, WiringBuilder, resolve};
        use metor_fsw_2_core::PortDesc;
        use metor_proto::types::{Msg, OwnedPacket};
        use metor_proto_wkt::{LogEvent, VTableMsg};

        let desc = PortDesc::of::<Beat>();
        let announced = |instance: &str| table_id(&desc.announce(instance).expect("a table").0);
        let collided = announced(COLLIDING.0);
        assert_eq!(collided, announced(COLLIDING.1), "the pair still collides");

        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], free_port()));
        let wiring = WiringBuilder::new()
            .coordinator(200.0, crate::ClockSpec::Wall)
            .system(COLLIDING.0)
            .ty("Beat")
            .end()
            .system(COLLIDING.1)
            .ty("Beat")
            .end()
            .serve(addr)
            .build();
        let mut registry = Registry::with_builtins();
        registry.register::<Beater, _>("Beat");
        let mut coord = resolve(&wiring, &registry).expect("the target resolves");

        let client = async {
            let metor_proto_stellar::Peer::Fsw { mut rx, .. } = metor_proto_stellar::identify(addr)
                .await
                .expect("the link answers")
            else {
                panic!("a downlink identifies as an fsw link")
            };
            let mut buf = vec![0u8; 64 * 1024];
            let mut announces = 0usize;
            loop {
                let pkt = rx.next_grow(buf).await.expect("the link stays up");
                if let OwnedPacket::Msg(m) = &pkt {
                    if m.id == VTableMsg::ID
                        && m.parse::<VTableMsg>().expect("a vtable announce").id == collided
                    {
                        announces += 1;
                    }
                    if m.id == LogEvent::ID
                        && let Ok(ev) = m.parse::<LogEvent>()
                        && field(&ev, "kind").as_deref() == Some("telemetry_table_id_collision")
                    {
                        assert_eq!(
                            field(&ev, "refused").as_deref(),
                            Some(&*format!("{}.beat", COLLIDING.1))
                        );
                        assert_eq!(
                            field(&ev, "kept").as_deref(),
                            Some(&*format!("{}.beat", COLLIDING.0))
                        );
                        assert_eq!(announces, 1, "the colliding id is announced once");
                        return;
                    }
                }
                buf = pkt.into_buf().into_inner();
            }
        };
        futures_lite::future::race(coord.run_for(4000), client).await;
    }

    fn field(ev: &metor_proto_wkt::LogEvent, key: &str) -> Option<String> {
        ev.fields
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    }

    #[test]
    fn subset_matches_qualified_instances() {
        let sys = downlink(Some("sat"));
        assert!(sys.mode.matches(&entry("sat.nav", "gyro_b")));
        assert!(!sys.mode.matches(&entry("nav", "gyro_b")));

        let sys = downlink(None);
        assert!(sys.mode.matches(&entry("nav", "gyro_b")));
        assert!(!sys.mode.matches(&entry("sat.nav", "gyro_b")));
    }
}
