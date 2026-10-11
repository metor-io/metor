//! The registry of named thread groups a build places async systems on.

use std::collections::HashMap;
use std::sync::{Arc, Weak};

use crate::coordinator::BuildError;

use super::group::{self, GroupHandle};

/// A `Threads` hands out one group per thread name, held only weakly: the
/// systems placed on a group own it, and a name whose group is gone respawns.
#[derive(Default)]
pub struct Threads {
    groups: HashMap<String, Weak<GroupHandle>>,
}

impl Threads {
    pub fn new() -> Self {
        Self::default()
    }

    /// The group named `name`, started if it is not already running.
    pub(crate) fn get_or_spawn(&mut self, name: &str) -> Result<Arc<GroupHandle>, BuildError> {
        if let Some(group) = self.groups.get(name).and_then(Weak::upgrade) {
            return Ok(group);
        }
        let group = group::spawn(name)?;
        self.groups.insert(name.to_string(), Arc::downgrade(&group));
        Ok(group)
    }
}
