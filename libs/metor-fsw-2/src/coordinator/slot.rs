//! Runtime-swappable slot occupants.
//!
//! A slot is a cyclic system the coordinator drives every cycle like any
//! other, except that its occupant can be replaced while the mission runs.
//! Runtime commands `Load`, `Start`, `Stop`, `Abort`, and `Reset` the
//! occupant over the same create/execute/destroy ABI a statically wired
//! dynamic system uses; there are no extra lifecycle symbols. Occupants are
//! sequences, and the loadable set of [`AllowedOccupant`]s is fixed at build
//! time. Each candidate library is opened and validated up front, and `Load`
//! selects one by name.
//!
//! # Ring ownership
//!
//! The coordinator owns every ring for the whole mission. An occupant only
//! borrows writer and reader handles over them, so dropping the occupant
//! (its `Drop` runs `fsw_destroy`) releases the ring roles, and a later
//! `Load` attaches fresh handles over the same regions. [`SlotRunner`]
//! therefore keeps per-port ring templates and can create any number of
//! occupants over them, one at a time, each a fresh `fsw_create` state.
//!
//! # Registered ports
//!
//! The port lists the build pass sees are the occupant's ports followed by
//! the runner's. The occupant ports form the prefix of each list, in the
//! occupant descriptor's order, so binding hands the occupant its rings with
//! a straight prefix walk and the positional bind contract never changes.
//! Within that prefix the occupant's trailing [`SlotControlIn`] input is
//! host connected; the runner holds the writer and delivers `Abort` cancel
//! frames through it. The runner's own tail is a `commands`
//! [`MsgIn<SequenceCommand>`] fan-in plus a self-tap on the occupant's
//! [`SequenceStatus`] output on the input side, and the host [`SlotStatus`]
//! frame plus the slot's events message channel on the output side.
//!
//! # Lifecycle and events
//!
//! Each [`step`](CyclicSlot::step) drains and applies addressed commands
//! first, so a command lands the cycle it arrives, then publishes the
//! [`SlotStatus`] frame and polls the occupant once while the phase is
//! Running. Once the phase leaves Running the occupant is not polled again.
//!
//! [`SlotState`] tracks the phase, while the live occupant future is tracked
//! separately in [`SlotRunner`]'s `slot` field. A `Stop` hard-drops the
//! future and returns the state to Loaded with no future behind it, so
//! `Start` is rejected until a `Reset` rebuilds the occupant (or a `Load`
//! replaces it — only a running or mid-load occupant blocks `Load`). Every
//! lifecycle transition emits a [`SequenceChannelEvent`] on the slot's
//! events channel, tagged with the slot's instance name; the runner is that
//! ring's only writer, and a command the state machine rejects emits a
//! `Refused` event rather than being silently dropped, so operators never
//! see a dead click. Events are emitted per transition rather than derived
//! from the latest status frame, because two commands can apply in a single
//! drain and a snapshot would lose the first.
//!
//! # Process mode
//!
//! A slot whose occupants carry an [`OccupantBacking::Artifact`] runs them
//! **out of process** (`docs/process-slots.md`): `Load` spawns a worker
//! instead of calling `fsw_create` — one worker per occupant Load, driven
//! through the ctl lifecycle and torn down by kill + reclaim, the process
//! twin of the hard-drop — and the host never dlopens the occupant
//! artifacts. Everything above the occupant seam (commands, events, status,
//! the progress drain, the terminal fold) is the same code as in-process;
//! what differs is the [`Occupant`] variant behind it and one extra phase,
//! [`SlotState::Loading`], while the spawned worker's pipeline is polled
//! forward once per cycle.

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

use super::{CyclicSlot, NAME_CAP, SlotState, StopReason, WorkerRunState, WorkerStatus, pack_name};
use crate::abi::FswStatus;
use crate::descriptor::{PortConn, PortDesc, PortId, SystemDescriptor, SystemKind, compatible};
use crate::dl::{DlSlot, DlSystem};
use crate::message::{MsgIn, MsgOut};
use crate::port::{Input, Output, frame_list_iter};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use crate::proc::StepOutcome;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use crate::proc::host::{LoadPoll, SeqWorker};
use crate::sequence::{PROGRESS_MSG_CAP, ProgressLine, SequenceStatus, SlotControlIn};

// ---------------------------------------------------------------------------
// Host telemetry / control frames
// ---------------------------------------------------------------------------

