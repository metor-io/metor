//! Memory use from `/proc/meminfo`.

use super::{Mem, meminfo};
use crate::sample::{Result, TextFile};

pub struct Source {
    file: TextFile,
}

impl Source {
    pub fn new() -> Self {
        Self {
            file: TextFile::with_capacity(8 * 1024),
        }
    }

    pub fn read(&mut self) -> Result<Mem> {
        meminfo::parse(self.file.read("/proc/meminfo")?)
    }
}
