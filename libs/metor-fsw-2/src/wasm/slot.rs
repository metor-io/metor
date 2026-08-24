//! A wasm occupant as a slot driver.
//!
//! [`WasmSlot`] is to [`WasmPack`] what `DlSlot` is to `DlPack`: it owns one
//! bound instance and advances it once per cycle. The lifecycle is the same
//! ABI in the same order, so the slot runner drives all three backings — dl,
//! process worker, and wasm — through the same `execute → FswStatus` shape.
//!
//! What differs is the cycle body. A dl occupant shares the coordinator's
//! rings and simply executes; a guest cannot reach them, so each cycle is
//! *pump in, execute, pump out* across [`RingBridge`], with the memory-stability
//! guard first because every bridge handle is a raw pointer into guest memory.
//!
//! Any failure here is terminal for the occupant and invisible to the rest of
//! the target: a trap, a fuel exhaustion, or a moved memory all fold to
//! [`FswStatus::Panicked`], which the runner already maps onto
//! `SlotState::Stopped` exactly as it does a `.so` panic.

use metor_fsw_2_core::Delivery;
use metor_fsw_2_core::abi::{FswRing, FswStatus, ROLE_INPUT, ROLE_OUTPUT};
use metor_proto::types::Timestamp;

use super::{RingBridge, WasmError, WasmPack};

/// One bound wasm occupant: its module, its instance, and the pump joining it
/// to the slot's rings.
pub struct WasmSlot {
    pack: WasmPack,
    /// The guest's instance pointer from `fsw_pack_create`.
    state: i32,
    bridge: Option<RingBridge>,
    /// Structurally corrupt bridge reads waiting for the coordinator's health
    /// scan. A corruption also stops the occupant; this counter preserves the
    /// specific cause instead of reporting only the generic panic fold.
    boundary_corruptions: u64,
    /// Latched once a cycle has failed, so a dead occupant is never re-entered.
    dead: bool,
}

impl WasmSlot {
    /// Load `wasm`, create entry `index` with `params`, give it rings inside
    /// its own memory, and join those to the slot's host rings.
    ///
    /// `host_inputs`/`host_outputs` are the slot's ring templates in occupant
    /// port order, including the mount-appended control input and status
    /// output; the guest gets a matching region for each.
    pub fn bind(
        wasm: &[u8],
        index: u32,
        params: &[u8],
        instance: &str,
        host_inputs: &[FswRing],
        host_outputs: &[FswRing],
        fuel_per_call: u64,
    ) -> Result<Self, WasmError> {
        Self::bind_with_memory_limit(
            wasm,
            index,
            params,
            instance,
            host_inputs,
            host_outputs,
            fuel_per_call,
            super::DEFAULT_MAX_MEMORY_BYTES,
        )
    }

    /// [`bind`](Self::bind) with an explicit guest-memory ceiling.
    #[allow(clippy::too_many_arguments)]
    pub fn bind_with_memory_limit(
        wasm: &[u8],
        index: u32,
        params: &[u8],
        instance: &str,
        host_inputs: &[FswRing],
        host_outputs: &[FswRing],
        fuel_per_call: u64,
        max_memory: usize,
    ) -> Result<Self, WasmError> {
        let pack = WasmPack::open_with_memory_limit(wasm, fuel_per_call, max_memory)?;
        Self::bind_opened(pack, index, params, instance, host_inputs, host_outputs)
    }

