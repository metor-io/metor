//! Runtime-swappable sequence occupants.
//!
//! [`SlotRunner`] accepts `Load`, `Start`, `Stop`, `Abort`, and `Reset` commands.
//! The allowed occupants and their port contracts are checked at build time.
//! The coordinator owns the rings; unloading an occupant releases its reader
//! and writer claims so its replacement can bind to the same regions.
//!
//! The occupant's ports come first in descriptor order. The runner adds command
//! inputs, a self-tap on [`SequenceStatus`], and status/event outputs. It owns
//! the writer for the occupant's [`SlotControlIn`] cancellation input.
//!
//! `Load` creates an occupant in `Loaded`; `Start` begins polling it. Process
//! occupants first pass through `Loading`, with one startup step per cycle.
//! `Stop` drops the live occupant but keeps the selected entry in `Loaded`;
//! `Reset` or `Load` must recreate it before another `Start`.
//!
//! Commands are drained before polling. Each transition emits a
//! [`SequenceChannelEvent`], including `Refused` for invalid commands. Events
//! preserve transitions that a status snapshot could miss within one cycle.

use core::mem::offset_of;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use metor_fsw_ring::RingBuffer;

use metor_proto::types::Timestamp;
use metor_proto_wkt::{
    SequenceChannelEvent, SequenceCommand, SequenceCommandKind, SequenceEventKind,
};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use metor_fsw_ring::NoWake;

use crate::FrameStr;
use crate::dl::DlSlot;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use crate::proc::StepOutcome;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use crate::proc::host::{LoadPoll, SeqWorker};
use metor_fsw_2_core::abi::FswStatus;
use metor_fsw_2_core::sequence::{PROGRESS_MSG_CAP, ProgressLine, SequenceStatus, SlotControlIn};
use metor_fsw_2_core::{CyclicSlot, NAME_CAP, SlotState, StopReason, WorkerRunState, WorkerStatus};
use metor_fsw_2_core::{Input, Output, frame_list_iter};
use metor_fsw_2_core::{MsgIn, MsgOut};

/// A fixed frame the host publishes every cycle carrying the slot phase and
/// the selected occupant's name. Occupant-side detail such as progress lines
/// and the terminal outcome rides the occupant's own
/// [`SequenceStatus`](metor_fsw_2_core::sequence::SequenceStatus) frame.
#[derive(crate::Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
#[metor_fsw(name = "slot_status")]
pub struct SlotStatus {
    #[metor_fsw(timestamp)]
    pub timestamp: Timestamp,
    /// The current [`SlotState::code`]
    /// (Empty=0/Loaded=1/Loading=2/Running=3/Done=4/Stopped=5).
    pub phase: u8,
    pub _pad: [u8; 7],
    /// The selected occupant's name, empty when no occupant is selected.
    #[metor_fsw(nest)]
    pub occupant: FrameStr<NAME_CAP>,
}

mod plan;
pub use plan::{AllowedOccupant, InitialOccupant, OccupantBacking, SlotConfigError};
pub(crate) use plan::{
    DEFAULT_FUEL_PER_CALL, SlotPorts, SlotReg, WASM_SETUP_FUEL, plan_slot, validate_slot_spec,
};

/// The live occupant, one variant per backing: the dl occupant is a future
/// polled in this process, the proc occupant a worker process driven over
/// the ctl block. Dropping either releases everything it held (the dl
/// occupant's `Drop` runs `fsw_pack_destroy`, the worker's kills, reaps, and
/// reclaims), so `None`-ing the field is the one teardown spelling for both.
enum Occupant {
    Dl(DlSlot),
    Wasm(Box<crate::wasm::WasmSlot>),
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    Proc(SeqWorker),
}

/// The spawn ingredients a process slot keeps between Loads, gathered by the
/// bind pass: one written manifest per allowed occupant (the rings are the
/// slot's, so the manifests differ only in artifact and params), the shared
/// ctl path and resolved worker executable, the host handles of every
/// worker-attached ring (the reclaim set each worker's teardown sweeps), and
/// the step deadline. Present exactly when the slot is process-mode.
pub(crate) struct ProcParts {
    /// Per-occupant manifests, indexed like the allowed set.
    pub manifests: Vec<PathBuf>,
    pub ctl_path: PathBuf,
    pub exe: PathBuf,
    pub rings: Vec<RingBuffer>,
    pub step_timeout: Duration,
}

