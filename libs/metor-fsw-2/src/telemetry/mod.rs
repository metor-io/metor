//! Work-Package 7 — the telemetry downlink (telemetry.md §3–§7).
//!
//! [`TelemetrySystem`] is the output registry's first consumer: an ordinary
//! [`CyclicSystem`] registered **last**, so its end-of-cycle `execute` snapshots the
//! freshest record of every tapped output buffer and re-emits each as a metor-proto
//! `Table` packet referencing a once-announced (instance-prefixed) `VTable` — the same
//! wire format `metor-db` ingests. Because a frame's ring payload *is* its table bytes,
//! there is no serialization step; the bytes a system committed are the bytes on the wire.
//!
//! ## Cycle / sender split (telemetry.md §4)
//!
//! The control cycle is single-threaded and synchronous; the socket is async. So the
//! in-cycle stage only *snapshots* (a `memcpy` per tapped buffer with a fresh record)
//! into a bounded, per-tap-coalescing hand-off, and a `stellarator::spawn`ed sender task
//! drains it and does the awaiting I/O. The cycle never blocks on the link: a backed-up
//! transport just causes snapshots to overwrite un-sent ones (latest-wins), bumping a
//! `telemetry.dropped` health counter. Loss, never delay — the same overrun philosophy
//! as the `Overwrite` rings one level down.

use std::sync::atomic::{
    AtomicBool, AtomicU64,
    Ordering::{AcqRel, Acquire, Relaxed, Release},
};
use std::sync::{Arc, Mutex};

use metor_fsw_ring::{BoxBacking, NoWake, View};
use metor_proto::types::{ComponentId, LenPacket, PacketId, Timestamp};
use metor_proto::vtable::VTable;
use metor_proto_stellar::PacketSink;
use metor_proto_wkt::{ComponentMetadata, SetComponentMetadata, VTableMsg};
use stellarator::JoinHandleDropGuard;
use stellarator::io::{OwnedReader, OwnedWriter, SplitExt};
use stellarator::net::TcpStream;
use stellarator::sync::WaitQueue;

use crate::binder::{BindPorts, RingSource};
use crate::descriptor::PortDesc;
use crate::registry::{OutputRegistry, RegistryEntry};
use crate::system::{CyclicSystem, Out, System, SystemInput, SystemOutput};

// ---------------------------------------------------------------------------
// Transport (telemetry.md §7)
// ---------------------------------------------------------------------------

/// An async error a [`Transport`] surfaces; v1 stops downlinking on any error (the
/// in-cycle snapshot stage keeps running and simply drops).
#[derive(Debug)]
pub enum TransportError {
    /// The link is not (or no longer) connected.
    Disconnected,
    /// An underlying I/O error, rendered to a string (transport-agnostic).
    Io(String),
}

/// Isolates the wire from the streamer (telemetry.md §7). v1 ships [`TcpTransport`];
/// `bbq`/SHM is a documented future impl. Tests drive an in-memory mock against the
/// same trait.
#[allow(async_fn_in_trait)]
pub trait Transport {
    /// Announce one tap's prefixed vtable + component metadata (sent once on connect):
    /// a `VTableMsg` followed by a `SetComponentMetadata` per component — exactly the
    /// two steps `SinkExt::init_world` does, but with a per-instance prefix.
    async fn announce(
        &mut self,
        msg: &VTableMsg,
        meta: &[ComponentMetadata],
    ) -> Result<(), TransportError>;

    /// Send one `Table` packet referencing a previously announced vtable.
    async fn send(&mut self, pkt: LenPacket) -> Result<(), TransportError>;
}

/// A live TCP connection's write half plus the read half held only to keep the socket
/// open (v1 does not read replies).
struct TcpConn {
    sink: PacketSink<OwnedWriter<TcpStream>>,
    #[allow(dead_code)]
    rx: OwnedReader<TcpStream>,
}

/// The v1 transport: connect-once to a ground/db endpoint and stream `LenPacket`s,
/// the same path cube-sat's hand-written loop uses (telemetry.md §7). On disconnect it
/// stops downlinking (drop-on-disconnect); automatic reconnect/backoff is future work.
pub struct TcpTransport {
    addr: std::net::SocketAddr,
    conn: Option<TcpConn>,
}