/// A fixed frame the host publishes every cycle carrying the slot phase and
/// the selected occupant's name. Occupant-side detail such as progress lines
/// and the terminal outcome rides the occupant's own
/// [`SequenceStatus`](crate::sequence::SequenceStatus) frame.
#[derive(crate::Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
#[metor_fsw(name = "slot_status")]
pub struct SlotStatus {
    #[metor_fsw(timestamp)]
    pub timestamp: Timestamp,
    /// The current [`SlotState::code`]
    /// (Empty=0/Loaded=1/Loading=2/Running=3/Done=4/Stopped=5).
    pub phase: u8,
    /// Used length of `occupant`, zero when no occupant is selected.
    pub occ_len: u8,
    pub _pad: [u8; 6],
    /// The selected occupant's name, fixed buffer plus length.
    pub occupant: [u8; NAME_CAP],
}

// ---------------------------------------------------------------------------
// Allowed / initial occupants + the registration payload
// ---------------------------------------------------------------------------

/// Where an [`AllowedOccupant`]'s code lives and how `Load` reaches it.
pub enum OccupantBacking {
    /// An opened library in this process; `Load` runs `fsw_create` over the
    /// handle, which stays loaded across swaps.
    Dl(Box<DlSystem>),
    /// A built cdylib the occupant's **worker process** opens
    /// (`docs/process-slots.md`); the host keeps only the path and never
    /// loads the artifact itself. Slots mix backings never: `plan_slot`
    /// requires the whole allowed set on one side of the seam.
    Artifact(PathBuf),
}

/// A candidate occupant a slot is allowed to load, validated at build time.
/// `Load` selects it by `name`; `descriptor` is the self-description the
/// slot's contract was derived from and validated against (a dl open or a
/// describe worker sourced it, per the backing), and `params` is the
/// postcard blob `fsw_create` decodes.
pub struct AllowedOccupant {
    pub name: String,
    pub params: Vec<u8>,
    pub descriptor: SystemDescriptor,
    pub backing: OccupantBacking,
}

impl AllowedOccupant {
    /// An in-process occupant over an opened library.
    pub fn dl(name: impl Into<String>, system: DlSystem, params: Vec<u8>) -> Self {
        Self {
            name: name.into(),
            params,
            descriptor: system.descriptor().clone(),
            backing: OccupantBacking::Dl(Box::new(system)),
        }
    }
}

/// Names an [`AllowedOccupant`] to load at startup, so a slot comes up
/// populated instead of empty. [`SlotRunner::init`] applies it once, starting
/// the occupant too when `start` is set.
#[derive(Clone, Debug)]
pub struct InitialOccupant {
    /// The allowed-set occupant name to load at startup.
    pub occupant: String,
    /// Whether to also start it, so it is running from the first cycle.
    pub start: bool,
}

/// A slot registration `plan_slot` rejected. The wiring front-end maps these
/// onto its own `LoadError`s; a mission author's typo is not a library bug, so
/// none of these panic.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum SlotConfigError {
    #[error("a slot needs at least one allowed occupant")]
    Empty,
    #[error("initial occupant `{occupant}` is not in the allowed set (allowed: {allowed})")]
    UnknownInitial {
        occupant: String,
        /// The comma-joined allowed occupant names, for the message.
        allowed: String,
    },
    #[error(
        "a slot's occupants must share one backing: all in-process or all process-mode \
         (`Load` may not silently change the slot's fault domain)"
    )]
    MixedBacking,
    #[error(
        "allowed occupant `{occupant}` is incompatible with the slot contract \
         (derived from `{base}`)"
    )]
    OccupantMismatch { occupant: String, base: String },
    #[error(
        "occupant `{occupant}` declares `{port}` itself; the mount appends it \
         (a pre-pack artifact must be rebuilt)"
    )]
    ReservedPort {
        occupant: String,
        port: &'static str,
    },
    #[error(
        "allowed occupant `{occupant}` declares a capability; capability \
         systems are wired for the whole run, never slot occupants"
    )]
    CapabilityOccupant { occupant: String },
}

/// The pure-spec half of slot validation: a non-empty allowed set, and an
/// `initial` name inside it. The wiring gate runs it before any occupant
/// artifact is opened (a typo must fail without a dlopen); the descriptor-level
/// checks live in [`plan_slot`], next to the descriptors they need.
pub(crate) fn validate_slot_spec(
    allowed: &[&str],
    initial: Option<&str>,
) -> Result<(), SlotConfigError> {
    if allowed.is_empty() {
        return Err(SlotConfigError::Empty);
    }
    if let Some(init) = initial
        && !allowed.contains(&init)
    {
        return Err(SlotConfigError::UnknownInitial {
            occupant: init.to_string(),
            allowed: allowed.join(", "),
        });
    }
    Ok(())
}

