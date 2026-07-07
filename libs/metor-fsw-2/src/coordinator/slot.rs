//! Runtime-swappable slot occupants.
//!
//! A slot is a cyclic system the coordinator drives every cycle like any
//! other, except that its occupant can be replaced while the mission runs.
//! Runtime commands `Load`, `Start`, `Stop`, `Abort`, and `Reset` the
//! occupant over the same create/execute/destroy ABI a statically wired
//! dynamic system uses; there are no extra lifecycle symbols. Occupants are
//! sequences, and the loadable set is fixed at build time. Each candidate
//! library is opened and validated up front, and `Load` selects one by name.
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
//! [`SlotState`] tracks the phase, while the live occupant future is tracked
//! separately in [`SlotRunner`]'s `slot` field. A `Stop` hard-drops the
//! future and returns the state to Loaded with no future behind it, so
//! `Start` is rejected until a `Reset` rebuilds the occupant. Every
//! lifecycle transition emits a [`SequenceChannelEvent`] on the slot's
//! events channel, tagged with the slot's instance name; the runner is that
//! ring's only writer. Events are emitted per transition rather than derived
//! from the latest status frame, because two commands can apply in a single
//! drain and a snapshot would lose the first.

use core::mem::offset_of;

use metor_proto::types::Timestamp;
use metor_proto_wkt::{
    SequenceChannelEvent, SequenceCommand, SequenceCommandKind, SequenceEventKind,
};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use metor_fsw_ring::NoWake;

use super::{CyclicSlot, NAME_CAP, SlotState, StopReason, pack_name};
use crate::abi::FswStatus;
use crate::dl::{DlSlot, DlSystem};
use crate::message::{MsgIn, MsgOut};
use crate::port::{Input, Output};
use crate::sequence::{PROGRESS_MSG_CAP, ProgressLine, SequenceStatus, SlotControlIn};

/// Capacity of one occupant name in the host frames below, the shared
/// [`NAME_CAP`](super::NAME_CAP).
pub const SLOT_NAME_CAP: usize = NAME_CAP;

// ---------------------------------------------------------------------------
// Host telemetry / control frames
// ---------------------------------------------------------------------------

/// Host-side slot telemetry, written every cycle. It carries the slot phase
/// and the selected occupant's name; occupant-side detail such as progress
/// lines and the terminal outcome rides the occupant's own
/// [`SequenceStatus`](crate::sequence::SequenceStatus) frame.
#[derive(crate::Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
#[metor_fsw(name = "slot_status")]
pub struct SlotStatus {
    #[metor_fsw(timestamp)]
    pub timestamp: Timestamp,
    /// The current [`SlotState::code`] (Empty=0/Loaded=1/Running=2/Done=3/Stopped=4).
    pub phase: u8,
    /// Used length of `occupant`, zero when no occupant is selected.
    pub occ_len: u8,
    pub _pad: [u8; 6],
    /// The selected occupant's name, fixed buffer plus length.
    pub occupant: [u8; SLOT_NAME_CAP],
}

// ---------------------------------------------------------------------------
// Allowed / initial occupants + the registration payload
// ---------------------------------------------------------------------------

/// One loadable occupant. `Load` selects it by `name`; `system` is the
/// already opened library, whose backing stays loaded across swaps, and
/// `params` is the postcard blob `fsw_create` decodes.
pub struct AllowedOccupant {
    pub name: String,
    pub system: DlSystem,
    pub params: Vec<u8>,
}

/// An occupant applied once at startup. [`SlotRunner::init`] loads it, and
/// starts it too when `start` is set.
#[derive(Clone, Debug)]
pub struct InitialOccupant {
    /// The allowed-set occupant name to load at startup.
    pub occupant: String,
    /// Whether to also start it, so it is running from the first cycle.
    pub start: bool,
}

impl InitialOccupant {
    /// An initial occupant that is loaded but not started.
    pub fn loaded(occupant: impl Into<String>) -> Self {
        Self {
            occupant: occupant.into(),
            start: false,
        }
    }
    /// An initial occupant that is loaded and started.
    pub fn running(occupant: impl Into<String>) -> Self {
        Self {
            occupant: occupant.into(),
            start: true,
        }
    }
}

/// The slot registration the builder records and `build()` turns into a
/// [`SlotRunner`].
///
/// `n_occ_inputs` and `n_occ_outputs` split the registered port lists into
/// the occupant prefix and the runner tail (see the module docs), so the
/// bind arm maps the occupant's rings with a straight prefix walk.
pub(crate) struct SlotReg {
    pub allowed: Vec<AllowedOccupant>,
    pub initial: Option<InitialOccupant>,
    /// Number of leading registered inputs that belong to the occupant
    /// (its user ports plus the host-connected [`SlotControlIn`]).
    pub n_occ_inputs: usize,
    /// Number of leading registered outputs that belong to the occupant
    /// (its user ports plus [`SequenceStatus`]/health/log).
    pub n_occ_outputs: usize,
}

