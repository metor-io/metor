//! What the io steps and the IO systems of one build share.

use std::cell::RefCell;
use std::rc::Rc;

use crate::coordinator::ParamError;

use super::Tables;
use super::queue::Table;

/// The state one build's IO systems share, behind `MakeCx::io`.
#[derive(Default)]
pub struct IoShared {
    /// The target's buffer tables. The `Os` and every `DeviceSide` resolve
    /// through it.
    pub(crate) tables: Tables,
    /// Every system with a `Cap`, in make order.
    pub(crate) members: Vec<Rc<Member>>,
}

/// `IoShared`, shared by the build, the channel adapter, and the io steps.
pub type SharedIo = Rc<RefCell<IoShared>>;

/// One IO system, kept across its makes.
pub(crate) struct Member {
    pub(crate) id: String,
    pub(crate) table: Rc<Table>,
}

impl IoShared {
    /// The member for system `id`, made on its first make. A later make
    /// rewinds its table, which fails while the old system holds a slot.
    pub(crate) fn member(&mut self, id: &str) -> Result<Rc<Member>, ParamError> {
        if let Some(member) = self.members.iter().find(|m| m.id == id) {
            member.table.bufs.rewind().map_err(|_| ParamError::IoBusy {
                system: id.to_string(),
            })?;
            return Ok(member.clone());
        }
        let table = Table::new();
        self.tables.add(&table.bufs);
        let member = Rc::new(Member {
            id: id.to_string(),
            table,
        });
        self.members.push(member.clone());
        Ok(member)
    }
}
