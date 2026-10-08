//! Maps one system's handles to the `Os`'s.
//!
//! Slot `n` is the system's `n`-th grant. A handle an op opens, such as an
//! accepted stream, takes a free slot past the grants and keeps its parent's
//! resource.

use super::RawHandle;

#[derive(Clone, Copy)]
struct Slot {
    os: RawHandle,
    res: u16,
    generation: u32,
    live: bool,
}

/// A system handle, resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Mapped {
    pub(crate) os: RawHandle,
    pub(crate) res: u16,
    /// Whether an op opened it, rather than a grant.
    pub(crate) derived: bool,
}

pub(crate) struct Handles {
    slots: Box<[Slot]>,
    grants: usize,
}

impl Handles {
    /// One slot per grant, `(os handle, resource)`, then `derived` free slots.
    pub(crate) fn new(grants: &[(RawHandle, u16)], derived: usize) -> Self {
        let grant = |&(os, res): &(RawHandle, u16)| Slot {
            os,
            res,
            generation: 0,
            live: true,
        };
        let free = Slot {
            os: RawHandle::new(0, 0),
            res: 0,
            generation: 0,
            live: false,
        };
        let slots = grants.iter().map(grant).chain((0..derived).map(|_| free));
        Self {
            slots: slots.collect(),
            grants: grants.len(),
        }
    }

    /// The live handle `raw` names.
    pub(crate) fn get(&self, raw: RawHandle) -> Option<Mapped> {
        let at = raw.index() as usize;
        let slot = self.slots.get(at)?;
        let live = slot.live && slot.generation == raw.generation();
        live.then_some(Mapped {
            os: slot.os,
            res: slot.res,
            derived: at >= self.grants,
        })
    }

    /// Ends `raw`. Later ops on it are stale.
    pub(crate) fn close(&mut self, raw: RawHandle) {
        if self.get(raw).is_some() {
            self.slots[raw.index() as usize].live = false;
        }
    }

    /// Maps a new `Os` handle on resource `res`, or `None` with no free slot.
    pub(crate) fn derive(&mut self, os: RawHandle, res: u16) -> Option<RawHandle> {
        let at = self.grants + self.slots[self.grants..].iter().position(|s| !s.live)?;
        let slot = &mut self.slots[at];
        slot.generation = slot.generation.wrapping_add(1);
        *slot = Slot {
            os,
            res,
            live: true,
            ..*slot
        };
        Some(RawHandle::new(at as u32, slot.generation))
    }

    /// Makes every grant live again, for a remade system.
    pub(crate) fn revive_grants(&mut self) {
        self.slots[..self.grants]
            .iter_mut()
            .for_each(|s| s.live = true);
    }

    /// Ends every derived handle, handing each `Os` handle to `close`.
    pub(crate) fn drop_derived(&mut self, mut close: impl FnMut(RawHandle)) {
        for slot in self.slots[self.grants..].iter_mut().filter(|s| s.live) {
            slot.live = false;
            close(slot.os);
        }
    }
}