/// Derive a slot's registered contract from its occupant set, the one place the
/// descriptor-level checks run: backing homogeneity (per-slot means
/// all-occupants, so a mixed set is
/// [`MixedBacking`](SlotConfigError::MixedBacking)), mutual occupant
/// compatibility against the first occupant's contract, the [`SlotPorts`] plan,
/// and the flattened registered [`SystemDescriptor`]. The returned `bool` is
/// whether the slot runs process-mode (an all-`Artifact` allow set).
///
/// The pure-spec checks ([`validate_slot_spec`]: a non-empty allow set, a valid
/// `initial`) are the caller's gate, so `allowed` is trusted non-empty here.
pub(crate) fn plan_slot(
    name: &str,
    allowed: &[AllowedOccupant],
) -> Result<(SystemDescriptor, SlotPorts, bool), SlotConfigError> {
    // Per-slot means all-occupants: the isolation boundary is the slot's
    // position in the cycle, and a mixed allow set would make `Load` silently
    // change the fault domain.
    let n_proc = allowed
        .iter()
        .filter(|a| matches!(a.backing, OccupantBacking::Artifact(_)))
        .count();
    if n_proc != 0 && n_proc != allowed.len() {
        return Err(SlotConfigError::MixedBacking);
    }
    if let Some(occ) = allowed
        .iter()
        .find(|a| !a.descriptor.capabilities.is_empty())
    {
        return Err(SlotConfigError::CapabilityOccupant {
            occupant: occ.name.clone(),
        });
    }
    let process = n_proc == allowed.len();
    // Every allowed occupant must share the contract; the slot sizes and
    // validates to the first occupant's descriptor (mutual subset).
    let base = &allowed[0].descriptor;
    for occ in &allowed[1..] {
        let d = &occ.descriptor;
        let ports_match = |a: &[PortDesc], b: &[PortDesc]| {
            a.len() == b.len()
                && a.iter()
                    .zip(b)
                    .all(|(x, y)| compatible(x, y) && compatible(y, x))
        };
        if !(ports_match(&d.inputs, &base.inputs) && ports_match(&d.outputs, &base.outputs)) {
            return Err(SlotConfigError::OccupantMismatch {
                occupant: occ.name.clone(),
                base: allowed[0].name.clone(),
            });
        }
    }
    let ports = SlotPorts::for_occupant(base, &allowed[0].name)?;
    // The registered descriptor name is the slot's instance name (a leaked
    // `&'static str` for the descriptor field and the `SlotRunner` identity).
    let leaked: &'static str = Box::leak(name.to_string().into_boxed_str());
    let registered = ports.registered(leaked);
    Ok((registered, ports, process))
}

/// A slot's registered ports as the three concepts they are, not the flat
/// positional lists the build pass consumes:
///
/// - the occupant's **user ports**, in its descriptor's order;
/// - the **mount-appended occupant tail** — the [`SlotControlIn`] cancel input
///   and the [`SequenceStatus`] output (a mount property, not descriptor
///   content: any entry can occupy a slot);
/// - the **runner tail**, which never crosses to the occupant.
///
/// The first two flatten into the occupant lists here because both cross to
/// the occupant, positionally, as the prefix of each registered list.
/// [`registered`](Self::registered) flattens prefix then tail into the
/// [`SystemDescriptor`] the build pass sees; edge resolution, registry keys,
/// and the occupant-side positional bind ABI all consume that flattening, so
/// its exact sequence is a compatibility surface (the
/// `registered_slot_descriptor_snapshot` test pins it).
pub(crate) struct SlotPorts {
    /// The occupant's ABI prefix: its user inputs plus the mount-appended,
    /// host-connected [`SlotControlIn`] (the runner holds the cancel writer).
    pub occupant_inputs: Vec<PortDesc>,
    /// The occupant's ABI prefix: its user outputs plus the mount-appended
    /// [`SequenceStatus`].
    pub occupant_outputs: Vec<PortDesc>,
    /// The runner's inputs: the `commands` fan-in (an ordinary edge input, so
    /// command wiring is ordinary message wiring) and the self-tap over the
    /// occupant's [`SequenceStatus`] output.
    pub tail_inputs: Vec<PortDesc>,
    /// The runner's outputs: the [`SlotStatus`] frame and the `"sequences"`
    /// events channel, both host-written and registry-tapped.
    pub tail_outputs: Vec<PortDesc>,
}