/// A slot runner drives one swappable position in the coordinator's schedule,
/// creating, polling, and destroying occupants in response to runtime
/// commands. It holds the per-port [`FswRing`](metor_fsw_2_core::abi::FswRing) templates,
/// the [`AllowedOccupant`] set, the live occupant, the [`SlotState`], and the
/// host-owned control and status writers. No occupant exists after `build()`;
/// the first one is created at `init` or by a runtime `Load` (see the module
/// docs for the full lifecycle).
pub(crate) struct SlotRunner {
    /// The slot's instance name, its identity and command address.
    name: Arc<str>,
    allowed: Vec<AllowedOccupant>,
    /// Applied once at [`init`](CyclicSlot::init); `None` after.
    initial: Option<InitialOccupant>,
    /// Ring templates for the occupant inputs, in occupant input order,
    /// ending with the control ring. Cloned per occupant; the regions are
    /// stable and the descriptors are `Copy`.
    input_regions: Vec<metor_fsw_2_core::abi::FswRing>,
    /// Ring templates for the occupant outputs, in occupant output order.
    output_regions: Vec<metor_fsw_2_core::abi::FswRing>,
    /// Interpreter fuel granted to each guest call, for a wasm-backed slot.
    ///
    /// This is the knob that makes a poll *bounded*: a guest that will not stop
    /// is cut off mid-instruction and reported, where a natively linked
    /// occupant would simply never return and stall the cycle. Unused by the
    /// other backings.
    fuel_per_call: u64,
    /// Maximum guest linear memory during setup; memory is frozen after bind.
    max_wasm_memory: usize,
    /// Host writer over the control ring; `Abort` writes a cancel frame here.
    control: Output<SlotControlIn>,
    /// Host writer over the [`SlotStatus`] ring, written each `step`.
    status_out: Output<SlotStatus>,
    /// The slot's events channel. The runner is the ring's sole writer,
    /// emitting a [`SequenceChannelEvent`] at every lifecycle transition and
    /// one `Progress` event per occupant progress line.
    events: MsgOut<SequenceChannelEvent>,
    /// Read view over the occupant's own [`SequenceStatus`] output, sourcing
    /// `Progress` events and the terminal outcome the raw status word cannot
    /// carry.
    seq_status: Input<SequenceStatus>,
    /// The slot's command fan-in, drained at the head of each
    /// [`step`](CyclicSlot::step) and filtered by instance name, so an
    /// addressed command dispatches the cycle it arrives.
    commands: MsgIn<SequenceCommand>,
    /// The latest occupant `run_state` drained while running, used to refine
    /// a terminal `Done` into completed, aborted, or failed.
    last_run_state: u8,
    /// The lifecycle state the coordinator reads through [`CyclicSlot::state`].
    state: SlotState,
    /// The live occupant, or `None` when empty or after a hard `Stop`.
    slot: Option<Occupant>,
    /// The process-mode spawn ingredients; `None` for an in-process slot,
    /// and the seam `worker_status` keys "is there a worker behind this slot" on.
    proc: Option<ProcParts>,
    /// Occupant-worker steps whose ack deadline lapsed with the worker still
    /// alive, since the coordinator last drained them onto its log.
    timeouts: u64,
    /// Unplanned worker deaths (pipeline failures included) over the slot's
    /// life, telemetered in the worker list's `restarts` field: Loads are
    /// commanded, deaths are the anomaly worth counting.
    deaths: u32,
    /// Index of the live or last occupant in `allowed`, for `Reset` and
    /// status naming.
    selected: Option<usize>,
    /// The last `now` seen in `step`, used to stamp the cancel frame an
    /// `Abort` writes (command handling carries no timestamp of its own).
    last_now: Timestamp,
    /// Command buffer reused across steps so the steady state allocates nothing.
    cmd_scratch: Vec<SequenceCommand>,
    /// Progress-detail buffer reused across steps; the strings themselves
    /// are fresh, only the carrying `Vec` is retained.
    detail_scratch: Vec<String>,
}

