//! Cyclic-slot status vocabulary: lifecycle states, stop reasons, and the
//! worker facts the coordinator publishes, plus the fixed-cap name packing the
//! status frames share.

use std::sync::Arc;

use metor_proto::types::Timestamp;

/// Why a cyclic slot hard-stopped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StopReason {
    /// A loaded system panicked inside its `.so`; the boundary caught it and
    /// returned [`FswStatus::Panicked`](crate::abi::FswStatus). Only reachable
    /// for a [`DlSlot`](crate::dl) or its process twin; a static
    /// `CyclicRunner` cannot produce it (a panic there unwinds the host
    /// directly).
    Panicked,
    /// A process system's worker died (crashed, was killed, or exited on its
    /// own). Its ring roles were reclaimed, so the rest of the graph keeps
    /// flowing; the stop is permanent, like a panic.
    ProcessDied,
}

impl StopReason {
    pub(super) fn code(self) -> u8 {
        match self {
            StopReason::Panicked => 0,
            StopReason::ProcessDied => 1,
        }
    }
}

/// Where a cyclic slot sits in its lifecycle, from empty through running to
/// done or hard-stopped. A static
/// [`CyclicRunner`](crate::CyclicRunner) and a build-time
/// [`DlSlot`](crate::dl::DlSlot) only ever inhabit `Running`/`Stopped` (once
/// `Stopped` they are never cleared; a sequence-mode worker's `DlSlot` also
/// latches `Done`, its poll-once guard); a process slot's `Stopped` clears
/// back to `Running` when its worker restarts (`docs/process-systems.md`
/// §6); the runtime [`SlotRunner`](slot) uses all six — `Loading` only in
/// process mode — and recovers from a terminal state via `Load`/`Reset`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SlotState {
    /// No occupant; `step` is a cheap no-op. (Runtime slots only.)
    Empty,
    /// An occupant is created and bound (its future built) but not yet polling.
    /// After a hard-drop `Stop` the state returns to `Loaded` with no live
    /// future. (Runtime slots only.)
    Loaded,
    /// A process slot's occupant worker is mid-pipeline — spawn, attach,
    /// bind/init — polled forward once per cycle, so a `Load` never stalls
    /// the loop. Ends at `Loaded` (and its event) when the worker reports
    /// bound, or `Stopped` on a pipeline failure. The command guards need no
    /// new cases: `Load`/`Start`/`Stop` match none of their accepted states
    /// here, so a command arriving mid-pipeline is refused (with a `Refused`
    /// event). (Runtime process slots only.)
    Loading,
    /// The slot is polled every cycle.
    Running,
    /// The occupant's future returned `Ready`. Terminal success, not an
    /// error-stop; the `Completed`/`Aborted`/`Failed` detail rides the
    /// occupant's own [`SequenceStatus`](crate::sequence::SequenceStatus)
    /// frame, and `outcome` is its latched `run_state` byte. (Runtime slots
    /// only.)
    Done { outcome: u8 },
    /// Hard-stopped; `reason` says why.
    Stopped { reason: StopReason },
}

impl SlotState {
    /// The projection the coordinator's stopped-systems status uses: only a
    /// hard stop is an error-stop (`Done`/`Empty`/`Loaded` are not).
    pub fn stop_reason(&self) -> Option<StopReason> {
        match self {
            SlotState::Stopped { reason } => Some(*reason),
            _ => None,
        }
    }

    /// The wire phase code published in [`SlotStatus::phase`], in lifecycle
    /// order (Empty=0/Loaded=1/Loading=2/Running=3/Done=4/Stopped=5).
    pub fn code(&self) -> u8 {
        match self {
            SlotState::Empty => 0,
            SlotState::Loaded => 1,
            SlotState::Loading => 2,
            SlotState::Running => 3,
            SlotState::Done { .. } => 4,
            SlotState::Stopped { .. } => 5,
        }
    }

    pub fn is_stopped(&self) -> bool {
        self.stop_reason().is_some()
    }

    /// The phase name used in operator-facing messages (refusal reasons).
    pub fn name(&self) -> &'static str {
        match self {
            SlotState::Empty => "empty",
            SlotState::Loaded => "loaded",
            SlotState::Loading => "loading",
            SlotState::Running => "running",
            SlotState::Done { .. } => "done",
            SlotState::Stopped { .. } => "stopped",
        }
    }
}

/// The name and stop reason of one hard-stopped cyclic system, surfaced
/// through [`Coordinator::stopped`] and the coordinator status frame.
#[derive(Clone, Debug)]
pub struct StoppedSystem {
    pub name: Arc<str>,
    pub reason: StopReason,
}

/// One position in the cyclic step loop. The coordinator inits, steps, and
/// shuts it down without knowing the concrete system inside;
/// [`CyclicRunner`](crate::CyclicRunner) is the typed implementation.
pub(crate) trait CyclicSlot {
    fn init(&mut self);
    fn step(&mut self, now: Timestamp);
    fn shutdown(&mut self);
    fn name(&self) -> &str;
    fn state(&self) -> &SlotState;
    /// Host-side step timeouts since last drained. The coordinator folds them
    /// into its own health (the worker owns the system's health ring, so a
    /// process slot cannot report through it). Overridden by `ProcSlot` and
    /// the process-mode `SlotRunner`.
    fn drain_timeouts(&mut self) -> u64 {
        0
    }
    /// Worker restarts begun since last drained, folded into coordinator
    /// health like the timeouts. Only `ProcSlot` overrides: slot occupants
    /// never auto-restart, so the runner has nothing to report here.
    fn drain_restarts(&mut self) -> u64 {
        0
    }
    /// The worker-process facts behind this slot, for the status frame's
    /// worker list: `None` for every in-process slot, `Some` from `ProcSlot`
    /// and the process-mode `SlotRunner`.
    fn worker_status(&self) -> Option<WorkerStatus> {
        None
    }
}

/// Where a process system's worker is in its life, as telemetered in the
/// status frame's worker list and [`Coordinator::workers`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkerRunState {
    /// Dead past the restart budget; the stop is permanent.
    Stopped,
    /// Dead or half-born, inside the restart pipeline.
    Restarting,
    /// Alive and being stepped.
    Running,
}

impl WorkerRunState {
    /// The wire code carried in the status frame's worker entries
    /// (Stopped=0 / Restarting=1 / Running=2).
    pub fn code(self) -> u8 {
        match self {
            WorkerRunState::Stopped => 0,
            WorkerRunState::Restarting => 1,
            WorkerRunState::Running => 2,
        }
    }
}

/// One process system's worker facts, as reported by
/// [`Coordinator::workers`]: the instance name, the worker pid (this is how
/// an operator learns a system runs out-of-process, and where), the restart
/// count, and the run state. The same facts ride the status frame's
/// `workers` list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerStatus {
    pub name: Arc<str>,
    /// The live worker's pid, or `0` between workers.
    pub pid: u32,
    pub restarts: u32,
    pub state: WorkerRunState,
}

/// The one shared byte cap on every name packed into a fixed-size host frame
/// (a stopped system in [`CoordinatorStatus`], the occupant in [`SlotStatus`])
/// and the validated cap on slot instance names, which double as the sequence
/// channels' wire address (`SequenceCommand::channel`). Matches
/// [`SEQUENCE_CHANNEL_NAME_CAP`](metor_proto_wkt::SEQUENCE_CHANNEL_NAME_CAP).
pub const NAME_CAP: usize = 48;

// The build-validated cap and the wire protocol's documented cap are one invariant.
const _: () = assert!(NAME_CAP == metor_proto_wkt::SEQUENCE_CHANNEL_NAME_CAP);
