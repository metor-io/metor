//! CPU time from `host_processor_info`.

use super::Ticks;
use crate::mach;
use crate::sample::{Result, SampleError};

pub struct Source;

impl Source {
    pub fn new() -> Self {
        Self
    }

    pub fn read(&mut self, total: &mut Ticks, cores: &mut Vec<(u16, Ticks)>) -> Result<()> {
        cores.clear();
        *total = Ticks::default();
        let info = LoadInfo::query()?;
        for (id, cpu) in info.cpus().enumerate() {
            let ticks = ticks(cpu);
            add(total, &ticks);
            cores.push((u16::try_from(id).unwrap_or(u16::MAX), ticks));
        }
        Ok(())
    }
}

fn ticks(cpu: &[libc::integer_t]) -> Ticks {
    let at = |state: libc::c_int| u64::from(cpu[state as usize] as u32);
    Ticks {
        user: at(libc::CPU_STATE_USER),
        nice: at(libc::CPU_STATE_NICE),
        system: at(libc::CPU_STATE_SYSTEM),
        idle: at(libc::CPU_STATE_IDLE),
        ..Ticks::default()
    }
}

fn add(total: &mut Ticks, t: &Ticks) {
    total.user += t.user;
    total.nice += t.nice;
    total.system += t.system;
    total.idle += t.idle;
}

/// The per-CPU tick array the kernel allocates, freed on drop.
struct LoadInfo {
    info: libc::processor_info_array_t,
    len: libc::mach_msg_type_number_t,
    cpus: usize,
}

impl LoadInfo {
    fn query() -> Result<Self> {
        let mut cpus: libc::natural_t = 0;
        let mut info: libc::processor_info_array_t = core::ptr::null_mut();
        let mut len: libc::mach_msg_type_number_t = 0;
        // SAFETY: every out-pointer is valid; the kernel allocates `info`.
        let code = unsafe {
            libc::host_processor_info(
                mach::host(),
                libc::PROCESSOR_CPU_LOAD_INFO,
                &mut cpus,
                &mut info,
                &mut len,
            )
        };
        if code != libc::KERN_SUCCESS {
            return Err(SampleError::Os {
                call: "host_processor_info",
                code,
            });
        }
        Ok(Self {
            info,
            len,
            cpus: cpus as usize,
        })
    }

    fn cpus(&self) -> impl Iterator<Item = &[libc::integer_t]> {
        // SAFETY: the kernel returned `len` integers at `info`, valid until drop.
        let all = unsafe { core::slice::from_raw_parts(self.info, self.len as usize) };
        let stride = libc::CPU_STATE_MAX as usize;
        all.chunks_exact(stride).take(self.cpus)
    }
}

impl Drop for LoadInfo {
    fn drop(&mut self) {
        let bytes = self.len as usize * size_of::<libc::integer_t>();
        // SAFETY: `info` is the kernel's allocation of `bytes` in this task.
        unsafe {
            libc::vm_deallocate(mach::task(), self.info as libc::vm_address_t, bytes);
        }
    }
}
