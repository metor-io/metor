//! A wasm pack entry as an ordinary wired cyclic system.
//!
//! [`WasmCyclic`] is to [`WasmSlot`](super::WasmSlot) what a plain wired
//! `DlSlot` is to a runtime slot: one bound instance in a fixed position of
//! the cyclic chain, created with `Mount::Wired` so no control/status tail is
//! appended, its rings mirrored and pumped across [`RingBridge`] exactly like
//! an occupant's. This is a general capability — any wasm pack entry mounts
//! this way, Rust-authored ones included; compiled Python packs are its first
//! producer.
//!
//! Faults degrade, never kill (plan D8). A trap, fuel exhaustion, moved
//! memory, or a corrupt guest ring latches the instance dead and surfaces as
//! `SlotState::Stopped`, which the coordinator's status scan folds into its
//! own log (`system_stopped`). A dead instance is never
//! re-entered, but its bridge keeps carrying inputs so upstream producers
//! never backpressure on a reader that stopped moving; what cannot be
//! delivered counts as `wasm_boundary_dropped`.

use std::sync::Arc;

use metor_fsw_2_core::abi::{FswRing, FswStatus, ROLE_INPUT, ROLE_OUTPUT};
use metor_fsw_2_core::{CyclicSlot, SlotState, StopReason};
use metor_proto::types::Timestamp;

use super::{RingBridge, WasmError, WasmPack};

/// One wired wasm instance: its module, its guest state, and the pump joining
/// it to the coordinator's rings.
pub(crate) struct WasmCyclic {
    pack: WasmPack,
    /// The guest's instance pointer from `fsw_pack_create`.
    state: i32,
    bridge: Option<RingBridge>,
    /// Structurally corrupt bridge reads for the coordinator's fault scan.
    boundary_corruptions: u64,
    /// Latched once a cycle has failed; a dead instance is never re-entered.
    dead: bool,
    /// Status identity, type-level like a static system's `System::NAME`.
    name: Arc<str>,
    slot_state: SlotState,
}

impl WasmCyclic {
    /// Open `wasm` under the setup budget, create entry `index` wired, mirror
    /// the coordinator's rings into the guest, bind, and pin — then apply the
    /// per-poll budget that actually bounds it.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn bind(
        wasm: &[u8],
        index: u32,
        params: &[u8],
        entry_name: Arc<str>,
        instance: &str,
        host_inputs: &[FswRing],
        host_outputs: &[FswRing],
        setup_fuel: u64,
        poll_fuel: u64,
        max_memory: usize,
    ) -> Result<Self, WasmError> {
        let mut pack = WasmPack::open_with_memory_limit(wasm, setup_fuel, max_memory)?;
        let entry = pack
            .manifest()
            .systems
            .get(index as usize)
            .ok_or(WasmError::Create(index))?;
        let in_delivery: Vec<_> = entry.descriptor.inputs.iter().map(|p| p.delivery).collect();
        let out_delivery: Vec<_> = entry
            .descriptor
            .outputs
            .iter()
            .map(|p| p.delivery)
            .collect();

        // Mount 0 is the plain wired mount: no control input, no status
        // output, the descriptor's own ports and nothing else.
        let state = pack.create(index, 0, params)?;
        let guest_inputs = super::mirror_rings(&mut pack, host_inputs, ROLE_INPUT)?;
        let guest_outputs = super::mirror_rings(&mut pack, host_outputs, ROLE_OUTPUT)?;
        pack.bind_init(state, &guest_inputs, &guest_outputs, instance)?;

        // Nothing may grow the guest's memory after this point; every bridge
        // handle below is a raw pointer into it.
        pack.pin_memory();
        let base = pack.memory_base();
        let host = |r: &FswRing| (r.base, r.len);
        let hin: Vec<_> = host_inputs.iter().map(host).collect();
        let hout: Vec<_> = host_outputs.iter().map(host).collect();
        // No mount tail, so the descriptor's own delivery lists cover the
        // rings exactly — no padding.
        // SAFETY: the guest regions were just formatted and its memory is
        // pinned; the host regions are the coordinator's, which outlive the
        // cyclic list and so this runner.
        let bridge = unsafe {
            RingBridge::new(
                base,
                &hin,
                &guest_inputs,
                &in_delivery,
                &hout,
                &guest_outputs,
                &out_delivery,
            )
        }?;

        pack.set_fuel_per_call(poll_fuel);
        Ok(Self {
            pack,
            state,
            bridge: Some(bridge),
            boundary_corruptions: 0,
            dead: false,
            name: entry_name,
            slot_state: SlotState::Running,
        })
    }

    /// One cycle: in, execute, out — with the memory-stability guard before
    /// any handle is touched.
    fn cycle(&mut self, now: Timestamp) -> Result<FswStatus, WasmError> {
        self.pack.check_memory_stable()?;
        let bridge = self
            .bridge
            .as_mut()
            .expect("a live wired wasm has a bridge");
        bridge.pump_in()?;
        let status = self.pack.execute(self.state, now.0 as u64)?;
        self.pack.check_memory_stable()?;
        let bridge = self
            .bridge
            .as_mut()
            .expect("a live wired wasm has a bridge");
        bridge.pump_out()?;
        Ok(status)
    }

    /// Latch the instance dead and surface the stop.
    fn stop(&mut self) {
        self.dead = true;
        self.slot_state = SlotState::Stopped {
            reason: StopReason::Panicked,
        };
    }
}

impl CyclicSlot for WasmCyclic {
    /// The guest's bind/init ran when this runner was built (the same point a
    /// slot occupant's does at `Load`); nothing is deferred to the loop.
    fn init(&mut self) {}

    fn step(&mut self, now: Timestamp) {
        if self.dead {
            // Keep carrying inputs so producers never backpressure on a dead
            // reader; drops are counted, and the guest is never re-entered.
            if self.pack.check_memory_stable().is_ok()
                && let Some(bridge) = self.bridge.as_mut()
            {
                let _ = bridge.pump_in();
            }
            return;
        }
        match self.cycle(now) {
            // A well-behaved cyclic entry never returns `Done`; a stray one is
            // keep-running, exactly as the dl path treats it.
            Ok(FswStatus::Running | FswStatus::Done) => {}
            Ok(FswStatus::Panicked) => {
                tracing::warn!(system = %self.name, "wasm system panicked; stopping it");
                self.stop();
            }
            Err(e) => {
                if matches!(&e, WasmError::RingRead(_)) {
                    self.boundary_corruptions += 1;
                }
                tracing::warn!(system = %self.name, error = %e, "wasm system failed; stopping it");
                self.stop();
            }
        }
    }

    fn shutdown(&mut self) {
        if !self.dead {
            let _ = self.pack.shutdown(self.state);
        }
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn state(&self) -> &SlotState {
        &self.slot_state
    }

    fn drain_boundary_drops(&mut self) -> u64 {
        self.bridge.as_mut().map_or(0, RingBridge::drain_dropped)
    }

    fn boundary_drop_kind(&self) -> &'static str {
        "wasm_boundary_dropped"
    }

    fn drain_boundary_corruptions(&mut self) -> u64 {
        std::mem::take(&mut self.boundary_corruptions)
    }
}

impl Drop for WasmCyclic {
    fn drop(&mut self) {
        if !self.dead {
            let _ = self.pack.destroy(self.state);
        }
        // The bridge's guest-side handles release claims through pointers into
        // linear memory, so it must die while the store still owns that memory.
        drop(self.bridge.take());
        let _ = self.pack.close();
    }
}
