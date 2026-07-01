//! Work-Package 10 (Wave 4) — the runtime-loadable slot (sequences-slots.md §2/§3/§6).
//!
//! A **slot** is a third [`CyclicSlot`] impl ([`SlotRunner`]) the coordinator drives
//! every cycle like a static [`CyclicRunner`](crate::CyclicRunner) or a build-time
//! [`DlSlot`](crate::dl) — but its occupant is **dynamic**: the host `Load`s, `Start`s,
//! `Stop`s, `Abort`s, `Reset`s, and `Unload`s it at runtime over the existing `fsw_*`
//! ABI, with no new lifecycle symbols (sequences-slots.md §2.1, §8). v1 holds
//! **sequence** occupants only: the allowed set is pre-opened/validated at `build()`
//! and `Load` selects one by name.
//!
//! The load-bearing reuse is the ring topology. The coordinator owns every ring in its
//! `RingTable` for the whole mission; an occupant only **borrows** transient
//! `Writer`/`View` handles over them (sequences-slots.md §2.3). On `Stop`/`Unload`/`Reset`
//! the occupant's `fsw_destroy` drops those `RawBacking` ports, releasing the ring roles
//! back to the host-owned ring, and a later `Load` re-acquires over the same regions
//! (Wave 1). So `make_slot` runs N times against one [`DlSystem`](crate::dl), each a
//! fresh `fsw_create` state over the slot's pre-allocated rings.
//!
//! The slot's contract is the **registered descriptor** the generic `build()` pass sees
//! (`src/coordinator/mod.rs`): the occupant descriptor with the trailing
//! [`SlotControlIn`] input removed. Every registered input is therefore an ordinary
//! edge-connected user input, and every output a normally-allocated, registry-tapped
//! occupant output. The slot additionally owns one control ring (the host writes the
//! cancel frame the occupant folds, §4.4) appended to the occupant's input array, and
//! one [`SlotStatus`] output ring it telemeters its host-side phase through (§7).

use core::mem::offset_of;

use metor_proto::types::Timestamp;
use metor_proto_wkt::{
    ChannelId, SequenceChannelEvent, SequenceCommand, SequenceCommandKind, SequenceEventKind,
};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use metor_fsw_ring::NoWake;

use super::{CyclicSlot, SlotState, StopReason};
use crate::abi::FswStatus;
use crate::dl::{DlSlot, DlSystem};
use crate::message::{MsgIn, MsgOut};
use crate::port::{Input, Output};
use crate::sequence::{PROGRESS_MSG_CAP, ProgressLine, SequenceStatus, SlotControlIn};

/// Capacity of one occupant/slot name in the host frames below (longer truncated).
pub const SLOT_NAME_CAP: usize = 48;

// ---------------------------------------------------------------------------
// SlotPhase — the slot-layer lifecycle (sequences-slots.md §2, Resolved Q6)
// ---------------------------------------------------------------------------

/// The slot's runtime lifecycle — the richer machine §2 introduces for the slot layer,
/// distinct from (and not overloading) the build-time 2-variant
/// [`SlotState`](crate::SlotState) that static/dl slots use.
///
/// The live occupant is tracked **separately** as [`SlotRunner::slot`]
/// (`Option<DlSlot>`): `slot.is_some()` means "has a live future." After a hard-drop
/// `Stop` the phase returns to `Loaded` but `slot` is `None`, so `Start` is rejected and
/// only `Reset` (a rebuild) can re-run it (sequences-slots.md §2 note).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SlotPhase {
    /// No occupant; `step` is a cheap no-op.
    Empty,
    /// An occupant is `fsw_create`d + `fsw_bind_init`'d (its future built) but not yet
    /// polling. After a hard-drop `Stop` the phase is `Loaded` with no live future.
    Loaded,
    /// The occupant is polled (`fsw_execute`) every cycle.
    Running,
    /// The occupant's future returned `Ready` — terminal. The
    /// `Completed`/`Aborted`/`Failed` detail rides the occupant's own
    /// [`SequenceStatus`](crate::sequence::SequenceStatus) frame, not the `outcome`
    /// byte here (the ABI status word carries no detail, §2.2).
    Done { outcome: u8 },
    /// The occupant lapped an input or panicked inside the `.so` — terminal (§2.2).
    Stopped { reason: StopReason },
}

impl SlotPhase {
    /// The wire phase code published in [`SlotStatus::phase`].
    pub fn code(self) -> u8 {
        match self {
            SlotPhase::Empty => 0,
            SlotPhase::Loaded => 1,
            SlotPhase::Running => 2,
            SlotPhase::Done { .. } => 3,
            SlotPhase::Stopped { .. } => 4,
        }
    }
}

