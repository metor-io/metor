//! Pumping records between the coordinator's rings and a guest's.
//!
//! A wasm occupant cannot be wired into the graph the way a `.so` one is. A
//! `.so` attaches to the coordinator's own ring regions and reads and writes
//! the same atomics every other system does; a guest cannot, because nothing
//! maps host memory into a wasm linear memory. The privilege runs one way — the
//! host can see all of the guest's memory, never the reverse — so the two sides
//! keep separate rings and the host copies between them once per cycle.
//!
//! Keeping the coordinator's rings **host-owned** is the point, not an
//! accident. Putting them inside the guest instead would remove the copy, but
//! it would also put the data native systems consume inside the sandbox, where
//! a faulting occupant could corrupt the headers and cursors they depend on.
//! That would trade away the property the substrate exists for. Here a guest
//! can corrupt only its own copy, and the host validates at the boundary.
//!
//! The copy is affordable: the Phase 0 spike measured marshalling at 7 ns
//! against a 2,873 ns cycle.
//!
//! ## Why the handles persist
//!
//! Every handle here is built once and held. A [`View`] claims a reader slot
//! and its cursor lives in that slot, so a view re-attached each cycle would
//! rejoin at the live edge and silently drop everything written since the last
//! one. The guest-side handles are raw pointers into the interpreter's backing
//! buffer, which `memory.grow` reallocates — hence
//! [`WasmPack::check_memory_stable`](super::WasmPack::check_memory_stable),
//! which every pump runs before touching them.

use metor_fsw_ring::{NoWake, RingBuffer, View, Writer};

use super::{GuestRing, WasmError};

/// One direction of one port: read from `from`, write to `to`.
struct Leg {
    from: View<NoWake>,
    to: Writer<NoWake>,
}

impl Leg {
    /// Forward every record waiting on `from`.
    ///
    /// Every record, not just the newest: a snapshot consumer collapses to
    /// latest-wins on its own, and a log consumer needs the backlog, so
    /// forwarding faithfully is what keeps both delivery modes intact across
    /// the boundary.
    ///
    /// A record the destination cannot take is dropped and counted rather than
    /// failing the cycle, which matches what the lossless ring already does to
    /// a producer that outruns its consumer.
    fn pump(&mut self) -> u64 {
        let mut dropped = 0;
        while let Ok(Some(grant)) = self.from.try_read() {
            if self.to.try_write(&grant).is_err() {
                dropped += 1;
            }
        }
        dropped
    }
}

/// The per-cycle record pump between a slot's host rings and its occupant's
/// guest rings.
pub struct RingBridge {
    /// Coordinator ring → guest ring, one per input port, in descriptor order.
    inputs: Vec<Leg>,
    /// Guest ring → coordinator ring, one per output port, in descriptor order.
    outputs: Vec<Leg>,
    /// Records dropped because a destination ring was full.
    dropped: u64,
}

impl RingBridge {
    /// Build the pump over a slot's host regions and its occupant's guest
    /// regions, which must be in matching descriptor order.
    ///
    /// # Safety
    /// `guest_base` is the current base of the guest's linear memory, and every
    /// `GuestRing` names a formatted region inside it. The caller keeps that
    /// memory from moving for the bridge's whole life — see
    /// [`WasmPack::check_memory_stable`](super::WasmPack::check_memory_stable).
    /// Each host region must likewise outlive the bridge.
    pub unsafe fn new(
        guest_base: *mut u8,
        host_inputs: &[(*mut u8, usize)],
        guest_inputs: &[GuestRing],
        host_outputs: &[(*mut u8, usize)],
        guest_outputs: &[GuestRing],
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
        for (host, guest) in host_inputs.iter().zip(guest_inputs) {
            inputs.push(Leg {
                from: attach_host(host)?.view(NoWake).map_err(|_| WasmError::NoSlot)?,
                to: attach_guest(guest)?
                    .writer(NoWake)
                    .map_err(|_| WasmError::WriterClaimed)?,
            });
        }

        let mut outputs = Vec::with_capacity(guest_outputs.len());
        for (host, guest) in host_outputs.iter().zip(guest_outputs) {
            outputs.push(Leg {
                from: attach_guest(guest)?
                    .view(NoWake)
                    .map_err(|_| WasmError::NoSlot)?,
                to: attach_host(host)?
                    .writer(NoWake)
                    .map_err(|_| WasmError::WriterClaimed)?,
            });
        }

        Ok(Self {
            inputs,
            outputs,
            dropped: 0,
        })
    }

    /// Carry this cycle's inputs into the guest, before its `execute`.
    pub fn pump_in(&mut self) {
        for leg in &mut self.inputs {
            self.dropped += leg.pump();
        }
    }

    /// Carry what the guest produced back out, after its `execute`.
    pub fn pump_out(&mut self) {
        for leg in &mut self.outputs {
            self.dropped += leg.pump();
        }
    }

    /// Records dropped because a destination ring was full, for the slot's
    /// health.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}