impl TcpTransport {
    /// A transport that will connect to `addr` on its first announce, inside the async
    /// sender task (connecting is async, so it cannot happen at build).
    pub fn new(addr: std::net::SocketAddr) -> Self {
        Self { addr, conn: None }
    }

    /// Connect lazily on first use; subsequent calls reuse the open socket.
    async fn ensure(&mut self) -> Result<&PacketSink<OwnedWriter<TcpStream>>, TransportError> {
        if self.conn.is_none() {
            let stream = TcpStream::connect(self.addr)
                .await
                .map_err(|e| TransportError::Io(format!("{e}")))?;
            let (rx, tx) = stream.split();
            self.conn = Some(TcpConn {
                sink: PacketSink::new(tx),
                rx,
            });
        }
        Ok(&self.conn.as_ref().expect("just connected").sink)
    }
}

impl Transport for TcpTransport {
    async fn announce(
        &mut self,
        msg: &VTableMsg,
        meta: &[ComponentMetadata],
    ) -> Result<(), TransportError> {
        let sink = self.ensure().await?;
        sink.send(msg)
            .await
            .0
            .map_err(|e| TransportError::Io(format!("{e}")))?;
        for m in meta {
            sink.send(&SetComponentMetadata(m.clone()))
                .await
                .0
                .map_err(|e| TransportError::Io(format!("{e}")))?;
        }
        Ok(())
    }