// ---------------------------------------------------------------------------
// Host telemetry / control frames
// ---------------------------------------------------------------------------

/// Host-side slot telemetry (sequences-slots.md §7): the [`SlotRunner`]'s view of
/// "what is loaded and is it running." Written each cycle and tapped by telemetry `All`
/// like any output. The occupant-side detail (progress, terminal outcome) rides the
/// separate [`SequenceStatus`](crate::sequence::SequenceStatus) frame.
#[derive(crate::Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
#[metor_fsw(name = "slot_status")]
pub struct SlotStatus {
    #[metor_fsw(timestamp)]
    pub timestamp: Timestamp,
    /// The current [`SlotPhase::code`] (Empty=0/Loaded=1/Running=2/Done=3/Stopped=4).
    pub phase: u8,
    /// Used length of `occupant` (0 when no occupant is selected).
    pub occ_len: u8,
    pub _pad: [u8; 6],
    /// The selected occupant's name (the allowed-set entry), fixed buffer + length.
    pub occupant: [u8; SLOT_NAME_CAP],
}

/// Pack a name into a fixed `SLOT_NAME_CAP` buffer + used length (truncating) — used by
/// the host-side [`SlotStatus`] occupant field.
fn pack_name(name: &str) -> ([u8; SLOT_NAME_CAP], u8) {
    let bytes = name.as_bytes();
    let n = bytes.len().min(SLOT_NAME_CAP);
    let mut buf = [0u8; SLOT_NAME_CAP];
    buf[..n].copy_from_slice(&bytes[..n]);
    (buf, n as u8)
}

// ---------------------------------------------------------------------------
// Allowed / initial occupants + the registration payload
// ---------------------------------------------------------------------------

/// One pre-opened allowed occupant: its `Load` name, the loaded [`DlSystem`] (whose
/// `Arc<Library>` stays loaded across swaps), and the canonical postcard params blob
/// `fsw_create` decodes (the allowed default, used until a per-command params blob lands
/// — future work).
pub struct AllowedOccupant {
    pub name: String,
    pub system: DlSystem,
    pub params: Vec<u8>,
}

/// An optional occupant applied at [`SlotRunner::init`] (build-time startup): `Load` it,
/// and `Start` it too if `start` is set.
#[derive(Clone, Debug)]
pub struct InitialOccupant {
    /// The allowed-set occupant name to Load at startup.
    pub occupant: String,
    /// Whether to also Start it (Running from the first cycle).
    pub start: bool,
}

impl InitialOccupant {
    /// A `Loaded` initial occupant (not started).
    pub fn loaded(occupant: impl Into<String>) -> Self {
        Self {
            occupant: occupant.into(),
            start: false,
        }
    }
    /// A `Running` initial occupant (Load + Start at startup).
    pub fn running(occupant: impl Into<String>) -> Self {
        Self {
            occupant: occupant.into(),
            start: true,
        }
    }
}

/// The slot registration the builder records and `build()` turns into a [`SlotRunner`]
/// (the slot twin of [`DlReg`](super::Reg)).
pub(crate) struct SlotReg {
    pub allowed: Vec<AllowedOccupant>,
    pub initial: Option<InitialOccupant>,
}

// ---------------------------------------------------------------------------
// SlotRunner — the third CyclicSlot impl
// ---------------------------------------------------------------------------

