//! What the io steps and the IO systems of one build share.
//!
//! The io steps and the channel adapter that read most of it need `host`.
#![cfg_attr(not(all(feature = "host", unix)), allow(dead_code))]

use core::cell::Cell;
use std::cell::RefCell;
use std::rc::Rc;

use crate::coordinator::ParamError;

use super::queue::Table;
use super::region::Region;
use super::{Corrupt, Tables};

/// The state one build's IO systems share, behind `MakeCx::io`.
#[derive(Default)]
pub struct IoShared {
    /// The target's buffer tables. The `Os` and every `DeviceSide` resolve
    /// through it.
    pub(crate) tables: Tables,
    /// Every system with a `Cap`, in make order.
    pub(crate) members: Vec<Rc<Member>>,
    /// The target's resources, in config order.
    #[cfg(all(feature = "host", unix))]
    pub(crate) resources: Vec<super::ResourceConfig>,
    /// Each system's grants from the config, by system id.
    #[cfg(all(feature = "host", unix))]
    pub(crate) grants: Vec<(String, Vec<super::Grant>)>,
    /// The reactor and what the io steps keep, once `io_submit` is made.
    #[cfg(all(feature = "host", unix))]
    pub(crate) host: Option<super::system::IoHost>,
}

/// Whether a member is waiting for its old ops to finish before a remake.
#[derive(Default)]
pub(crate) struct Quiesce {
    pub(crate) active: Cell<bool>,
    /// The member's reactor source, once `io_submit` is made.
    pub(crate) src: Cell<Option<u16>>,
    /// `io_submit` has drained the member's SQ since `begin`.
    pub(crate) drained: Cell<bool>,
}

/// `IoShared`, shared by the build, the channel adapter, and the io steps.
pub type SharedIo = Rc<RefCell<IoShared>>;

/// One IO system, kept across its makes.
pub(crate) struct Member {
    pub(crate) id: String,
    pub(crate) table: Rc<Table>,
    /// The system's `Cap` parameters, in definition order. Grant `n` is
    /// `caps[n]`.
    pub(crate) caps: Vec<String>,
    pub(crate) quiesce: Quiesce,
}

impl IoShared {
    /// The member for system `id`, whose `Cap` parameters are `caps`, made on
    /// its first make. A later make rewinds its table, which fails while the
    /// old system holds a slot.
    pub(crate) fn member(&mut self, id: &str, caps: Vec<String>) -> Result<Rc<Member>, ParamError> {
        if let Some(member) = self.members.iter().find(|m| m.id == id) {
            member.table.bufs.rewind().map_err(|_| ParamError::IoBusy {
                system: id.to_string(),
            })?;
            return Ok(member.clone());
        }
        Ok(self.join(id, caps, Table::new()))
    }

    /// Makes system `id`'s member over `region`, an arena laid out
    /// elsewhere. Its later makes rewind it through `member`.
    #[cfg_attr(not(test), expect(dead_code, reason = "the pack ABI attaches regions"))]
    pub(crate) fn adopt_region(
        &mut self,
        id: &str,
        caps: Vec<String>,
        region: Region,
    ) -> Result<Rc<Member>, Corrupt> {
        Ok(self.join(id, caps, Table::attach(region)?))
    }

    fn join(&mut self, id: &str, caps: Vec<String>, table: Rc<Table>) -> Rc<Member> {
        self.tables.add(&table.bufs);
        let member = Rc::new(Member {
            id: id.to_string(),
            table,
            caps,
            quiesce: Quiesce::default(),
        });
        self.members.push(member.clone());
        member
    }
}

impl IoShared {
    /// How many grants name `resource` in the config.
    #[cfg(all(feature = "host", unix))]
    pub(crate) fn grants_on(&self, resource: &str) -> usize {
        let grants = self.grants.iter().flat_map(|(_, g)| g);
        grants.filter(|g| g.resource == resource).count()
    }

    /// How many ops the device end on system `id`'s parameter `param` holds:
    /// `MAX_OPS` for each host grant on its resource, at least one.
    pub(crate) fn held_cap(&self, id: &str, param: &str) -> usize {
        #[cfg(all(feature = "host", unix))]
        let hosts = self
            .grants
            .iter()
            .filter(|(system, _)| system == id)
            .flat_map(|(_, g)| g)
            .find(|g| g.param == param)
            .map_or(1, |g| self.grants_on(&g.resource).saturating_sub(1));
        #[cfg(not(all(feature = "host", unix)))]
        let hosts = {
            let _ = (id, param);
            1
        };
        super::MAX_OPS * hosts.max(1)
    }

    pub(crate) fn find(&self, id: &str) -> Option<Rc<Member>> {
        self.members.iter().find(|m| m.id == id).cloned()
    }

    /// Starts quiescing `m`: cancels its ops in flight.
    pub(crate) fn begin(&mut self, m: &Member) {
        m.quiesce.active.set(true);
        m.quiesce.drained.set(false);
        #[cfg(all(feature = "host", unix))]
        if let (Some(host), Some(src)) = (&mut self.host, m.quiesce.src.get()) {
            host.reactor.cancel_all(super::SourceId(src));
        }
    }

    /// Whether `m` has nothing left in flight: always, before `io_submit`
    /// is made; after, once its SQ is drained and the reactor holds none of
    /// its ops.
    pub(crate) fn idle(&self, m: &Member) -> bool {
        #[cfg(all(feature = "host", unix))]
        if let (Some(host), Some(src)) = (&self.host, m.quiesce.src.get()) {
            let none = host.reactor.in_flight(super::SourceId(src)) == 0;
            return m.quiesce.drained.get() && none;
        }
        let _ = m;
        true
    }

    pub(crate) fn end(&self, m: &Member) {
        m.quiesce.active.set(false);
        m.quiesce.drained.set(false);
    }
}
