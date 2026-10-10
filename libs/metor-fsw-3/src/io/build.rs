//! What one build's IO systems, model systems, and io steps share, behind
//! `MakeCx::io`.

use core::cell::RefCell;
use std::rc::Rc;

use super::arena::Arena;
use super::device::Arenas;
use super::{Grant, MAX_OPS, ResourceConfig};

/// The state one build's IO shares.
#[derive(Default)]
pub struct IoBuild {
    /// The target's resources, in config order.
    pub(crate) resources: Vec<ResourceConfig>,
    /// Each system's grants from the config, by system id.
    pub(crate) grants: Vec<(String, Vec<Grant>)>,
    /// Every system with a `Cap`, in make order. A member's index is its
    /// index in `arenas` and in the dispatch.
    pub(crate) members: Vec<Member>,
    pub(crate) arenas: Rc<Arenas>,
    /// The dispatch and backend, once `io_submit` is made.
    #[cfg(all(feature = "host", unix))]
    pub(crate) host: Option<super::host::Host>,
}

/// `IoBuild`, shared by the build, the channel adapter, and the io steps.
pub type SharedIo = Rc<RefCell<IoBuild>>;

/// One system with a `Cap`, kept across its makes.
pub(crate) struct Member {
    pub(crate) id: String,
    pub(crate) arena: Rc<Arena>,
    /// Its `Cap` parameters, in definition order. Grant `n` is `caps[n]`.
    pub(crate) caps: Vec<String>,
}

impl IoBuild {
    /// System `id`'s arena, made on its first make.
    pub(crate) fn member(&mut self, id: &str, caps: Vec<String>) -> Rc<Arena> {
        match self.arena(id) {
            Some(arena) => arena,
            None => self.join(id, caps, Arena::new()),
        }
    }

    /// Makes system `id` a member over an arena laid out elsewhere, such as
    /// a guest's.
    pub(crate) fn join(&mut self, id: &str, caps: Vec<String>, arena: Arena) -> Rc<Arena> {
        let arena = Rc::new(arena);
        self.arenas.push(arena.clone());
        self.members.push(Member {
            id: id.to_string(),
            arena: arena.clone(),
            caps,
        });
        arena
    }

    pub(crate) fn arena(&self, id: &str) -> Option<Rc<Arena>> {
        self.index(id).map(|m| self.members[m].arena.clone())
    }

    pub(crate) fn index(&self, id: &str) -> Option<usize> {
        self.members.iter().position(|m| m.id == id)
    }

    /// How many grants name `resource` in the config.
    pub(crate) fn grants_on(&self, resource: &str) -> usize {
        let grants = self.grants.iter().flat_map(|(_, g)| g);
        grants.filter(|g| g.resource == resource).count()
    }

    /// How many ops the device end on system `id`'s parameter `param` holds:
    /// `MAX_OPS` for each host grant on its resource, at least one.
    pub(crate) fn held_cap(&self, id: &str, param: &str) -> usize {
        let grants = self.grants.iter().filter(|(system, _)| system == id);
        let grant = grants.flat_map(|(_, g)| g).find(|g| g.param == param);
        let hosts = grant.map_or(1, |g| self.grants_on(&g.resource).saturating_sub(1));
        MAX_OPS * hosts.max(1)
    }

    /// Starts quiescing system `id`: cancels its ops in flight. False if it
    /// has no IO.
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

    /// Whether system `id` has nothing left in flight. Always true before
    /// `io_submit` is made.
    pub(crate) fn is_system_idle(&self, id: &str) -> bool {
        #[cfg(all(feature = "host", unix))]
        if let (Some(host), Some(m)) = (&self.host, self.index(id)) {
            return host.dispatch.member_idle(m);
        }
        let _ = id;
        true
    }

    pub(crate) fn end_queiscing_system(&mut self, id: &str) {
        #[cfg(all(feature = "host", unix))]
        if let (Some(m), Some(host)) = (self.index(id), &mut self.host) {
            host.dispatch.end_queiscing_member(m);
        }
        let _ = id;
    }
}
