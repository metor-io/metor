//! `IoState`: the IO state the coordinator owns, which makes reach through
//! `MakeCx::io` and steps through `StepCx::io`.

use std::rc::Rc;

use super::arena::Arena;
#[cfg(all(feature = "host", unix))]
use crate::coordinator::MakeError;

/// The IO state a build shares: each system's arena and, in a target's build,
/// the dispatch.
///
/// A target's build adds one member per system with a `Cap`, in config order.
/// A pack adds a member the first time each of its systems is made, and has
/// no dispatch.
#[derive(Default)]
pub struct IoState {
    /// Every system with a `Cap`, by member index: its id and arena.
    pub(crate) members: Vec<(String, Rc<Arena>)>,
    /// The dispatch and backend, in a target's build.
    #[cfg(all(feature = "host", unix))]
    pub(crate) host: Option<super::host::Host>,
}

impl IoState {
    /// A target's members, in config order.
    #[cfg(feature = "host")]
    pub(crate) fn with_members<'a>(ids: impl Iterator<Item = &'a str>) -> Self {
        let mut io = Self::default();
        ids.for_each(|id| drop(io.get_or_add_member(id)));
        io
    }

    /// System `id`'s arena, joining it as a member if it is not one.
    pub(crate) fn get_or_add_member(&mut self, id: &str) -> Rc<Arena> {
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

    /// System `id`'s member index, or `NoIoMember`.
    #[cfg(all(feature = "host", unix))]
    pub(crate) fn member_index(&self, id: &str) -> Result<usize, MakeError> {
        let system = id.to_string();
        self.index(id).ok_or(MakeError::NoIoMember { system })
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
impl Drop for IoState {
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
