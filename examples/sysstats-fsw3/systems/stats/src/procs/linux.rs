//! Processes from `/proc/<pid>/stat` and `/proc/<pid>/io`.

use core::fmt::Write;
use std::ffi::CStr;
use std::io;

use super::ProcStats;
use super::proc_pid::{self, Units};
use crate::sample::{Result, TextFile};

pub struct Source {
    units: Units,
    pids: Vec<u32>,
    path: String,
    file: TextFile,
}

impl Source {
    pub fn new(capacity: usize) -> Self {
        // SAFETY: `sysconf` has no memory preconditions.
        let (hz, page) = unsafe {
            (
                libc::sysconf(libc::_SC_CLK_TCK),
                libc::sysconf(libc::_SC_PAGESIZE),
            )
        };
        let units = Units {
            tick_ns: 1_000_000_000 / u64::try_from(hz).unwrap_or(100).max(1),
            page_bytes: u64::try_from(page).unwrap_or(4096),
        };
        Self {
            units,
            pids: Vec::with_capacity(capacity),
            path: String::with_capacity(32),
            file: TextFile::with_capacity(1024),
        }
    }

    /// Reads every process still running by the time its files are opened.
    pub fn read(&mut self, out: &mut Vec<(u32, ProcStats)>) -> Result<()> {
        out.clear();
        list_pids(&mut self.pids)?;
        for i in 0..self.pids.len() {
            let pid = self.pids[i];
            if let Some(stats) = self.read_pid(pid) {
                out.push((pid, stats));
            }
        }
        Ok(())
    }

    fn read_pid(&mut self, pid: u32) -> Option<ProcStats> {
        let units = self.units;
        let mut stats = proc_pid::parse_stat(self.read_file(pid, "stat")?, units).ok()?;
        if let Some((read, write)) = self
            .read_file(pid, "io")
            .and_then(|t| proc_pid::parse_io(t).ok())
        {
            stats.read_bytes = read;
            stats.write_bytes = write;
        }
        Some(stats)
    }

    fn read_file(&mut self, pid: u32, file: &str) -> Option<&str> {
        self.path.clear();
        write!(self.path, "/proc/{pid}/{file}").ok()?;
        self.file.read(&self.path).ok()
    }
}

/// Lists the numeric entries of `/proc` into `pids`.
fn list_pids(pids: &mut Vec<u32>) -> Result<()> {
    pids.clear();
    // SAFETY: the path is NUL-terminated.
    let dir = unsafe { libc::opendir(c"/proc".as_ptr()) };
    if dir.is_null() {
        return Err(io::Error::last_os_error().into());
    }
    loop {
        // SAFETY: `dir` is open until the `closedir` below.
        let entry = unsafe { libc::readdir(dir) };
        if entry.is_null() {
            break;
        }
        // SAFETY: `d_name` is NUL-terminated and valid until the next `readdir`.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if let Some(pid) = name.to_str().ok().and_then(|n| n.parse().ok()) {
            pids.push(pid);
        }
    }
    // SAFETY: `dir` came from `opendir` and is closed once.
    unsafe { libc::closedir(dir) };
    Ok(())
}
