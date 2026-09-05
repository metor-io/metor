//! The [`WireError`] graph-defect vocabulary, reported before any byte flows.

use metor_fsw_2_core::{Hz, PortId};

use metor_fsw_2_core::NAME_CAP;

/// A defect in the declared graph.
///
/// Not `Eq`: [`InvalidCycleRate`](WireError::InvalidCycleRate) carries the
/// offending `f64` rate so the message can name it.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum WireError {
    /// A system has no port carrying the named frame or message.
    #[error("system #{system} has no port {port:?}")]
    UnknownPort { system: usize, port: PortId },
    /// The producer's record shape does not satisfy the consumer's required
    /// shape (the table subset rule, postcard id equality, delivery agreement).
    #[error("incompatible edge {producer} -> {consumer} on port {port:?}")]
    Incompatible {
        producer: String,
        consumer: String,
        port: PortId,
    },
    /// A [`FanIn::One`](crate::FanIn) input port was never connected, so nothing
    /// would ever write it. [`FanIn::Many`](crate::FanIn) inputs may be left
    /// unconnected; zero producers is legal there.
    #[error("{system} input for port {port:?} is not connected")]
    UnconnectedInput { system: String, port: PortId },
    /// Two producers were connected into one [`FanIn::One`](crate::FanIn) input
    /// port. [`FanIn::Many`](crate::FanIn) inputs allow fan-in, so this never
    /// fires for them, though an exact duplicate of one edge is still a
    /// [`DuplicateEdge`](Self::DuplicateEdge).
    #[error("{system} input for port {port:?} connected more than once")]
    DoubleConnect { system: String, port: PortId },
    /// The exact same fan-in edge, one `(producer, consumer, port)` triple, was
    /// connected twice. Fan-in of distinct producers is legal; a copy-pasted
    /// duplicate edge would deliver every record to the consumer twice (a
    /// double-applied command), so it is rejected.
    #[error(
        "duplicate edge {producer} -> {consumer} on port {port:?} — the same edge was connected twice (every record would deliver twice)"
    )]
    DuplicateEdge {
        producer: String,
        consumer: String,
        port: PortId,
    },
    /// `connect_delayed` on an edge into a [`Delivery::Log`](crate::Delivery)
    /// input. `delayed` marks a one-cycle-late snapshot sample; a log is a
    /// decoupled event/command stream with no same-cycle dependency, so the
    /// delay is meaningless and rejected instead of silently ignored.
    #[error(
        "delayed edge {producer} -> {consumer} on Log port {port:?}: `delayed` marks a one-cycle-late snapshot sample, which is meaningless on a decoupled event/command log — drop the delayed flag"
    )]
    DelayedLogEdge {
        producer: String,
        consumer: String,
        port: PortId,
    },
    /// An input declared [`FanIn::Many`](crate::FanIn) with
    /// [`Delivery::Snapshot`](crate::Delivery). Latest-wins across several
    /// producers is ill-defined without cross-ring ordering, so the combination
    /// is rejected.
    #[error(
        "{system} input {port:?} declares FanIn::Many with Delivery::Snapshot: latest-wins across several producers is ill-defined — use Delivery::Log or FanIn::One"
    )]
    SnapshotFanIn { system: String, port: PortId },
    /// An edge targets a host-connected input (`PortConn::Host`/`SelfTap`).
    /// Its counterpart is held by the system's runner, never an edge; a slot
    /// occupant's `slot_control` is written by `Abort`, not by another system.
    #[error(
        "{system} input {port:?} is host-connected: its counterpart is held by the system's runner, not an edge — remove the edge"
    )]
    HostPort { system: String, port: PortId },
    /// A non-delayed frame edge points backward between two scheduled cycle
    /// positions: the consumer would run before its producer every cycle and
    /// permanently read a stale value, exactly what `connect_delayed` exists
    /// to make explicit. Log edges are exempt, and async import/export
    /// boundaries count as scheduled positions.
    #[error(
        "{consumer} is registered before {producer} but consumes its {port:?} output: it would step first every cycle and permanently read the previous cycle's value — register {producer} before {consumer}, or declare the one-cycle delay with connect_delayed"
    )]
    StaleFrameEdge {
        producer: String,
        consumer: String,
        port: PortId,
    },
    /// The wall-clock rate must yield a nonzero, representable timer period.
    /// Simulated clocks ignore the rate.
    #[error(
        "cycle_rate {rate} must be finite and positive and yield a nonzero, representable timer period"
    )]
    InvalidCycleRate { rate: Hz },
    /// A port's depth, record size, or reader budget cannot fit a ring.
    #[error(
        "invalid ring size: record limit {max_size} bytes, depth {depth}, reader budget {max_readers}"
    )]
    InvalidRingSize {
        max_size: usize,
        depth: usize,
        max_readers: usize,
    },
    /// A simulated clock must advance on every cycle.
    #[error("simulated clock step {dt:?} must be positive")]
    InvalidSimulatedStep { dt: std::time::Duration },
    /// A feedback loop was left unbroken: every cycle in the graph must break
    /// exactly one of its edges with `connect_delayed`, so the one-cycle-late
    /// sampling is explicit rather than an artifact of registration order.
    /// `systems` names the cycle members in loop order.
    #[error(
        "unbroken feedback cycle {cycle} — break one edge with connect_delayed",
        cycle = .systems.join(" -> ")
    )]
    FeedbackCycle { systems: Vec<String> },
    /// Two registered buffers computed the same instance-qualified registry key
    /// `"<instance>.<name>"`. Frames and channels share one keyspace, so the
    /// collision is detectable instead of silently shadowing one entry.
    #[error(
        "two buffers share the registry key {key:?} — rename one instance or port so every '<instance>.<name>' is unique"
    )]
    DuplicateRegistryKey { key: String },
    /// A slot instance name exceeds [`NAME_CAP`] bytes. Slot names are the
    /// sequence channels' wire address (`SequenceCommand::channel`) and must
    /// also round-trip losslessly into the fixed-size frames that carry them
    /// (`SlotStatus::occupant`, the coordinator status entries). A longer name
    /// would telemeter truncated while addressing untruncated, so it is
    /// rejected at build instead of silently truncated.
    #[error(
        "slot instance name {name:?} is {len} bytes; the sequence-channel wire address is capped at {max} bytes",
        max = NAME_CAP
    )]
    SlotNameTooLong { name: String, len: usize },
    /// A system without a receive-all port was registered after one that has
    /// it (the telemetry downlink). The downlink's end-of-cycle snapshot only
    /// sees systems that stepped before it, so a later registration would
    /// telemeter one cycle stale; enforced rather than silently reordered,
    /// since reordering would change the step order the stale-edge
    /// diagnostics validate. Async boundaries count too, since they export at
    /// their registered position.
    #[error(
        "system '{system}' is registered after the receive-all system '{receive_all}' (the telemetry downlink), whose end-of-cycle snapshot would miss it; register '{system}' before the telemetry downlink"
    )]
    ReceiveAllNotLast { system: String, receive_all: String },
    /// The run's shared-memory session (the mmap ring files process systems
    /// exchange data over) could not be set up.
    #[error("cannot set up the shared-memory session: {detail}")]
    Shm { detail: String },
    /// A process system's worker could not be spawned or never attached.
    /// `system` is the instance name; `detail` carries the cause, including
    /// the worker's own failure code when it reported one.
    #[error("process system '{system}': {detail}")]
    ProcSpawn { system: String, detail: String },
    /// A wired wasm system could not be instantiated, created, or bound.
    /// `system` is the instance name; `detail` carries the guest-side cause.
    #[error("wasm system '{system}': {detail}")]
    WasmBind { system: String, detail: String },
}
