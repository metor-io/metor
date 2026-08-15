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

use metor_fsw_2_core::Delivery;

use super::{GuestRing, WasmError};

/// One direction of one port: read from `from`, write to `to`.
///
/// The forwarding policy follows the port's delivery mode, which is the same
/// split the coordinator already makes for async systems: `CopyIn`
/// (`coordinator::CopyIn`) mirrors only the newest record and exists *only*
/// for snapshot inputs, because a log consumer there attaches to the
/// producer's ring directly and takes the whole backlog. A guest can attach to
/// nothing, so both modes cross here — each with the policy that mode wants.
struct Leg {
    from: View<NoWake>,
    to: Writer<NoWake>,
    delivery: Delivery,
    /// `from`'s `committed` at the last forward, for snapshot legs.
    ///
    /// `try_latest` re-serves the pinned newest record when nothing new has
    /// arrived, so without this a snapshot leg would re-forward the same
    /// record every cycle. `u64::MAX` means nothing forwarded yet — the same
    /// sentinel and the same reason as `CopyIn::last_committed`.
    last_committed: u64,
}

impl Leg {
    fn new(from: View<NoWake>, to: Writer<NoWake>, delivery: Delivery) -> Self {
        Self {
            from,
            to,
            delivery,
            last_committed: u64::MAX,
        }
    }

    /// Forward this cycle's records, by the port's delivery mode.
    ///
    /// A log leg drains: every record matters and the consumer needs the
    /// backlog. A snapshot leg forwards only the newest, and only when the
    /// upstream has actually committed something — mirroring a backlog into a
    /// latest-wins port would just relocate it.
    ///
    /// A record the destination cannot take is dropped and counted rather than
    /// failing the cycle, which matches what the ring already does to a
    /// producer that outruns its consumer.
    fn pump(&mut self) -> u64 {
        match self.delivery {
            Delivery::Log => {
                let mut dropped = 0;
                while let Ok(Some(grant)) = self.from.try_read() {
                    if self.to.try_write(&grant).is_err() {
                        dropped += 1;
                    }
                }
                dropped
            }
            Delivery::Snapshot => {
                let committed = self.from.committed();
                if committed == self.last_committed {
                    return 0;
                }
                self.last_committed = committed;
                match self.from.try_latest() {
                    Ok(Some(grant)) => u64::from(self.to.try_write(&grant).is_err()),
                    // A corrupt read is "nothing new", as in `run_copy_ins`.
                    _ => 0,
                }
            }
        }
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
            inputs.push(Leg::new(
                attach_host(host)?.view(NoWake).map_err(|_| WasmError::NoSlot)?,
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
            outputs.push(Leg::new(
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
