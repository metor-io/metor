//! A wasm pack entry as an ordinary wired cyclic system.
//!
//! [`WasmCyclic`] is to [`WasmSlot`] what a plain wired `DlSlot` is to a
//! runtime slot: one bound instance in a fixed position of the cyclic chain,
//! created with `Mount::Wired` so no control/status tail is appended. Any
//! wasm pack entry mounts this way, Rust-authored ones included; compiled
//! Python packs are its first producer.
//!
//! Faults degrade, never kill. A trap, fuel exhaustion, moved memory, or a
//! corrupt guest ring latches the instance dead and surfaces as
//! `SlotState::Stopped`, which the coordinator's status scan folds into its
//! own log. A dead instance is never re-entered, but its bridge keeps
//! carrying inputs so upstream producers never backpressure on it.

use std::sync::Arc;

use metor_fsw_2_core::abi::{FswRing, FswStatus};
use metor_fsw_2_core::{CyclicSlot, Mount, SlotState, StopReason};
use metor_proto::types::Timestamp;

use super::{WasmError, WasmPack, WasmSlot};

/// One wired wasm instance behind the `CyclicSlot` interface.
pub(crate) struct WasmCyclic {
    slot: WasmSlot,
    /// Status identity, type-level like a static system's `System::NAME`.
    name: Arc<str>,
    slot_state: SlotState,
}

impl WasmCyclic {
    /// Open `wasm` under the setup budget, bind entry `index` wired over the
    /// coordinator's rings, then apply the per-poll budget that actually
    /// bounds it.
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
        let pack = WasmPack::open_with_memory_limit(wasm, setup_fuel, max_memory)?;
        let mut slot = WasmSlot::bind_opened(
            pack,
            index,
            params,
            instance,
            host_inputs,
            host_outputs,
            Mount::Wired,
        )?;
        slot.set_fuel_per_call(poll_fuel);
        Ok(Self {
            slot,
            name: entry_name,
            slot_state: SlotState::Running,
        })
    }
}

impl CyclicSlot for WasmCyclic {
    /// The guest's bind/init ran when this runner was built; nothing is
    /// deferred to the loop.
    fn init(&mut self) {}

    fn step(&mut self, now: Timestamp) {
        if self.slot_state.is_stopped() {
            self.slot.carry_inputs();
            return;
        }
        match self.slot.execute_raw(now) {
            // A well-behaved cyclic entry never returns `Done`; a stray one is
            // keep-running, exactly as the dl path treats it.
            FswStatus::Running | FswStatus::Done => {}
            FswStatus::Panicked => {
                tracing::warn!(system = %self.name, "wasm system stopped");
                self.slot_state = SlotState::Stopped {
                    reason: StopReason::Panicked,
                };
            }
        }
    }

    /// The guest's shutdown and destroy run when the slot drops, after the
    /// coordinator's shutdown pass.
    fn shutdown(&mut self) {}

    fn name(&self) -> &str {
        &self.name
    }

    fn state(&self) -> &SlotState {
        &self.slot_state
    }

    fn drain_boundary_drops(&mut self) -> u64 {
        self.slot.drain_dropped()
    }

    fn boundary_drop_kind(&self) -> &'static str {
        "wasm_boundary_dropped"
    }

    fn drain_boundary_corruptions(&mut self) -> u64 {
        self.slot.drain_corruptions()
    }
}
