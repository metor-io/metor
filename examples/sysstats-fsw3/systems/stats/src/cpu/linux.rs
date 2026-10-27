//! CPU time from `/proc/stat`.

use super::{Ticks, proc_stat};
use crate::sample::{Result, TextFile};

pub struct Source {
    file: TextFile,
}

impl Source {
    pub fn new() -> Self {
        Self {
            file: TextFile::with_capacity(64 * 1024),
        }
    }

    pub fn read(&mut self, total: &mut Ticks, cores: &mut Vec<(u16, Ticks)>) -> Result<()> {
        proc_stat::parse(self.file.read("/proc/stat")?, total, cores)
    }
}
