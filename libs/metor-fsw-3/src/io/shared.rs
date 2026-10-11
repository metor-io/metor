//! `IoShared`: what a build's IO systems, models, guests, channel adapters,
//! and io steps share, behind `MakeCx::io`.

use core::cell::RefCell;
use std::rc::Rc;

use super::arena::Arena;

/// The state one build's IO shares.
///
/// A target's build makes one member per system with a `Cap`, in config
/// order, and its dispatch and backend. A pack makes a member as each of its
/// systems is first made, and has no dispatch.
#[derive(Default)]
pub struct IoShared {
    /// Every system with a `Cap`, by member index: its id and arena.
    pub(crate) members: Vec<(String, Rc<Arena>)>,
    /// The dispatch and backend, in a target's build.
    #[cfg(all(feature = "host", unix))]
    pub(crate) host: Option<super::host::Host>,
}

/// `IoShared`, shared through `MakeCx::io`.
pub type SharedIo = Rc<RefCell<IoShared>>;

impl IoShared {
    /// A target's members, in config order.
    #[cfg(feature = "host")]
    pub(crate) fn with_members<'a>(ids: impl Iterator<Item = &'a str>) -> Self {
        let mut io = Self::default();
        ids.for_each(|id| drop(io.member(id)));
        io
    }

    /// System `id`'s arena, joining it as a member if it is not one.
    pub(crate) fn member(&mut self, id: &str) -> Rc<Arena> {
        if let Some(arena) = self.arena(id) {
            return arena;
        }
        let arena = Rc::new(Arena::new());
        self.members.push((id.to_string(), arena.clone()));
        arena
    }

    pub(crate) fn arena(&self, id: &str) -> Option<Rc<Arena>> {
        self.index(id).map(|m| self.members[m].1.clone())
    }

    pub(crate) fn index(&self, id: &str) -> Option<usize> {
        self.members.iter().position(|(m, _)| m == id)
    }

    /// Starts quiescing system `id`: cancels its ops in flight. False if it
    /// has no IO.
    #[cfg(feature = "host")]
    pub(crate) fn quiesce_system(&mut self, id: &str) -> bool {
        let Some(m) = self.index(id) else {
            return false;
        };
        #[cfg(all(feature = "host", unix))]
        if let Some(host) = &mut self.host {
            host.dispatch.quiesce_member(m);
        }
        let _ = m;
        true
    }

    /// Whether system `id` has nothing left in flight.
    #[cfg(feature = "host")]
    pub(crate) fn is_system_idle(&self, id: &str) -> bool {
        #[cfg(all(feature = "host", unix))]
        if let (Some(host), Some(m)) = (&self.host, self.index(id)) {
            return host.dispatch.member_idle(m);
        }
        let _ = id;
        true
    }

    #[cfg(feature = "host")]
    pub(crate) fn end_queiscing_system(&mut self, id: &str) {
        #[cfg(all(feature = "host", unix))]
        if let (Some(m), Some(host)) = (self.index(id), &mut self.host) {
            host.dispatch.end_queiscing_member(m);
        }
        let _ = id;
    }
}

#[cfg(all(feature = "host", unix))]
impl Drop for IoShared {
    /// Cancels every op in flight. If some never land, leaks every arena, so
    /// the kernel never writes freed memory.
    fn drop(&mut self) {
        if let Some(host) = &mut self.host
            && !host.teardown()
        {
            self.members
                .iter()
                .for_each(|(_, a)| core::mem::forget(a.clone()));
        }
    }
}