// ---------------------------------------------------------------------------
// SlotRunner
// ---------------------------------------------------------------------------

/// The runtime-swappable slot the coordinator drives as a `Box<dyn CyclicSlot>`.
/// It holds the per-port [`FswRing`](crate::abi::FswRing) templates, the
/// allowed-occupant set, the live occupant, the [`SlotState`], and the
/// host-owned control and status writers. No occupant exists after `build()`;
/// the first one is created at `init` or by a runtime `Load`.
///
/// The live future is tracked separately from the state. After a hard-drop
/// `Stop` the state returns to `Loaded` but `slot` is `None`, so `Start` is
/// rejected and only a `Reset` rebuild can run the occupant again.
pub(crate) struct SlotRunner {
    /// The slot's instance name, its identity and command address.
    name: &'static str,
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
    /// Read view over the occupant's own [`SequenceStatus`] output, drained
    /// every cycle while running to source `Progress` events and the terminal
    /// outcome the raw status word cannot carry.
    seq_status: Input<SequenceStatus>,
    /// The slot's command fan-in, fed by exactly the producers explicitly
    /// wired to it (zero edges is a legal, command-less slot). Drained at the
    /// head of each [`step`](CyclicSlot::step) and filtered by instance name,
    /// so an addressed command dispatches the cycle it arrives.
    commands: MsgIn<SequenceCommand>,
    /// The latest occupant `run_state` drained while running, used to refine
    /// a terminal `Done` into completed, aborted, or failed.
    last_run_state: u8,
    /// The lifecycle state the coordinator reads through [`CyclicSlot::state`].
    state: SlotState,
    /// The live occupant future, or `None` when empty or after a hard `Stop`.
    slot: Option<DlSlot>,
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
        name: &'static str,
        allowed: Vec<AllowedOccupant>,
        initial: Option<InitialOccupant>,
        input_regions: Vec<crate::abi::FswRing>,
        output_regions: Vec<crate::abi::FswRing>,
        control: Output<SlotControlIn>,
        status_out: Output<SlotStatus>,
        events: MsgOut<SequenceChannelEvent>,
        seq_status: Input<SequenceStatus>,
        commands: MsgIn<SequenceCommand>,
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
            selected: None,
            last_now: Timestamp(0),
            cmd_scratch: Vec::new(),
            detail_scratch: Vec::new(),
        }
    }

    /// Create the selected occupant over the ring templates and build its
    /// future. The caller drops any previous occupant first.
    fn build_occupant(&mut self, idx: usize) {
        let occ = &self.allowed[idx];
        // SAFETY: every template region is a coordinator-owned ring that
        // outlives this runner (the coordinator drops runners before rings),
        // and any previous occupant has already released its non-owning
        // handles by dropping, so the roles are free to re-attach.
        let mut slot = unsafe {
            occ.system.make_slot(
                &occ.params,
                self.input_regions.clone(),
                self.output_regions.clone(),
                self.name,
            )
        };
        slot.init();
        self.slot = Some(slot);
    }

    /// Emit a [`SequenceChannelEvent`] tagged with this slot's instance name.
    /// Best effort; a full ring drops the event rather than blocking the cycle.
    fn emit_event(&mut self, kind: SequenceEventKind) {
        let _ = self.events.emit(&SequenceChannelEvent {
            channel: self.name.to_string(),
            kind,
        });
    }

    /// Apply a runtime command addressed to this slot. Every slot's fan-in
    /// sees every command producer, so dispatch filters by instance name; a
    /// command naming no slot matches nothing anywhere and is dropped.
    fn apply_command(&mut self, cmd: &SequenceCommand) {
        if cmd.channel != self.name {
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

    /// Select an allowed occupant by name and build it. Legal from empty or a
    /// terminal state only. A name outside the allowed set leaves the state
    /// untouched and emits a `Failed` event naming the allowed set, so the
    /// operator sees the rejection instead of a silently stuck slot.
    fn do_load(&mut self, occupant: &str) {
        if !matches!(
            self.state,
            SlotState::Empty | SlotState::Done { .. } | SlotState::Stopped { .. }
        ) {
            return; // a live or post-Stop Loaded slot is not re-Loadable
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
        // Drop any terminal occupant's state before reusing the rings.
        self.slot = None;
        self.build_occupant(idx);
        self.selected = Some(idx);
        self.state = SlotState::Loaded;
        self.last_run_state = 0;
        let name = self.allowed[idx].name.clone();
        self.emit_event(SequenceEventKind::Loaded { name });
    }

    /// Begin polling. Only a loaded slot with a live future starts.
    fn do_start(&mut self) {
        if self.slot.is_some() && matches!(self.state, SlotState::Loaded) {
            self.state = SlotState::Running;
            self.emit_event(SequenceEventKind::Started);
        }
    }

    /// Hard-drop the occupant's future (its `Drop` runs `fsw_destroy`,
    /// releasing the ring roles), leaving the slot loaded with no live
    /// future. Only a `Reset` can run it again.
    fn do_stop(&mut self) {
        if matches!(self.state, SlotState::Running) {
            self.slot = None;
            self.state = SlotState::Loaded;
            self.emit_event(SequenceEventKind::Stopped);
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
        }
    }

    /// Rebuild the selected occupant from the beginning. Legal from a
    /// terminal state or a post-Stop loaded slot; a running occupant must be
    /// stopped first.
    fn do_reset(&mut self) {
        let Some(idx) = self.selected else {
            return;
        };
        if matches!(self.state, SlotState::Running) {
            return; // stop a running occupant before resetting it
        }
        self.slot = None;
        self.build_occupant(idx);
        self.state = SlotState::Loaded;
        self.last_run_state = 0;
        // Reset re-arms to idle, so observers see the channel back at Loaded.
        let name = self.allowed[idx].name.clone();
        self.emit_event(SequenceEventKind::Loaded { name });
    }

    /// Drain every occupant [`SequenceStatus`] record published since the
    /// last cycle, emitting one `Progress` event per line and latching the
    /// newest `run_state` for the terminal fold. Each record carries only the
    /// lines pushed that cycle (the occupant empties its buffer on publish),
    /// so nothing is coalesced or replayed.
    fn drain_progress(&mut self) {
        let mut details = core::mem::take(&mut self.detail_scratch);
        let mut state = self.last_run_state;
        let _ = self.seq_status.drain(|f| {
            state = f.get().run_state;
            let list = f.list::<ProgressLine>(offset_of!(SequenceStatus, progress));
            for line in list.iter() {
                let n = (line.len as usize).min(PROGRESS_MSG_CAP);
                if let Ok(s) = core::str::from_utf8(&line.msg[..n]) {
                    details.push(s.to_string());
                }
            }
        });
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

    /// Publish the host-side [`SlotStatus`] frame.
    fn publish_status(&mut self, now: Timestamp) {
        let (occupant, occ_len) = match self.selected {
            Some(idx) => pack_name(&self.allowed[idx].name),
            None => ([0u8; SLOT_NAME_CAP], 0),
        };
        let frame = SlotStatus {
            timestamp: now,
            phase: self.state.code(),
            occ_len,
            _pad: [0; 6],
            occupant,
        };
        let _ = self.status_out.write(&frame);
    }
}

impl CyclicSlot for SlotRunner {
    /// Apply the initial occupant, loading it and optionally starting it.
    /// Slots without one init to a no-op.
    fn init(&mut self) {
        if let Some(initial) = self.initial.take() {
            self.do_load(&initial.occupant);
            if initial.start {
                self.do_start();
            }
        }
    }

    /// Drain and apply addressed commands, publish status, then poll the
    /// occupant once while running and fold its raw [`FswStatus`] into the
    /// slot phase. Once the phase leaves running the occupant is not polled.
    fn step(&mut self, now: Timestamp) {
        self.last_now = now;
        // Apply commands before stepping the occupant so a command lands the
        // cycle it arrives. The buffer is taken and returned around the loop
        // because apply_command needs `&mut self`, and retained across steps
        // so the steady state allocates nothing.
        let mut cmds = core::mem::take(&mut self.cmd_scratch);
        self.commands.drain(|c| cmds.push(c));
        for cmd in &cmds {
            self.apply_command(cmd);
        }
        cmds.clear();
        self.cmd_scratch = cmds;
        self.publish_status(now);
        if matches!(self.state, SlotState::Running)
            && let Some(slot) = self.slot.as_mut()
        {
            let st = slot.execute_raw(now);
            // Drain what the occupant just published before folding the
            // terminal event, so observers see the final Progress lines ahead
            // of the Completed/Aborted/Failed derived below.
            self.drain_progress();
            self.state = match st {
                FswStatus::Running => SlotState::Running,
                // The raw status word carries no outcome detail; refine the
                // terminal Done from the run_state latched out of the
                // occupant's status frames.
                FswStatus::Done => {
                    self.emit_terminal_done();
                    SlotState::Done {
                        outcome: self.last_run_state,
                    }
                }
                FswStatus::Panicked => {
                    self.emit_event(SequenceEventKind::Failed {
                        reason: "panicked".to_string(),
                    });
                    SlotState::Stopped {
                        reason: StopReason::Panicked,
                    }
                }
            };
        }
    }

    /// Drop the live occupant (its `Drop` runs `fsw_destroy`) at teardown.
    /// The coordinator frees the ring regions afterward.
    fn shutdown(&mut self) {
        self.slot = None;
    }

    fn name(&self) -> &'static str {
        self.name
    }

    fn state(&self) -> &SlotState {
        &self.state
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
        .writer(NoWake, NoWake)
        .expect("slot ring is bound to exactly one host writer at build");
    Output::new(writer)
}
