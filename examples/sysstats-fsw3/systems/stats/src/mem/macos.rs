//! Memory use from `host_statistics64`, `hw.memsize`, and `vm.swapusage`.

use super::Mem;
use crate::mach;
use crate::sample::{Result, SampleError};

pub struct Source {
    page: u64,
}

impl Source {
    pub fn new() -> Self {
        // SAFETY: `vm_page_size` is set by the runtime before main.
        let page = unsafe { libc::vm_page_size } as u64;
        Self { page }
    }

    pub fn read(&mut self) -> Result<Mem> {
        let vm = vm_stats()?;
        let pages = |n: libc::natural_t| u64::from(n) * self.page;
        // `xsw_usage` is `{ total, avail, used: u64, pagesize: u32, encrypted: u32 }`.
        let swap: [u64; 4] = mach::sysctl(c"vm.swapusage")?;
        let free = pages(vm.free_count) + pages(vm.speculative_count);
        Ok(Mem {
            total: mach::sysctl(c"hw.memsize")?,
            free,
            available: free + pages(vm.inactive_count) + pages(vm.purgeable_count),
            cached: pages(vm.external_page_count),
            active: pages(vm.active_count),
            inactive: pages(vm.inactive_count),
            wired: pages(vm.wire_count),
            compressed: pages(vm.compressor_page_count),
            swap_total: swap[0],
            swap_used: swap[2],
            ..Mem::zeroed()
        })
    }
}

fn vm_stats() -> Result<libc::vm_statistics64> {
    // SAFETY: `vm_statistics64` is plain integers, valid when zeroed.
    let mut stats: libc::vm_statistics64 = unsafe { core::mem::zeroed() };
    let mut count = libc::HOST_VM_INFO64_COUNT;
    // SAFETY: `stats` holds `count` integers.
    let code = unsafe {
        libc::host_statistics64(
            mach::host(),
            libc::HOST_VM_INFO64,
            (&raw mut stats).cast(),
            &mut count,
        )
    };
    if code != libc::KERN_SUCCESS {
        return Err(SampleError::Os {
            call: "host_statistics64",
            code,
        });
    }
    Ok(stats)
}
