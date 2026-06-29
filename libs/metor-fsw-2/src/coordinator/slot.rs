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

use metor_proto::types::Timestamp;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use metor_fsw_ring::NoWake;

use super::{CyclicSlot, SlotState, StopReason};
use crate::abi::FswStatus;
use crate::dl::{DlSlot, DlSystem};
use crate::port::Output;
use crate::sequence::SlotControlIn;

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

/// The slot-command kind, mirroring the lifecycle edges (sequences-slots.md §2.1).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SlotCommandKind {
    /// Select an allowed occupant by name, `fsw_create` + `fsw_bind_init` it. Empty→Loaded.
    Load = 0,
    /// Begin polling the occupant. Loaded(live)→Running.
    Start = 1,
    /// Hard-drop the occupant's future (`fsw_destroy`). Running→Loaded (no live future).
    Stop = 2,
    /// Cooperative cancel: write a cancel frame the occupant folds. Stays Running until Done.
    Abort = 3,
    /// Rebuild the occupant from the beginning (`fsw_destroy` + `fsw_create` + `fsw_bind_init`).
    Reset = 4,
    /// Drop the occupant. →Empty.
    Unload = 5,
}

impl SlotCommandKind {
    fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            0 => SlotCommandKind::Load,
            1 => SlotCommandKind::Start,
            2 => SlotCommandKind::Stop,
            3 => SlotCommandKind::Abort,
            4 => SlotCommandKind::Reset,
            5 => SlotCommandKind::Unload,
            _ => return None,
        })
    }
}

/// One runtime slot command, crossing the coordinator's control ring as an ordinary
/// frame (sequences-slots.md §3, Resolved Q1). Addressed by `slot` name; `occupant`
/// names the allowed-set entry on `Load`. Fixed-shape: a params blob on `Load` is out
/// of scope for the v1 frame (the allowed default params are used) — carrying one is
/// future work.
#[derive(crate::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy)]
#[repr(C)]
#[metor_fsw(name = "slot_command")]
pub struct SlotCommand {
    #[metor_fsw(timestamp)]
    pub timestamp: Timestamp,
    /// The [`SlotCommandKind`] code (Load=0/Start=1/Stop=2/Abort=3/Reset=4/Unload=5).
    pub kind: u8,
    /// Used length of `slot`.
    pub slot_len: u8,
    /// Used length of `occupant` (Load only).
    pub occ_len: u8,
    pub _pad: [u8; 5],
    /// The target slot's name, fixed buffer + length.
    pub slot: [u8; SLOT_NAME_CAP],
    /// The occupant name (Load only), fixed buffer + length.
    pub occupant: [u8; SLOT_NAME_CAP],
}

impl SlotCommand {
    fn make(kind: SlotCommandKind, slot: &str, occupant: &str) -> Self {
        let (slot_buf, slot_len) = pack_name(slot);
        let (occ_buf, occ_len) = pack_name(occupant);
        Self {
            timestamp: Timestamp(0),
            kind: kind as u8,
            slot_len,
            occ_len,
            _pad: [0; 5],
            slot: slot_buf,
            occupant: occ_buf,
        }
    }

    /// A `Load` command selecting `occupant` (an allowed-set name) into `slot`.
    pub fn load(slot: &str, occupant: &str) -> Self {
        Self::make(SlotCommandKind::Load, slot, occupant)
    }
    /// A `Start` command for `slot`.
    pub fn start(slot: &str) -> Self {
        Self::make(SlotCommandKind::Start, slot, "")
    }
    /// A hard-drop `Stop` command for `slot`.
    pub fn stop(slot: &str) -> Self {
        Self::make(SlotCommandKind::Stop, slot, "")
    }
    /// A cooperative `Abort` command for `slot`.
    pub fn abort(slot: &str) -> Self {
        Self::make(SlotCommandKind::Abort, slot, "")
    }
    /// A `Reset` (rebuild) command for `slot`.
    pub fn reset(slot: &str) -> Self {
        Self::make(SlotCommandKind::Reset, slot, "")
    }
    /// An `Unload` command for `slot`.
    pub fn unload(slot: &str) -> Self {
        Self::make(SlotCommandKind::Unload, slot, "")
    }

    /// The target slot name.
    pub fn slot_name(&self) -> &str {
        read_name(&self.slot, self.slot_len)
    }
    /// The occupant name (meaningful on `Load`).
    pub fn occupant_name(&self) -> &str {
        read_name(&self.occupant, self.occ_len)
    }
    fn kind(&self) -> Option<SlotCommandKind> {
        SlotCommandKind::from_code(self.kind)
    }
}