impl SlotRunner {
    /// Assemble a slot from its build products. No occupant is created here;
    /// that happens at `init` or on a `Load`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        name: Arc<str>,
        allowed: Vec<AllowedOccupant>,
        initial: Option<InitialOccupant>,
        input_regions: Vec<metor_fsw_2_core::abi::FswRing>,
        output_regions: Vec<metor_fsw_2_core::abi::FswRing>,
        control: Output<SlotControlIn>,
        status_out: Output<SlotStatus>,
        events: MsgOut<SequenceChannelEvent>,
        seq_status: Input<SequenceStatus>,
        commands: MsgIn<SequenceCommand>,
        proc: Option<ProcParts>,
        fuel_per_call: u64,
        max_wasm_memory: usize,
    ) -> Self {
        Self {
            name,
            allowed,
            initial,
            input_regions,
            output_regions,
            fuel_per_call,
            max_wasm_memory,
            control,
            status_out,
            events,
            seq_status,
            commands,
            last_run_state: 0,
            state: SlotState::Empty,
            slot: None,
            proc,
            timeouts: 0,
            deaths: 0,
            selected: None,
            last_now: Timestamp(0),
            cmd_scratch: Vec::new(),
            detail_scratch: Vec::new(),
        }
    }

    /// Create the selected occupant behind the backing seam. The caller
    /// drops any previous occupant first (for a worker that drop *is* the
    /// kill/reap/reclaim, so the fresh occupant claims ring roles only after
    /// the old ones were freed). The dl arm binds synchronously and lands
    /// `Loaded`; the proc arm spawns the occupant's worker and lands
    /// `Loading` (announced with a `Loading` event), advanced one pipeline
    /// phase per step so a Load never stalls the cycle loop. The `Loaded`
    /// event fires when the occupant is actually bound, immediately here for
    /// dl, from the pipeline for proc.
    fn build_occupant(&mut self, idx: usize) {
        let occ = &self.allowed[idx];
        match &occ.backing {
            OccupantBacking::Dl(system) => {
                // SAFETY: every template region is a coordinator-owned ring
                // that outlives this runner (the coordinator drops runners
                // before rings), and any previous occupant has already
                // released its non-owning handles by dropping, so the roles
                // are free to re-attach.
                let mut slot = unsafe {
                    system.make_slot(
                        &occ.params,
                        self.input_regions.clone(),
                        self.output_regions.clone(),
                        &self.name,
                        &self.name,
                        crate::Mount::SlotOccupant,
                    )
                };
                slot.init();
                self.slot = Some(Occupant::Dl(slot));
                self.state = SlotState::Loaded;
                let name = self.allowed[idx].name.clone();
                self.emit_event(SequenceEventKind::Loaded { name });
            }
            OccupantBacking::Wasm {
                path,
                entry_identity,
            } => {
                let path = path.clone();
                let entry_identity = entry_identity.clone();
                let name = occ.name.clone();
                let params = occ.params.clone();
                match self.build_wasm(&path, &name, &entry_identity, &params) {
                    Ok(slot) => {
                        self.slot = Some(Occupant::Wasm(Box::new(slot)));
                        self.state = SlotState::Loaded;
                        self.emit_event(SequenceEventKind::Loaded { name });
                    }
                    Err(detail) => {
                        // A module that cannot be loaded or bound is a load
                        // failure, not a running occupant that died: the slot
                        // stays empty and the operator is told why.
                        self.slot = None;
                        self.state = SlotState::Empty;
                        tracing::error!(occupant = %name, error = %detail, "wasm occupant failed to load");
                        self.emit_event(SequenceEventKind::Failed {
                            reason: format!("wasm load failed: {detail}"),
                        });
                    }
                }
            }
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            OccupantBacking::Artifact(_) => {
                let parts = self
                    .proc
                    .as_ref()
                    .expect("a process slot binds its ProcParts");
                match SeqWorker::spawn(
                    &parts.exe,
                    &parts.ctl_path,
                    &parts.manifests[idx],
                    parts.rings.clone(),
                    parts.step_timeout,
                ) {
                    Ok(worker) => {
                        self.slot = Some(Occupant::Proc(worker));
                        self.state = SlotState::Loading;
                        let name = self.allowed[idx].name.clone();
                        self.emit_event(SequenceEventKind::Loading { name });
                    }
                    Err(detail) => self.fail_occupant(format!("worker spawn failed: {detail}")),
                }
            }
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            OccupantBacking::Artifact(_) => {
                unreachable!("build() rejects process slots on targets without a shared futex")
            }
        }
    }

    /// Load a wasm module and bind entry `idx` to the slot's rings.
    ///
    /// The module is read fresh on every `Load`, unlike the dl backing which
    /// keeps its library open across swaps: a `.wasm` is bytes rather than a
    /// mapped object, so re-reading costs a file read and buys the operator a
    /// genuinely fresh instance.
    fn build_wasm(
        &mut self,
        path: &std::path::Path,
        entry_name: &str,
        entry_identity: &[u8],
        params: &[u8],
    ) -> Result<crate::wasm::WasmSlot, String> {
        let wasm = std::fs::read(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
        crate::wasm::WasmSlot::bind_compatible(
            &wasm,
            entry_name,
            entry_identity,
            params,
            &self.name,
            &self.input_regions,
            &self.output_regions,
            WASM_SETUP_FUEL,
            self.fuel_per_call,
            self.max_wasm_memory,
        )
        .map_err(|e| e.to_string())
    }

    /// A worker failed the slot, mid-pipeline or mid-run: land the terminal
    /// stop and tell the operator why. No auto-restart, deliberately, since
    /// re-running a sequence would silently re-issue every command it
    /// already sent; recovery stays with the operator's existing
    /// `Reset`/`Load`.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn fail_occupant(&mut self, reason: String) {
        self.slot = None;
        self.deaths += 1;
        self.state = SlotState::Stopped {
            reason: StopReason::ProcessDied,
        };
        self.emit_event(SequenceEventKind::Failed { reason });
    }

    /// The load pipeline completed: the occupant is bound, which is exactly
    /// what `Loaded` (and its event) mean in-process.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn occupant_bound(&mut self) {
        self.state = SlotState::Loaded;
        let idx = self.selected.expect("a loading slot has a selection");
        let name = self.allowed[idx].name.clone();
        self.emit_event(SequenceEventKind::Loaded { name });
    }

    /// Advance a proc occupant's load pipeline one phase per cycle. A
    /// pipeline failure is the slot's terminal stop, named stage and all.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn poll_loading(&mut self) {
        if !matches!(self.state, SlotState::Loading) {
            return;
        }
        let Some(Occupant::Proc(worker)) = self.slot.as_mut() else {
            return;
        };
        match worker.poll_load() {
            LoadPoll::Pending => {}
            LoadPoll::Ready => self.occupant_bound(),
            LoadPoll::Failed { stage } => self.fail_occupant(format!("worker {stage} failed")),
        }
    }

    /// Block out a proc occupant's initial load inside the coordinator's
    /// init barrier, since init is not cycle time and `initial
    /// state="running"` needs the occupant `Loaded` before its `Start` can
    /// apply. A failure folds exactly as the polled pipeline's would.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn finish_initial_load(&mut self) {
        if !matches!(self.state, SlotState::Loading) {
            return;
        }
        let Some(Occupant::Proc(worker)) = self.slot.as_mut() else {
            return;
        };
        match worker.wait_ready() {
            Ok(()) => self.occupant_bound(),
            Err(stage) => self.fail_occupant(format!("worker {stage} failed")),
        }
    }

    /// Emit a [`SequenceChannelEvent`] tagged with this slot's instance name.
    /// Best effort; a full ring drops the event rather than blocking the cycle.
    fn emit_event(&mut self, kind: SequenceEventKind) {
        match &kind {
            SequenceEventKind::Failed { reason } => {
                tracing::error!(slot = %self.name, %reason, "slot occupant failed")
            }
            SequenceEventKind::Refused { reason } => {
                tracing::warn!(slot = %self.name, %reason, "slot command refused")
            }
            kind => tracing::info!(slot = %self.name, event = ?kind, "slot event"),
        }
        let _ = self.events.emit(&SequenceChannelEvent {
            channel: self.name.to_string(),
            kind,
        });
    }

    /// Apply a runtime command addressed to this slot. Every slot's fan-in
    /// sees every command producer, so dispatch filters by instance name; a
    /// command naming no slot matches nothing anywhere and is dropped.
    fn apply_command(&mut self, cmd: &SequenceCommand) {
        if cmd.channel != *self.name {
            return;
        }
        match &cmd.command {
            SequenceCommandKind::Load { name } => self.do_load(name),
            SequenceCommandKind::Start => self.do_start(),
            SequenceCommandKind::Stop => self.do_stop(),
            SequenceCommandKind::Abort => self.do_abort(),
            SequenceCommandKind::Reset => self.do_reset(),
        }
    }

    /// Select an allowed occupant by name and build it. Legal from any state
    /// except a running or mid-load occupant, which must stop first;
    /// loading over a `Loaded` slot drops the current occupant and builds
    /// the named one. A name outside the allowed set leaves the state
    /// untouched and emits a `Failed` event naming the allowed set, so the
    /// operator sees the rejection instead of a silently stuck slot.
    fn do_load(&mut self, occupant: &str) {
        match self.state {
            SlotState::Running => {
                let reason =
                    format!("load `{occupant}` refused: an occupant is running; stop it first");
                self.emit_event(SequenceEventKind::Refused { reason });
                return;
            }
            SlotState::Loading => {
                let reason =
                    format!("load `{occupant}` refused: the previous load is still binding");
                self.emit_event(SequenceEventKind::Refused { reason });
                return;
            }
            _ => {}
        }
        let Some(idx) = self.allowed.iter().position(|a| a.name == occupant) else {
            let reason = format!(
                "unknown occupant `{occupant}` (allowed: {})",
                self.allowed
                    .iter()
                    .map(|a| a.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            self.emit_event(SequenceEventKind::Failed { reason });
            return;
        };
        // Drop any previous occupant's state before reusing the rings (for a
        // proc occupant, that drop kills and reclaims its worker first).
        self.slot = None;
        self.selected = Some(idx);
        self.last_run_state = 0;
        self.build_occupant(idx);
    }

    /// Begin polling. Only a loaded slot with a live future starts.
    fn do_start(&mut self) {
        if self.slot.is_some() && matches!(self.state, SlotState::Loaded) {
            self.state = SlotState::Running;
            self.emit_event(SequenceEventKind::Started);
            return;
        }
        let reason = if matches!(self.state, SlotState::Loaded) {
            "start refused: the occupant was stopped; Reset rebuilds it".to_string()
        } else {
            format!("start refused: slot is {}", self.state.name())
        };
        self.emit_event(SequenceEventKind::Refused { reason });
    }

    /// Hard-drop the occupant (a dl occupant's `Drop` runs `fsw_pack_destroy`,
    /// releasing the ring roles; a proc occupant's kills and reclaims its
    /// worker, since a sequence has nothing graceful to lose), leaving the
    /// slot loaded with nothing live behind it. Only a `Reset` can run it
    /// again.
    fn do_stop(&mut self) {
        if matches!(self.state, SlotState::Running) {
            self.slot = None;
            self.state = SlotState::Loaded;
            self.emit_event(SequenceEventKind::Stopped);
        } else {
            let reason = format!("stop refused: slot is {}", self.state.name());
            self.emit_event(SequenceEventKind::Refused { reason });
        }
    }

    /// Write a cancel frame the occupant folds at its next poll. Cancellation
    /// is cooperative; the state stays running until the future returns done.
    fn do_abort(&mut self) {
        if matches!(self.state, SlotState::Running) {
            let _ = self.control.write(&SlotControlIn {
                timestamp: self.last_now,
                cancel: 1,
                _pad: [0; 7],
            });
        } else {
            let reason = format!("abort refused: slot is {}", self.state.name());
            self.emit_event(SequenceEventKind::Refused { reason });
        }
    }

    /// Rebuild the selected occupant from the beginning. Legal from a
    /// terminal state or a post-Stop loaded slot; a running or mid-load
    /// occupant must be stopped first.
    fn do_reset(&mut self) {
        let Some(idx) = self.selected else {
            let reason = "reset refused: nothing has been loaded".to_string();
            self.emit_event(SequenceEventKind::Refused { reason });
            return;
        };
        if matches!(self.state, SlotState::Running | SlotState::Loading) {
            let reason = format!(
                "reset refused: slot is {}; stop it first",
                self.state.name()
            );
            self.emit_event(SequenceEventKind::Refused { reason });
            return;
        }
        self.slot = None;
        self.last_run_state = 0;
        // Reset re-arms to idle, so observers see the channel back at Loaded
        // (a proc occupant re-announces it when its fresh pipeline binds).
        self.build_occupant(idx);
    }

    /// Drain every occupant [`SequenceStatus`] record published since the
    /// last cycle, emitting one `Progress` event per line and latching the
    /// newest `run_state` for the terminal fold. Each record carries only the
    /// lines pushed that cycle (the occupant empties its buffer on publish),
    /// so nothing is coalesced or replayed.
    fn drain_progress(&mut self) {
        let mut details = core::mem::take(&mut self.detail_scratch);
        let mut state = self.last_run_state;
        let result = self.seq_status.drain(|f| {
            state = f.get().run_state;
            for line in
                frame_list_iter::<ProgressLine>(f.table(), offset_of!(SequenceStatus, progress))
            {
                let n = (line.len as usize).min(PROGRESS_MSG_CAP);
                if let Ok(s) = core::str::from_utf8(&line.msg[..n]) {
                    details.push(s.to_string());
                }
            }
        });
        if result.is_err() {
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            self.fail_occupant("corrupt sequence status ring".to_string());
            details.clear();
            self.detail_scratch = details;
            return;
        }
        self.last_run_state = state;
        for detail in details.drain(..) {
            self.emit_event(SequenceEventKind::Progress { detail });
        }
        self.detail_scratch = details;
    }

    /// Fold one polled status word into the slot state, the shared tail of
    /// both backings' step. Drains what the occupant just published before
    /// folding the terminal event, so observers see the final Progress lines
    /// ahead of the Completed/Aborted/Failed derived from them.
    fn fold_status(&mut self, st: FswStatus) {
        self.drain_progress();
        self.state = match st {
            FswStatus::Running => SlotState::Running,
            // The raw status word carries no outcome detail; refine the
            // terminal Done from the run_state latched out of the occupant's
            // status frames. A Done proc occupant's worker stays up, holding
            // its ring roles until Reset/Load/Unload like an in-process one.
            FswStatus::Done => {
                let kind = match self.last_run_state {
                    1 => SequenceEventKind::Completed,
                    2 => SequenceEventKind::Aborted,
                    _ => SequenceEventKind::Failed {
                        reason: "failed".to_string(),
                    },
                };
                self.emit_event(kind);
                SlotState::Done {
                    outcome: self.last_run_state,
                }
            }
            FswStatus::Panicked => {
                // A proc occupant's worker already quarantined the panic
                // (the foreign state was destroyed on its side, freeing its
                // ring roles); end the worker too rather than stepping a
                // corpse until Unload.
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                if matches!(self.slot, Some(Occupant::Proc(_))) {
                    self.slot = None;
                }
                self.emit_event(SequenceEventKind::Failed {
                    reason: "panicked".to_string(),
                });
                SlotState::Stopped {
                    reason: StopReason::Panicked,
                }
            }
        };
    }

    /// Publish the host-side [`SlotStatus`] frame.
    fn publish_status(&mut self, now: Timestamp) {
        let occupant = match self.selected {
            Some(idx) => FrameStr::new(&self.allowed[idx].name),
            None => FrameStr::EMPTY,
        };
        let frame = SlotStatus {
            timestamp: now,
            phase: self.state.code(),
            _pad: [0; 7],
            occupant,
        };
        self.status_out.publish(&frame);
    }
}

impl CyclicSlot for SlotRunner {
    fn init(&mut self) {
        if let Some(initial) = self.initial.take() {
            self.do_load(&initial.occupant);
            // A proc occupant's pipeline completes inside the init barrier,
            // so the `start` below finds it Loaded before the first cycle.
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            self.finish_initial_load();
            if initial.start {
                self.do_start();
            }
        }
    }

    fn step(&mut self, now: Timestamp) {
        self.last_now = now;
        // Apply commands before stepping the occupant so a command lands the
        // cycle it arrives. The buffer is taken and returned around the loop
        // because apply_command needs `&mut self`, and retained across steps
        // so the steady state allocates nothing.
        let mut cmds = core::mem::take(&mut self.cmd_scratch);
        if self.commands.drain(|c| cmds.push(c)).is_err() {
            cmds.clear();
            self.cmd_scratch = cmds;
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            self.fail_occupant("corrupt sequence command ring".to_string());
            return;
        }
        for cmd in &cmds {
            self.apply_command(cmd);
        }
        cmds.clear();
        self.cmd_scratch = cmds;
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        self.poll_loading();
        self.publish_status(now);
        if matches!(self.state, SlotState::Running) {
            // Poll first, fold after: the fold emits events and rewrites the
            // state, which needs the occupant borrow released.
            let mut status = None;
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            let mut worker_died = false;
            match self.slot.as_mut() {
                Some(Occupant::Dl(slot)) => status = Some(slot.execute_raw(now)),
                Some(Occupant::Wasm(slot)) => status = Some(slot.execute_raw(now)),
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                Some(Occupant::Proc(worker)) => match worker.step(now) {
                    StepOutcome::Acked(st) => status = Some(st),
                    StepOutcome::TimedOut => {
                        if worker.child_dead() {
                            // Gone for certain; the ack never comes.
                            worker_died = true;
                        } else {
                            // Alive but late: telemetered, and the sequence
                            // protocol self-heals (the worker serves only
                            // the newest doorbell).
                            self.timeouts += 1;
                        }
                    }
                },
                None => {}
            }
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            if worker_died {
                self.fail_occupant("worker died".to_string());
            }
            if let Some(st) = status {
                self.fold_status(st);
            }
        }
    }

    fn shutdown(&mut self) {
        // Target teardown is the one graceful exit a proc occupant's worker
        // gets (shutdown request, grace window, then kill) unlike the
        // runtime `Stop`'s immediate kill; blocking is acceptable here.
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if let Some(Occupant::Proc(worker)) = self.slot.as_mut() {
            worker.shutdown();
        }
        self.slot = None;
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn state(&self) -> &SlotState {
        &self.state
    }

    fn drain_status_drops(&mut self) -> u64 {
        self.status_out.take_dropped()
    }

    fn drain_timeouts(&mut self) -> u64 {
        std::mem::take(&mut self.timeouts)
    }

    fn drain_boundary_drops(&mut self) -> u64 {
        match self.slot.as_mut() {
            Some(Occupant::Wasm(slot)) => slot.drain_dropped(),
            _ => 0,
        }
    }

    fn boundary_drop_kind(&self) -> &'static str {
        "wasm_boundary_dropped"
    }

    fn drain_boundary_corruptions(&mut self) -> u64 {
        match self.slot.as_mut() {
            Some(Occupant::Wasm(slot)) => slot.drain_corruptions(),
            _ => 0,
        }
    }

    fn worker_status(&self) -> Option<WorkerStatus> {
        self.proc.as_ref()?;
        let pid = match &self.slot {
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            Some(Occupant::Proc(worker)) => worker.pid(),
            _ => 0,
        };
        // "Half-born, inside the pipeline" describes a Loading occupant
        // exactly, hence `Restarting`; any live worker (running, idle, or
        // Done and holding its roles) reads `Running`, and between workers
        // the slot reads `Stopped`. The occupant-level story (which one,
        // what phase, the outcome) stays on `SlotStatus` and the events
        // channel; this list answers only "is there a live process here".
        let state = if matches!(self.state, SlotState::Loading) {
            WorkerRunState::Restarting
        } else if pid != 0 {
            WorkerRunState::Running
        } else {
            WorkerRunState::Stopped
        };
        Some(WorkerStatus {
            name: self.name.clone(),
            pid,
            restarts: self.deaths,
            state,
        })
    }
}

/// Mint the single host writer over a control, command, or status ring.
/// Cyclic slots are polled, never woken, hence [`NoWake`].
pub(crate) fn slot_writer<F: crate::Frame + IntoBytes + Immutable>(
    ring: &metor_fsw_ring::RingBuffer,
) -> Output<F> {
    // Each host-side ring gets its writer minted exactly once at build, so
    // the writer claim is always free here.
    let writer = ring
        .writer(NoWake)
        .expect("slot ring is bound to exactly one host writer at build");
    Output::new(writer)
}
