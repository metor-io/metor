//! Delivery-aware record forwarding across an isolated system boundary.
//!
//! The owner decides *when* imports and exports run. A wasm occupant brackets
//! its synchronous execute call with them; an async boundary runs both at the
//! system's position in the cycle while the task itself runs between cycles.

use metor_fsw_2_core::Delivery;
use metor_fsw_ring::{NoWake, View, WakeSource, Writer};

/// One source ring forwarded into one destination ring according to the
/// source port's delivery contract.
pub(crate) struct RingPump<W: WakeSource> {
    from: View<NoWake>,
    to: Writer<W>,
    delivery: Delivery,
    /// Source `committed` observed at the last snapshot forward.
    last_committed: u64,
}

impl<W: WakeSource> RingPump<W> {
    pub(crate) fn new(from: View<NoWake>, to: Writer<W>, delivery: Delivery) -> Self {
        Self {
            from,
            to,
            delivery,
            last_committed: u64::MAX,
        }
    }

    /// Forward what this delivery mode owes and return destination drops.
    pub(crate) fn pump(&mut self) -> u64 {
        match self.delivery {
            Delivery::Log => {
                let mut dropped = 0;
                while let Ok(Some(grant)) = self.from.try_read() {
                    dropped += u64::from(self.to.try_write(&grant).is_err());
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
                    // Boundary corruption is treated as no transferable data.
                    _ => 0,
                }
            }
        }
    }
}

/// Bidirectional I/O for one isolated cycle participant.
pub(crate) struct IoBridge<I: WakeSource, O: WakeSource> {
    imports: Vec<RingPump<I>>,
    exports: Vec<RingPump<O>>,
    dropped: u64,
}

impl<I: WakeSource, O: WakeSource> IoBridge<I, O> {
    pub(crate) fn new(imports: Vec<RingPump<I>>, exports: Vec<RingPump<O>>) -> Self {
        Self {
            imports,
            exports,
            dropped: 0,
        }
    }

    pub(crate) fn import(&mut self) {
        let dropped: u64 = self.imports.iter_mut().map(RingPump::pump).sum();
        self.dropped += dropped;
    }

    pub(crate) fn export(&mut self) {
        let dropped: u64 = self.exports.iter_mut().map(RingPump::pump).sum();
        self.dropped += dropped;
    }

    pub(crate) fn dropped(&self) -> u64 {
        self.dropped
    }

    pub(crate) fn drain_dropped(&mut self) -> u64 {
        std::mem::take(&mut self.dropped)
    }
}