impl SlotPorts {
    /// Derive the slot's port plan from an occupant contract by extension,
    /// not surgery: the mount tail is appended to the occupant's own ports,
    /// then the runner tail is laid out after the prefix. Rejects an occupant
    /// that declares a mount-appended port itself (a pre-pack artifact).
    pub(crate) fn for_occupant(
        base: &SystemDescriptor,
        occupant: &str,
    ) -> Result<Self, SlotConfigError> {
        let control_id = PortId::Component(<SlotControlIn as crate::Frame>::FRAME_ID);
        let seq_status_id = PortId::Component(<SequenceStatus as crate::Frame>::FRAME_ID);
        let reserved = |port| SlotConfigError::ReservedPort {
            occupant: occupant.to_string(),
            port,
        };
        if base.inputs.iter().any(|p| p.id() == control_id) {
            return Err(reserved("SlotControlIn"));
        }
        if base.outputs.iter().any(|p| p.id() == seq_status_id) {
            return Err(reserved("SequenceStatus"));
        }
        let mut occupant_inputs = base.inputs.clone();
        occupant_inputs.push(PortDesc::of::<SlotControlIn>().with_conn(PortConn::Host));
        let mut occupant_outputs = base.outputs.clone();
        occupant_outputs.push(PortDesc::of::<SequenceStatus>());
        Ok(Self {
            occupant_inputs,
            occupant_outputs,
            tail_inputs: vec![
                PortDesc::msg::<SequenceCommand>(),
                PortDesc::of::<SequenceStatus>().with_conn(PortConn::SelfTap(seq_status_id)),
            ],
            tail_outputs: vec![
                PortDesc::of::<SlotStatus>().with_conn(PortConn::Host),
                PortDesc::msg_named::<SequenceChannelEvent>("sequences").with_conn(PortConn::Host),
            ],
        })
    }

    /// Flatten the plan into the registered descriptor: occupant prefix then
    /// runner tail, per direction. This sequence is load-bearing (see the
    /// type docs); extend the plan, never reorder it.
    pub(crate) fn registered(&self, name: &'static str) -> SystemDescriptor {
        SystemDescriptor {
            name: name.into(),
            kind: SystemKind::Cyclic,
            inputs: self
                .occupant_inputs
                .iter()
                .chain(&self.tail_inputs)
                .cloned()
                .collect(),
            outputs: self
                .occupant_outputs
                .iter()
                .chain(&self.tail_outputs)
                .cloned()
                .collect(),
            // Sequence occupants declare wired ports only (ReceiveAll is host-only).
            capabilities: Vec::new(),
        }
    }

    /// Registered-input index of the mount-appended [`SlotControlIn`], the
    /// last port of the occupant input prefix.
    pub(crate) fn control_in_idx(&self) -> usize {
        self.occupant_inputs.len() - 1
    }

    /// Registered-input index of the runner's `commands` fan-in, the first
    /// tail input.
    pub(crate) fn commands_in_idx(&self) -> usize {
        self.occupant_inputs.len()
    }

    /// Registered-output index of the mount-appended [`SequenceStatus`], the
    /// last port of the occupant output prefix (and the self-tap's target).
    pub(crate) fn seq_status_out_idx(&self) -> usize {
        self.occupant_outputs.len() - 1
    }

    /// Registered-output index of the runner's [`SlotStatus`], the first
    /// tail output.
    pub(crate) fn status_out_idx(&self) -> usize {
        self.occupant_outputs.len()
    }

    /// Registered-output index of the runner's `"sequences"` events channel,
    /// the second tail output.
    pub(crate) fn events_out_idx(&self) -> usize {
        self.occupant_outputs.len() + 1
    }
}

/// A slot's configuration as recorded at registration, held until `build()`
/// assembles the [`SlotRunner`] from it.
pub(crate) struct SlotReg {
    pub allowed: Vec<AllowedOccupant>,
    pub initial: Option<InitialOccupant>,
    /// The named port plan the registered descriptor was flattened from; the
    /// bind arm reads the occupant/tail split and the tail-port indices off
    /// it instead of re-deriving them by shape.
    pub ports: SlotPorts,
    /// Run occupants out of process (`docs/process-slots.md`): the occupant
    /// prefix's crossing rings — its outputs, its Edge inputs' producers, and
    /// the host control ring — are allocated as session-dir files a worker
    /// process can attach. The runner tail stays host-side either way.
    pub process: bool,
}

// ---------------------------------------------------------------------------
// SlotRunner
// ---------------------------------------------------------------------------