/// The runtime-swappable slot the coordinator drives as a `Box<dyn CyclicSlot>`. It
/// holds the slot's pre-built per-port [`FswRing`](crate::abi::FswRing) templates (the
/// host owns the rings for the whole mission), the allowed-occupant set, the live
/// occupant (`Option<DlSlot>`), the [`SlotPhase`], and the host-owned control/status
/// writers. The occupant is **not** created at `build()` — only `Load` (init or a
/// runtime command) creates it.
pub(crate) struct SlotRunner {
    /// The slot's instance name (its `CyclicSlot` identity and the command address).
    name: &'static str,
    allowed: Vec<AllowedOccupant>,
    /// Applied once at [`init`](CyclicSlot::init); `None` after.
    initial: Option<InitialOccupant>,
    /// The occupant input `FswRing` template, in occupant input order: producer views
    /// for the user inputs, then the control ring's region (role INPUT). Cloned per
    /// `make_slot` (the regions are stable; the `FswRing`s are `Copy`).
    input_regions: Vec<crate::abi::FswRing>,
    /// The occupant output `FswRing` template (role OUTPUT), in occupant output order.
    output_regions: Vec<crate::abi::FswRing>,
    /// Host writer over the control ring — `Abort` writes a cancel frame here.
    control: Output<SlotControlIn>,
    /// Host writer over the SlotStatus ring — written each `step`.
    status_out: Output<SlotStatus>,
    /// The slot's **message** channel (`docs/messages.md` §5): the sole writer of the
    /// per-slot events ring, emitting a [`SequenceChannelEvent`] at every lifecycle
    /// transition and a `Progress` line per occupant progress record. Single-writer
    /// discipline — distinct from the coordinator's boot-`SequenceRegistry` ring.
    events: MsgOut<SequenceChannelEvent>,
    /// A read view on the slot's **own** occupant [`SequenceStatus`] output ring,
    /// drained every cycle while Running to source `Progress` lines and the terminal
    /// outcome the host-side `FswStatus` word cannot carry (`docs/messages.md` §5.1).
    seq_status: Input<SequenceStatus>,
    /// This slot's [`ChannelId`] — its build-order index among slots (Q2), the id the
    /// boot `SequenceRegistry` and every emitted [`SequenceChannelEvent`] address.
    channel_id: ChannelId,
    /// This slot's command input (`docs/message-wiring.md` §6): a fan-in
    /// [`MsgIn<SequenceCommand>`](MsgIn) over **every** command producer (the coordinator's
    /// in-proc `control_handle` channel + every system's `SequenceCommand` output). Drained at
    /// the head of each [`step`](CyclicSlot::step) and filtered by `channel_id`, so a command
    /// addressed to this slot dispatches the cycle it arrives — the per-slot replacement for the
    /// coordinator's old broadcast `drain_command_bus`.
    commands: MsgIn<SequenceCommand>,
    /// The latest occupant `run_state` drained while Running, used to refine the terminal
    /// `Done` into `Completed`/`Aborted`/`Failed` (the ABI status word carries no detail).
    last_run_state: u8,
    phase: SlotPhase,
    /// The live occupant (a fresh `fsw_create` future), or `None` (Empty / post-Stop).
    slot: Option<DlSlot>,
    /// The selected allowed index (the live/last occupant), for `Reset` + status naming.
    selected: Option<usize>,
    /// The last `now` seen in `step`, used to stamp an `Abort` cancel frame (which does
    /// not need an accurate stamp — `command` has no `now` of its own).
    last_now: Timestamp,
    /// The 2-variant [`SlotState`] the coordinator's stopped-systems accessor reads,
    /// derived from `phase` (Stopped→Stopped, everything else incl. Done→Running).
    slot_state: SlotState,
}