    async fn send(&mut self, pkt: LenPacket) -> Result<(), TransportError> {
        let sink = self.ensure().await?;
        sink.send(pkt)
            .await
            .0
            .map_err(|e| TransportError::Io(format!("{e}")))?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Which outputs to tap (telemetry.md §3).
#[derive(Clone, Debug)]
pub enum TelemetryMode {
    /// Tap every registry entry: every system's user frames *and* their implicit
    /// `health`/`log`, plus the coordinator-owned `health`/`log`/`status`.
    All,
    /// Tap only the entries whose instance name or frame name matches the configured
    /// lists (matching either is enough).
    Subset {
        instances: Vec<String>,
        frames: Vec<String>,
    },
}

impl TelemetryMode {
    fn matches(&self, entry: &RegistryEntry) -> bool {
        match self {
            TelemetryMode::All => true,
            TelemetryMode::Subset { instances, frames } => {
                instances.iter().any(|i| i.as_str() == &*entry.instance)
                    || frames.iter().any(|f| ComponentId::new(f) == entry.frame_id)
            }
        }
    }
}

/// The telemetry downlink configuration handed to
/// [`add_telemetry`](crate::CoordinatorBuilder::add_telemetry).
pub struct TelemetryConfig<T: Transport> {
    /// Where the snapshots go (TCP in v1, a mock in tests).
    pub transport: T,
    /// Which outputs to tap.
    pub mode: TelemetryMode,
}

// ---------------------------------------------------------------------------
// Hand-off (the bounded, per-tap-coalescing queue, telemetry.md §4)
// ---------------------------------------------------------------------------

/// One pending packet slot per tap: a newer snapshot overwrites an older un-sent one
/// (latest-wins coalescing — the Overwrite ring semantics, one level up). Overwriting an
/// *occupied* slot means a snapshot was dropped (the link is backed up), bumping `dropped`.
struct HandOff {
    slots: Mutex<Vec<Option<LenPacket>>>,
    /// Set when a slot is filled; cleared by the sender when it parks. Just avoids busy
    /// spinning — a missed wake is harmless because the sender always drains *all* slots.
    pending: AtomicBool,
    /// Count of snapshots dropped by overwrite (surfaced as `telemetry.dropped` health).
    dropped: AtomicU64,
    /// Wakes the parked sender when a snapshot is pushed (or on shutdown).
    wq: WaitQueue,
}

impl HandOff {
    fn new(n_taps: usize) -> Self {
        Self {
            slots: Mutex::new((0..n_taps).map(|_| None).collect()),
            pending: AtomicBool::new(false),
            dropped: AtomicU64::new(0),
            wq: WaitQueue::new(),
        }
    }

    /// Cycle side (never blocks): coalesce `pkt` into `slot`, counting a drop if it
    /// overwrote an un-sent packet, then wake the sender.
    fn push(&self, slot: usize, pkt: LenPacket) {
        {
            let mut slots = self.slots.lock().expect("handoff poisoned");
            if slots[slot].is_some() {
                self.dropped.fetch_add(1, Relaxed);
            }
            slots[slot] = Some(pkt);
        }
        self.pending.store(true, Release);
        self.wq.wake_all();
    }

    /// Sender side: take every pending packet (drops the lock before any `.await`).
    fn drain(&self) -> Vec<LenPacket> {
        let mut slots = self.slots.lock().expect("handoff poisoned");
        slots.iter_mut().filter_map(Option::take).collect()
    }
}

/// One announced tap's wire schema, moved into the sender task to replay on connect.
struct Announce {
    packet_id: PacketId,
    vtable: VTable,
    metadata: Vec<ComponentMetadata>,
}

/// The async sender task (telemetry.md §4): announce every tap once on connect, then
/// drain the hand-off and send `Table` packets. Stops downlinking on any transport error
/// or when `stop` is set (the cycle is unaffected either way).
async fn run_sender<T: Transport>(
    mut transport: T,
    announces: Vec<Announce>,
    handoff: Arc<HandOff>,
    stop: Arc<AtomicBool>,
) {
    for a in &announces {
        let msg = VTableMsg {
            id: a.packet_id,
            vtable: a.vtable.clone(),
        };
        if transport.announce(&msg, &a.metadata).await.is_err() {
            return;
        }
    }
    loop {
        if stop.load(Acquire) {
            return;
        }
        let pkts = handoff.drain();
        if pkts.is_empty() {
            let _ = handoff
                .wq
                .wait_for(|| handoff.pending.swap(false, AcqRel) || stop.load(Acquire))
                .await;
            continue;
        }
        for pkt in pkts {
            if transport.send(pkt).await.is_err() {
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Ports (no typed inputs; the output bundle carries the registry handle)
// ---------------------------------------------------------------------------

/// The telemetry system has no typed inputs (telemetry.md §3); its output bundle pulls
/// the broad registry handle so `init` can reach it through the framework's `output`.
pub struct TelemetryPorts {
    registry: Arc<OutputRegistry>,
}

impl SystemOutput for TelemetryPorts {
    fn descriptors() -> Vec<PortDesc> {
        Vec::new()
    }
}

impl BindPorts<BoxBacking> for TelemetryPorts {
    /// Host-only (`B = BoxBacking`): pulls the broad registry the host [`Binder`]
    /// carries (telemetry.md §2.4). The telemetry downlink is never dlopen'd.
    fn bind<S: RingSource<B = BoxBacking>>(src: &mut S) -> Self {
        Self {
            registry: src.output_registry(),
        }
    }
}

/// The empty input bundle (the streamer pulls outputs via the registry, not typed edges).
pub struct TelemetryIn;

impl SystemInput for TelemetryIn {
    fn descriptors() -> Vec<PortDesc> {
        Vec::new()
    }
    fn any_lapped(&self) -> bool {
        false
    }
}

impl BindPorts<BoxBacking> for TelemetryIn {
    fn bind<S: RingSource<B = BoxBacking>>(_src: &mut S) -> Self {
        TelemetryIn
    }
}

// ---------------------------------------------------------------------------
// The system
// ---------------------------------------------------------------------------

/// One resolved tap: a read view into a buffer, its assigned packet id, and the reused
/// snapshot scratch.
struct Tap {
    slot: usize,
    packet_id: PacketId,
    view: View<BoxBacking, NoWake, NoWake>,
    scratch: Vec<u8>,
}

/// The telemetry downlink system (telemetry.md §3). Generic over the [`Transport`]; the
/// concrete `T` is chosen by the builder/loader (TCP) or a test (mock).
pub struct TelemetrySystem<T: Transport> {
    transport: Option<T>,
    mode: TelemetryMode,
    taps: Vec<Tap>,
    handoff: Option<Arc<HandOff>>,
    stop: Option<Arc<AtomicBool>>,
    /// Holds the sender task; its `Drop` cancels the task on teardown.
    #[allow(dead_code)]
    sender: Option<JoinHandleDropGuard<()>>,
    /// How many drops we have already surfaced to health (so each new one is reported once).
    last_dropped: u64,
}

impl<T: Transport> TelemetrySystem<T> {
    /// Construct the (pre-init) downlink from its config. Taps are resolved and the
    /// sender spawned at `init`, where the registry handle is reachable.
    pub fn new(config: TelemetryConfig<T>) -> Self {
        Self {
            transport: Some(config.transport),
            mode: config.mode,
            taps: Vec::new(),
            handoff: None,
            stop: None,
            sender: None,
            last_dropped: 0,
        }
    }
}

impl<T: Transport + 'static> System for TelemetrySystem<T> {
    type Input = TelemetryIn;
    type Output = Out<TelemetryPorts>;
    const NAME: &'static str = "telemetry";

    /// Resolve the tap set, claim one read `View` per tapped buffer, and spawn the async
    /// sender (telemetry.md §3/§4). `init` runs on the coordinator's loop task within
    /// `start()`, so `stellarator::spawn` has a runtime; the sender announces before any
    /// data is queued (nothing is pushed until the first `execute`).
    fn init(&mut self, output: &mut Self::Output) {
        let registry = output.registry.clone();
        let mut taps = Vec::new();
        let mut announces = Vec::new();
        for entry in registry.entries() {
            if !self.mode.matches(entry) {
                continue;
            }
            let view = match entry.view() {
                Ok(v) => v,
                // Reader-slot budget exhausted: surface it and skip this tap rather than
                // panicking (telemetry.md §2.5). Build-time sizing makes this unreachable
                // for the known consumers, but a hand-built over-subscription is observable.
                Err(_) => {
                    output.health().error("telemetry.reader_slot");
                    continue;
                }
            };
            let slot = taps.len();
            let packet_id = (slot as u16).to_le_bytes();
            taps.push(Tap {
                slot,
                packet_id,
                view,
                scratch: Vec::new(),
            });
            announces.push(Announce {
                packet_id,
                vtable: entry.vtable.clone(),
                metadata: entry.metadata.clone(),
            });
        }

        let handoff = Arc::new(HandOff::new(taps.len()));
        let stop = Arc::new(AtomicBool::new(false));
        if let Some(transport) = self.transport.take() {
            let handle = stellarator::spawn(run_sender(
                transport,
                announces,
                handoff.clone(),
                stop.clone(),
            ));
            self.sender = Some(handle.drop_guard());
        }
        self.taps = taps;
        self.handoff = Some(handoff);
        self.stop = Some(stop);
    }

    /// Signal the sender to stop and wake it so it exits cooperatively (the drop guard
    /// cancels it regardless when the system is dropped at coordinator teardown).
    fn shutdown(&mut self, _output: &mut Self::Output) {
        if let Some(stop) = &self.stop {
            stop.store(true, Release);
        }
        if let Some(handoff) = &self.handoff {
            handoff.wq.wake_all();
        }
    }
}

impl<T: Transport + 'static> CyclicSystem for TelemetrySystem<T> {
    /// End-of-cycle snapshot (telemetry.md §3/§4): for each tap with a fresh record,
    /// copy the latest table bytes into a `LenPacket` and hand it to the sender; never
    /// awaits. Any drops detected since last cycle are surfaced as `telemetry.dropped`.
    fn execute(&mut self, _now: Timestamp, _input: &mut Self::Input, output: &mut Self::Output) {
        let handoff = match &self.handoff {
            Some(h) => h.clone(),
            None => return,
        };
        for tap in &mut self.taps {
            // Drain to the newest committed record (latest-wins); a lapped view skips to
            // the live edge. No new record this cycle ⇒ nothing to send for this tap.
            let mut got = false;
            loop {
                match tap.view.try_read_into(&mut tap.scratch) {
                    Ok(true) => got = true,
                    Ok(false) => break,
                    Err(_) => {
                        tap.view.resync();
                        break;
                    }
                }
            }
            if got {
                let mut pkt = LenPacket::table(tap.packet_id, tap.scratch.len());
                pkt.extend_from_slice(&tap.scratch);
                handoff.push(tap.slot, pkt);
            }
        }
        // Surface any snapshots the sender's backlog forced us to drop (telemetry.md §4).
        let dropped = handoff.dropped.load(Relaxed);
        while self.last_dropped < dropped {
            output.health().error("telemetry.dropped");
            self.last_dropped += 1;
        }
    }
}

#[cfg(test)]
mod tests;