/// The live occupant, one variant per backing: the dl occupant is a future
/// polled in this process, the proc occupant a worker process driven over
/// the ctl block. Dropping either releases everything it held — the dl
/// occupant's `Drop` runs `fsw_destroy`, the worker's kills, reaps, and
/// reclaims — so `None`-ing the field is the one teardown spelling for both.
enum Occupant {
    Dl(DlSlot),
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
/// commands. It holds the per-port [`FswRing`](crate::abi::FswRing) templates,
/// the [`AllowedOccupant`] set, the live occupant, the [`SlotState`], and the
/// host-owned control and status writers. No occupant exists after `build()`;
/// the first one is created at `init` or by a runtime `Load`.
///
/// The live future is tracked separately from the state. After a hard-drop
/// `Stop` the state returns to `Loaded` but `slot` is `None`, so `Start` is
/// rejected and only a `Reset` rebuild can run the occupant again.
pub(crate) struct SlotRunner {
    /// The slot's instance name, its identity and command address.
    name: Arc<str>,
    allowed: Vec<AllowedOccupant>,
    /// Applied once at [`init`](CyclicSlot::init); `None` after.
    initial: Option<InitialOccupant>,
    /// Ring templates for the occupant inputs, in occupant input order,
    /// ending with the control ring. Cloned per occupant; the regions are
    /// stable and the descriptors are `Copy`.
    input_regions: Vec<crate::abi::FswRing>,
    /// Ring templates for the occupant outputs, in occupant output order.
    output_regions: Vec<crate::abi::FswRing>,
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
    /// alive, since the coordinator last drained them into its health.
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
        input_regions: Vec<crate::abi::FswRing>,
        output_regions: Vec<crate::abi::FswRing>,
        control: Output<SlotControlIn>,
        status_out: Output<SlotStatus>,
        events: MsgOut<SequenceChannelEvent>,
        seq_status: Input<SequenceStatus>,
        commands: MsgIn<SequenceCommand>,
        proc: Option<ProcParts>,
    ) -> Self {
        Self {
            name,
            allowed,
            initial,
            input_regions,
            output_regions,
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
    /// event fires when the occupant is actually bound — immediately here
    /// for dl, from the pipeline for proc.
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

    /// A worker failed the slot — mid-pipeline or mid-run: land the terminal
    /// stop and tell the operator why. No auto-restart, deliberately:
    /// re-running a sequence silently re-issues every command it already
    /// sent, a mission-level decision, so recovery stays with the operator's
    /// existing `Reset`/`Load`.
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
    /// init barrier — init is not cycle time, and `initial state="running"`
    /// needs the occupant `Loaded` before its `Start` can apply. A failure
    /// folds exactly as the polled pipeline's would.
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
    /// except a running or mid-load occupant, which must stop first —
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

    /// Hard-drop the occupant (a dl occupant's `Drop` runs `fsw_destroy`,
    /// releasing the ring roles; a proc occupant's kills and reclaims its
    /// worker — a sequence has nothing graceful to lose), leaving the slot
    /// loaded with nothing live behind it. Only a `Reset` can run it again.
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

    /// Emit the terminal event for a done fold from the latched `run_state`.
    /// A value of 1 means completed and 2 means aborted; anything else is a
    /// failure, and since [`SequenceStatus`] carries no reason string the
    /// reason is generic.
    fn emit_terminal_done(&mut self) {
        let kind = match self.last_run_state {
            1 => SequenceEventKind::Completed,
            2 => SequenceEventKind::Aborted,
            _ => SequenceEventKind::Failed {
                reason: "failed".to_string(),
            },
        };
        self.emit_event(kind);
    }

    /// Fold one polled status word into the slot state — the shared tail of
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
                self.emit_terminal_done();
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
        let (occupant, occ_len) = match self.selected {
            Some(idx) => pack_name(&self.allowed[idx].name),
            None => ([0u8; NAME_CAP], 0),
        };
        let frame = SlotStatus {
            timestamp: now,
            phase: self.state.code(),
            occ_len,
            _pad: [0; 6],
            occupant,
        };
        if self.status_out.write(&frame).is_err()
            && !matches!(self.state, SlotState::Stopped { .. })
        {
            self.fail_occupant("slot status output blocked".to_string());
        }
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
        // Mission teardown is the one graceful exit a proc occupant's worker
        // gets — shutdown request, grace window, then kill — unlike the
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

    fn drain_timeouts(&mut self) -> u64 {
        std::mem::take(&mut self.timeouts)
    }

    fn worker_status(&self) -> Option<WorkerStatus> {
        self.proc.as_ref()?;
        let pid = match &self.slot {
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            Some(Occupant::Proc(worker)) => worker.pid(),
            _ => 0,
        };
        // "Half-born, inside the pipeline" describes a Loading occupant
        // exactly, hence `Restarting`; any live worker — running, idle, or
        // Done and holding its roles — reads `Running`, and between workers
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