impl SlotRunner {
    /// Assemble a slot from its registered name, allowed set, optional initial occupant,
    /// the per-port ring templates, and the host control/status writers (called by
    /// `build()`; the occupant is created later, at `init`/`Load`).
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
        channel_id: ChannelId,
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
            channel_id,
            commands,
            last_run_state: 0,
            phase: SlotPhase::Empty,
            slot: None,
            selected: None,
            last_now: Timestamp(0),
            slot_state: SlotState::Running,
        }
    }

    /// Refresh the 2-variant `slot_state` accessor from the live phase. A completed slot
    /// (`Done`) is **not** an error-stop, so it maps to `Running` for the coordinator's
    /// stopped-systems telemetry; only `Stopped` (lapped/panicked) surfaces there.
    fn sync_slot_state(&mut self) {
        self.slot_state = match self.phase {
            SlotPhase::Stopped { reason } => SlotState::Stopped { reason },
            _ => SlotState::Running,
        };
    }

    /// `make_slot` the selected occupant over the slot's ring templates and `init` it
    /// (`fsw_bind_init` builds the future). The old occupant, if any, is dropped first
    /// by the caller.
    fn build_occupant(&mut self, idx: usize) {
        let occ = &self.allowed[idx];
        // SAFETY: every region in the templates is a `RingTable`-owned ring that
        // outlives this slot — the coordinator drops `cyclic` (this runner, whose Drop
        // chain destroys the live occupant) before `rings`; and the occupant's own
        // `fsw_destroy` releases its `RawBacking` ports before any re-`Load` re-attaches.
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

    /// Emit one [`SequenceChannelEvent`] on this slot's message channel, tagged with the
    /// slot's [`ChannelId`] (`docs/messages.md` §5). Best-effort: a full overwrite ring
    /// drops the oldest record, surfaced (not silently reordered) by the downlink.
    fn emit_event(&mut self, kind: SequenceEventKind) {
        let _ = self.events.emit(&SequenceChannelEvent {
            channel_id: self.channel_id,
            kind,
        });
    }

    /// Apply a runtime [`SequenceCommand`] addressed to this slot (`docs/message-wiring.md` §6).
    /// Filters by **`channel_id`** — the slot's own build-order id, the same id the boot
    /// `SequenceRegistry` and the panel address — since every slot's command fan-in sees every
    /// command producer. The kind maps straight onto the lifecycle handlers, no intermediate type.
    fn apply_command(&mut self, cmd: &SequenceCommand) {
        if cmd.channel_id != self.channel_id {
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

    /// `Load`: select an allowed occupant by name and build it. Allowed only from Empty
    /// or a terminal phase (Done/Stopped); from a live/post-Stop Loaded it is ignored.
    fn do_load(&mut self, occupant: &str) {
        if !matches!(
            self.phase,
            SlotPhase::Empty | SlotPhase::Done { .. } | SlotPhase::Stopped { .. }
        ) {
            return; // ignore: a live or post-Stop Loaded slot is not re-Loadable
        }
        let Some(idx) = self.allowed.iter().position(|a| a.name == occupant) else {
            return; // unknown occupant name: ignore
        };
        // Drop any terminal occupant's state before reusing the rings.
        self.slot = None;
        self.build_occupant(idx);
        self.selected = Some(idx);
        self.phase = SlotPhase::Loaded;
        self.last_run_state = 0;
        self.sync_slot_state();
        // Emit at the transition (every-record, never diffed from the latest-wins
        // SlotStatus): two commands can apply in one drain, so a snapshot would lose this.
        let name = self.allowed[idx].name.clone();
        self.emit_event(SequenceEventKind::Loaded { name });
    }

    /// `Start`: begin polling. Only from a Loaded slot with a live future.
    fn do_start(&mut self) {
        if self.slot.is_some() && matches!(self.phase, SlotPhase::Loaded) {
            self.phase = SlotPhase::Running;
            self.sync_slot_state();
            self.emit_event(SequenceEventKind::Started);
        }
    }

    /// `Stop` (hard-drop): drop the occupant's future (its `Drop` runs `fsw_destroy`,
    /// releasing the ring roles), leaving Loaded with no live future. Re-run via `Reset`.
    fn do_stop(&mut self) {
        if matches!(self.phase, SlotPhase::Running) {
            self.slot = None;
            self.phase = SlotPhase::Loaded;
            self.sync_slot_state();
            self.emit_event(SequenceEventKind::Stopped);
        }
    }

    /// `Abort`: write a cancel frame the occupant folds at its next `fsw_execute`. The
    /// phase stays Running until the future returns Done (cooperative, §4.4).
    fn do_abort(&mut self) {
        if matches!(self.phase, SlotPhase::Running) {
            let _ = self.control.write(&SlotControlIn {
                timestamp: self.last_now,
                cancel: 1,
                _pad: [0; 7],
            });
        }
    }

    /// `Reset`: rebuild the selected occupant from the beginning. From Done/Stopped or a
    /// post-Stop Loaded slot.
    fn do_reset(&mut self) {
        let Some(idx) = self.selected else {
            return;
        };
        if matches!(self.phase, SlotPhase::Running) {
            return; // a live, running occupant is not Reset (Stop first)
        }
        self.slot = None;
        self.build_occupant(idx);
        self.phase = SlotPhase::Loaded;
        self.last_run_state = 0;
        self.sync_slot_state();
        // Reset re-arms to idle — the panel sees the channel back at `Loaded { name }`.
        let name = self.allowed[idx].name.clone();
        self.emit_event(SequenceEventKind::Loaded { name });
    }

    /// Drain every occupant [`SequenceStatus`] record published since the last cycle:
    /// emit a `Progress { detail }` per new progress line (each record carries only the
    /// lines pushed that cycle — the occupant's `drain_progress` empties its buffer on
    /// publish) and latch the latest `run_state` for the terminal fold. Every-record,
    /// never coalesced (`docs/messages.md` §5.1). A lapped view resyncs to the live edge.
    fn drain_progress(&mut self) {
        let mut details: Vec<String> = Vec::new();
        let mut state = self.last_run_state;
        let res = self.seq_status.drain(|f| {
            state = f.get().run_state;
            let list = f.list::<ProgressLine>(offset_of!(SequenceStatus, progress));
            for line in list.iter() {
                let n = (line.len as usize).min(PROGRESS_MSG_CAP);
                if let Ok(s) = core::str::from_utf8(&line.msg[..n]) {
                    details.push(s.to_string());
                }
            }
        });
        if res.is_err() {
            self.seq_status.resync();
        }
        self.last_run_state = state;
        for detail in details {
            self.emit_event(SequenceEventKind::Progress { detail });
        }
    }

    /// Emit the terminal [`SequenceChannelEvent`] for a `Done` fold, mapping the latched
    /// occupant `run_state` (`docs/messages.md` §5.2): `1`→`Completed`, `2`→`Aborted`,
    /// else `Failed { reason: "failed" }`. `SequenceStatus` carries no reason string, so
    /// the generic reason is documented future work (§7).
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

    /// Publish the host-side [`SlotStatus`] (phase + occupant name).
    fn publish_status(&mut self, now: Timestamp) {
        let (occupant, occ_len) = match self.selected {
            Some(idx) => pack_name(&self.allowed[idx].name),
            None => ([0u8; SLOT_NAME_CAP], 0),
        };
        let frame = SlotStatus {
            timestamp: now,
            phase: self.phase.code(),
            occ_len,
            _pad: [0; 6],
            occupant,
        };
        let _ = self.status_out.write(&frame);
    }
}

impl CyclicSlot for SlotRunner {
    /// Apply the `initial` occupant (Load + optional Start). Empty slots init to a no-op.
    fn init(&mut self) {
        if let Some(initial) = self.initial.take() {
            self.do_load(&initial.occupant);
            if initial.start {
                self.do_start();
            }
        }
    }

    /// Publish status, then (if Running with a live occupant) poll the occupant once and
    /// fold the raw [`FswStatus`] into the slot phase. On any terminal status the slot
    /// stops calling `execute_raw` (the phase leaves Running).
    fn step(&mut self, now: Timestamp) {
        self.last_now = now;
        // Drain this slot's command fan-in (every command producer) and apply each addressed
        // command *before* stepping the occupant, so a command lands the cycle it arrives — the
        // per-slot replacement for the coordinator's old head-of-cycle broadcast.
        let mut cmds: Vec<SequenceCommand> = Vec::new();
        self.commands.drain(|c| cmds.push(c));
        for cmd in &cmds {
            self.apply_command(cmd);
        }
        self.publish_status(now);
        if matches!(self.phase, SlotPhase::Running)
            && let Some(slot) = self.slot.as_mut()
        {
            let st = slot.execute_raw(now);
            // Drain the SequenceStatus the occupant just published (this poll's Progress
            // lines + run_state) *before* folding the terminal event, so the panel sees
            // Progress* ahead of the Completed/Aborted/Failed it derives below.
            self.drain_progress();
            self.phase = match st {
                FswStatus::Running => SlotPhase::Running,
                // The FswStatus word carries no outcome detail — the
                // Completed/Aborted/Failed detail rides the occupant's SequenceStatus
                // frame (latched in `last_run_state`); refine the terminal Done from it.
                FswStatus::Done => {
                    self.emit_terminal_done();
                    SlotPhase::Done {
                        outcome: self.last_run_state,
                    }
                }
                FswStatus::StoppedLapped => {
                    self.emit_event(SequenceEventKind::Failed {
                        reason: "lapped".to_string(),
                    });
                    SlotPhase::Stopped {
                        reason: StopReason::LappedInput,
                    }
                }
                FswStatus::Panicked => {
                    self.emit_event(SequenceEventKind::Failed {
                        reason: "panicked".to_string(),
                    });
                    SlotPhase::Stopped {
                        reason: StopReason::Panicked,
                    }
                }
            };
            self.sync_slot_state();
        }
    }

    /// Drop the live occupant (its `Drop` runs `fsw_destroy`) at teardown. The
    /// `RingTable` frees the regions afterward (coordinator field order).
    fn shutdown(&mut self) {
        self.slot = None;
    }

    fn name(&self) -> &'static str {
        self.name
    }

    fn state(&self) -> &SlotState {
        &self.slot_state
    }
}

/// A fresh host writer over a control/command/status ring (mirrors how the coordinator
/// wraps its own ports). Cyclic ⇒ [`NoWake`].
pub(crate) fn slot_writer<F: crate::Frame + IntoBytes + Immutable>(
    ring: &metor_fsw_ring::RingBuffer<metor_fsw_ring::BoxBacking>,
) -> Output<F> {
    Output::new(ring.writer(NoWake, NoWake))
}