/// Pack a name into a fixed `SLOT_NAME_CAP` buffer + used length (truncating).
fn pack_name(name: &str) -> ([u8; SLOT_NAME_CAP], u8) {
    let bytes = name.as_bytes();
    let n = bytes.len().min(SLOT_NAME_CAP);
    let mut buf = [0u8; SLOT_NAME_CAP];
    buf[..n].copy_from_slice(&bytes[..n]);
    (buf, n as u8)
}

/// Read a fixed-buffer name back as a `&str` (lossy on non-UTF8 boundaries is avoided —
/// the writer only ever packs valid UTF-8 prefixes, but a truncation could split a
/// multibyte char; we fall back to the longest valid prefix).
fn read_name(buf: &[u8; SLOT_NAME_CAP], len: u8) -> &str {
    let n = (len as usize).min(SLOT_NAME_CAP);
    match core::str::from_utf8(&buf[..n]) {
        Ok(s) => s,
        Err(e) => core::str::from_utf8(&buf[..e.valid_up_to()]).unwrap_or(""),
    }
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
    ) -> Self {
        Self {
            name,
            allowed,
            initial,
            input_regions,
            output_regions,
            control,
            status_out,
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
        self.sync_slot_state();
    }

    /// `Start`: begin polling. Only from a Loaded slot with a live future.
    fn do_start(&mut self) {
        if self.slot.is_some() && matches!(self.phase, SlotPhase::Loaded) {
            self.phase = SlotPhase::Running;
            self.sync_slot_state();
        }
    }

    /// `Stop` (hard-drop): drop the occupant's future (its `Drop` runs `fsw_destroy`,
    /// releasing the ring roles), leaving Loaded with no live future. Re-run via `Reset`.
    fn do_stop(&mut self) {
        if matches!(self.phase, SlotPhase::Running) {
            self.slot = None;
            self.phase = SlotPhase::Loaded;
            self.sync_slot_state();
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
        self.sync_slot_state();
    }

    /// `Unload`: drop the occupant and return to Empty.
    fn do_unload(&mut self) {
        self.slot = None;
        self.selected = None;
        self.phase = SlotPhase::Empty;
        self.sync_slot_state();
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
        self.publish_status(now);
        if matches!(self.phase, SlotPhase::Running)
            && let Some(slot) = self.slot.as_mut()
        {
            let st = slot.execute_raw(now);
            self.phase = match st {
                FswStatus::Running => SlotPhase::Running,
                // The FswStatus word carries no outcome detail — the
                // Completed/Aborted/Failed detail rides the occupant's SequenceStatus
                // frame; the slot phase just becomes terminal Done.
                FswStatus::Done => SlotPhase::Done { outcome: 0 },
                FswStatus::StoppedLapped => SlotPhase::Stopped {
                    reason: StopReason::LappedInput,
                },
                FswStatus::Panicked => SlotPhase::Stopped {
                    reason: StopReason::Panicked,
                },
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

    /// Dispatch a runtime command addressed to this slot (the default no-op on the trait
    /// makes every non-slot `CyclicSlot` ignore it; the coordinator broadcasts each
    /// drained command to every slot). Filters by name to stay total even though the
    /// dispatcher already targets by name.
    fn command(&mut self, cmd: &SlotCommand) {
        if cmd.slot_name() != self.name {
            return;
        }
        match cmd.kind() {
            Some(SlotCommandKind::Load) => self.do_load(cmd.occupant_name()),
            Some(SlotCommandKind::Start) => self.do_start(),
            Some(SlotCommandKind::Stop) => self.do_stop(),
            Some(SlotCommandKind::Abort) => self.do_abort(),
            Some(SlotCommandKind::Reset) => self.do_reset(),
            Some(SlotCommandKind::Unload) => self.do_unload(),
            None => {} // unknown kind code: ignore
        }
    }
}

/// A fresh host writer over a control/command/status ring (mirrors how the coordinator
/// wraps its own ports). Cyclic ⇒ [`NoWake`].
pub(crate) fn slot_writer<F: crate::Frame + IntoBytes + Immutable>(
    ring: &metor_fsw_ring::RingBuffer<metor_fsw_ring::BoxBacking>,
) -> Output<F> {
    Output::new(ring.writer(NoWake, NoWake))
}