    /// Bind a freshly read module by entry name after proving its ABI-relevant
    /// manifest identity still matches the one resolved into the target.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn bind_compatible(
        wasm: &[u8],
        entry_name: &str,
        expected_identity: &[u8],
        params: &[u8],
        instance: &str,
        host_inputs: &[FswRing],
        host_outputs: &[FswRing],
        setup_fuel: u64,
        poll_fuel: u64,
        max_memory: usize,
    ) -> Result<Self, WasmError> {
        let pack = WasmPack::open_with_memory_limit(wasm, setup_fuel, max_memory)?;
        let (index, entry) = pack
            .manifest()
            .systems
            .iter()
            .enumerate()
            .find(|(_, entry)| entry.descriptor.name == entry_name)
            .ok_or_else(|| WasmError::EntryMissing(entry_name.to_string()))?;
        if super::entry_identity(entry) != expected_identity {
            return Err(WasmError::EntryChanged(entry_name.to_string()));
        }
        let mut slot = Self::bind_opened(
            pack,
            index as u32,
            params,
            instance,
            host_inputs,
            host_outputs,
        )?;
        slot.set_fuel_per_call(poll_fuel);
        Ok(slot)
    }

    fn bind_opened(
        mut pack: WasmPack,
        index: u32,
        params: &[u8],
        instance: &str,
        host_inputs: &[FswRing],
        host_outputs: &[FswRing],
    ) -> Result<Self, WasmError> {
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

        // Mount 1 is the slot-occupant mount, which appends the control input
        // and the status output the entry never declares.
        let state = pack.create(index, 1, params)?;

        // One guest region per host region, sized from the host's, so the two
        // sides of a leg always agree on what a record can be.
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

        // Delivery is known only for the entry's declared ports; the mount tail
        // is snapshot (control, status), so pad rather than assume alignment.
        let in_delivery = Self::pad_delivery(in_delivery, hin.len());
        let out_delivery = Self::pad_delivery(out_delivery, hout.len());

        // SAFETY: the guest regions were just formatted and its memory is
        // pinned; the host regions are the coordinator's, which outlive the
        // runner and so this occupant.
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

        Ok(Self {
            pack,
            state,
            bridge: Some(bridge),
            boundary_corruptions: 0,
            dead: false,
        })
    }

    /// Extend a per-declared-port delivery list to cover the mount tail, which
    /// is snapshot on both sides.
    fn pad_delivery(mut d: Vec<Delivery>, len: usize) -> Vec<Delivery> {
        d.resize(len, Delivery::Snapshot);
        d
    }

    /// One cycle: carry the inputs in, advance the guest, carry the outputs
    /// back out.
    ///
    /// Returns the raw [`FswStatus`] the runner folds, with every failure —
    /// trap, fuel exhaustion, moved memory — reported as
    /// [`FswStatus::Panicked`]. That is the same word a `.so` occupant returns
    /// when its panic is caught, so the runner's terminal handling needs no
    /// wasm-specific case.
    pub fn execute_raw(&mut self, now: Timestamp) -> FswStatus {
        if self.dead {
            return FswStatus::Panicked;
        }
        match self.cycle(now) {
            Ok(status) => status,
            Err(e) => {
                if matches!(&e, WasmError::RingRead(_)) {
                    self.boundary_corruptions += 1;
                }
                tracing::warn!(error = %e, "wasm occupant failed; stopping it");
                self.dead = true;
                FswStatus::Panicked
            }
        }
    }

    fn cycle(&mut self, now: Timestamp) -> Result<FswStatus, WasmError> {
        // Before any handle is touched: a grown guest invalidates all of them.
        self.pack.check_memory_stable()?;
        let bridge = self.bridge.as_mut().expect("a live wasm slot has a bridge");
        bridge.pump_in()?;
        let status = self.pack.execute(self.state, now.0 as u64)?;
        // Out even on a terminal status: the last cycle's status frame and log
        // records are what tell an operator how the sequence ended.
        self.pack.check_memory_stable()?;
        let bridge = self.bridge.as_mut().expect("a live wasm slot has a bridge");
        bridge.pump_out()?;
        Ok(status)
    }

    /// Tighten (or loosen) the budget each later cycle runs under.
    ///
    /// Binding costs far more fuel than a cycle does, so a slot binds under a
    /// generous budget and then applies its configured per-poll one — which is
    /// what actually bounds the occupant.
    pub fn set_fuel_per_call(&mut self, fuel: u64) {
        self.pack.set_fuel_per_call(fuel);
    }

    /// Records the bridge could not deliver, for the slot's health.
    pub fn dropped(&self) -> u64 {
        self.bridge.as_ref().map_or(0, RingBridge::dropped)
    }

    /// Drain bridge drops for coordinator health.
    pub fn drain_dropped(&mut self) -> u64 {
        self.bridge.as_mut().map_or(0, RingBridge::drain_dropped)
    }

    /// Drain structurally corrupt bridge reads for coordinator health.
    pub fn drain_corruptions(&mut self) -> u64 {
        std::mem::take(&mut self.boundary_corruptions)
    }
}

impl Drop for WasmSlot {
    fn drop(&mut self) {
        if !self.dead {
            let _ = self.pack.shutdown(self.state);
            let _ = self.pack.destroy(self.state);
        }
        // The bridge's guest-side handles release claims through pointers into
        // linear memory, so it must die while the store still owns that memory.
        drop(self.bridge.take());
        let _ = self.pack.close();
    }
}
