//! Copy records between host-owned and guest-owned rings each cycle.
//!
//! Separate rings keep guest corruption away from native consumers. Handles
//! persist so readers retain their cursors between cycles. Guest memory must
//! stay at a fixed address until all bridge handles are dropped; the caller
//! checks this with [`WasmPack::check_memory_stable`](super::WasmPack::check_memory_stable).

use metor_fsw_ring::{NoWake, RingBuffer};

use metor_fsw_2_core::Delivery;

use crate::io_bridge::{IoBridge, RingPump};

use super::{GuestRing, WasmError};

/// The per-cycle record pump between a slot's host rings and its occupant's
/// guest rings.
pub struct RingBridge {
    io: IoBridge<NoWake, NoWake>,
}

impl RingBridge {
    /// Build the pump over a slot's host regions and its occupant's guest
    /// regions, which must be in matching descriptor order.
    ///
    /// # Safety
    /// `guest_base` is the current base of the guest's linear memory, and every
    /// `GuestRing` names a formatted region inside it. The caller keeps that
    /// memory from moving for the bridge's whole life; see
    /// [`WasmPack::check_memory_stable`](super::WasmPack::check_memory_stable).
    /// Each host region must likewise outlive the bridge.
    pub unsafe fn new(
        guest_base: *mut u8,
        host_inputs: &[(*mut u8, usize)],
        guest_inputs: &[GuestRing],
        input_delivery: &[Delivery],
        host_outputs: &[(*mut u8, usize)],
        guest_outputs: &[GuestRing],
        output_delivery: &[Delivery],
    ) -> Result<Self, WasmError> {
        // SAFETY: the caller asserts every region is live and stays put.
        let attach_guest = |g: &GuestRing| unsafe {
            RingBuffer::attach_raw(guest_base.add(g.offset as usize), g.len)
                .map_err(WasmError::Ring)
        };
        // SAFETY: same, for a coordinator-owned region.
        let attach_host = |&(base, len): &(*mut u8, usize)| unsafe {
            RingBuffer::attach_raw(base, len).map_err(WasmError::Ring)
        };

        let mut inputs = Vec::with_capacity(guest_inputs.len());
        for ((host, guest), &delivery) in host_inputs.iter().zip(guest_inputs).zip(input_delivery) {
            inputs.push(RingPump::new(
                attach_host(host)?
                    .view(NoWake)
                    .map_err(|_| WasmError::NoSlot)?,
                attach_guest(guest)?
                    .writer(NoWake)
                    .map_err(|_| WasmError::WriterClaimed)?,
                delivery,
            ));
        }

        let mut outputs = Vec::with_capacity(guest_outputs.len());
        for ((host, guest), &delivery) in
            host_outputs.iter().zip(guest_outputs).zip(output_delivery)
        {
            outputs.push(RingPump::new(
                attach_guest(guest)?
                    .view(NoWake)
                    .map_err(|_| WasmError::NoSlot)?,
                attach_host(host)?
                    .writer(NoWake)
                    .map_err(|_| WasmError::WriterClaimed)?,
                delivery,
            ));
        }

        Ok(Self {
            io: IoBridge::new(inputs, outputs),
        })
    }

    /// Carry this cycle's inputs into the guest, before its `execute`.
    pub fn pump_in(&mut self) -> Result<(), WasmError> {
        self.io.import().map_err(WasmError::RingRead)
    }

    /// Carry what the guest produced back out, after its `execute`.
    pub fn pump_out(&mut self) -> Result<(), WasmError> {
        self.io.export().map_err(WasmError::RingRead)
    }

    /// Records dropped because a destination ring was full, for the slot's
    /// fault scan.
    pub fn dropped(&self) -> u64 {
        self.io.dropped()
    }

    /// Drain records dropped since the last fault scan.
    pub fn drain_dropped(&mut self) -> u64 {
        self.io.drain_dropped()
    }
}
