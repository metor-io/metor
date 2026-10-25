//! Processes from `proc_listallpids`, `proc_pidinfo`, and `proc_pid_rusage`.
//! Without root, only this user's processes can be read.

use metor_fsw_3::FrameStr;

use super::ProcStats;
use crate::sample::{Result, SampleError};

pub struct Source {
    pids: Vec<libc::pid_t>,
    /// Mach time to nanoseconds, as `numer / denom`.
    timebase: (u64, u64),
}

impl Source {
    // libc marks the Mach timebase API deprecated in favor of `mach2`.
    #[allow(deprecated)]
    pub fn new(capacity: usize) -> Self {
        let mut info = libc::mach_timebase_info { numer: 1, denom: 1 };
        // SAFETY: `info` is a valid out-pointer.
        unsafe { libc::mach_timebase_info(&mut info) };
        Self {
            pids: vec![0; capacity],
            timebase: (u64::from(info.numer), u64::from(info.denom.max(1))),
        }
    }

    pub fn read(&mut self, out: &mut Vec<(u32, ProcStats)>) -> Result<()> {
        out.clear();
        let count = self.list()?;
        for &pid in &self.pids[..count] {
            let Ok(key) = u32::try_from(pid) else {
                continue;
            };
            if let Some(stats) = self.read_pid(pid) {
                out.push((key, stats));
            }
        }
        Ok(())
    }

    fn list(&mut self) -> Result<usize> {
        let bytes = i32::try_from(self.pids.len() * size_of::<libc::pid_t>()).unwrap_or(i32::MAX);
        // SAFETY: `pids` holds `bytes` writable bytes.
        let count = unsafe { libc::proc_listallpids(self.pids.as_mut_ptr().cast(), bytes) };
        if count < 0 {
            return Err(SampleError::Os {
                call: "proc_listallpids",
                code: count,
            });
        }
        Ok((count as usize).min(self.pids.len()))
    }

    fn read_pid(&self, pid: libc::pid_t) -> Option<ProcStats> {
        let task = task_info(pid)?;
        let ns = |t: u64| {
            (u128::from(t) * u128::from(self.timebase.0) / u128::from(self.timebase.1)) as u64
        };
        let (user_ns, system_ns) = (ns(task.pti_total_user), ns(task.pti_total_system));
        let (read_bytes, write_bytes) = disk_io(pid).unwrap_or((0, 0));
        Some(ProcStats {
            cpu_ns: user_ns.saturating_add(system_ns),
            user_ns,
            system_ns,
            rss_bytes: task.pti_resident_size,
            threads: u64::try_from(task.pti_threadnum).unwrap_or(0),
            read_bytes,
            write_bytes,
            cpu_util: 0.0,
            name: name(pid),
        })
    }
}

fn task_info(pid: libc::pid_t) -> Option<libc::proc_taskinfo> {
    // SAFETY: `proc_taskinfo` is plain integers, valid when zeroed.
    let mut info: libc::proc_taskinfo = unsafe { core::mem::zeroed() };
    let size = size_of::<libc::proc_taskinfo>() as libc::c_int;
    // SAFETY: `info` holds `size` writable bytes.
    let got =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTASKINFO, 0, (&raw mut info).cast(), size) };
    (got == size).then_some(info)
}

fn disk_io(pid: libc::pid_t) -> Option<(u64, u64)> {
    // SAFETY: `rusage_info_v2` is plain integers, valid when zeroed.
    let mut info: libc::rusage_info_v2 = unsafe { core::mem::zeroed() };
    // SAFETY: `info` is a `rusage_info_v2`, which the flavor fills.
    let code = unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V2, (&raw mut info).cast()) };
    (code == 0).then_some((info.ri_diskio_bytesread, info.ri_diskio_byteswritten))
}

fn name(pid: libc::pid_t) -> FrameStr<16> {
    let mut buf = [0u8; 64];
    // SAFETY: `buf` holds `buf.len()` writable bytes.
    let len = unsafe { libc::proc_name(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    let len = usize::try_from(len).unwrap_or(0).min(buf.len());
    FrameStr::truncated(core::str::from_utf8(&buf[..len]).unwrap_or(""))
}
