//! The `stats` pack: host statistics sampled each cycle.
//!
//! [`cpu`], [`mem`], [`nics`], [`procs`], and [`disks`] each read one source
//! and write one frame. Each frame has one definition, and a per-OS module
//! fills it on Linux and macOS. The systems are async, so their reads run on
//! the thread their config names and set their own rate.

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("the stats pack supports Linux and macOS");

pub mod cpu;
pub mod disks;
pub mod mem;
pub mod nics;
pub mod procs;
pub mod sample;
pub mod set;

#[cfg(target_os = "macos")]
mod mach;

pub use cpu::{CoreStats, Cpu, CpuState};
pub use disks::{DiskStats, Disks, DisksState};
pub use mem::{Mem, MemState};
pub use nics::{NicStats, Nics, NicsState};
pub use procs::{ProcStats, Procs, ProcsState};

use metor_fsw_3::SystemTable;

/// Returns a table of the pack's systems, registered as `cpu`, `mem`,
/// `nics`, `procs`, and `disks`.
pub fn pack() -> SystemTable {
    let mut table = SystemTable::new();
    // PANIC Safety: these static record schemas are compatible.
    table
        .register_async("cpu", cpu::cpu(), CpuState::new)
        .expect("valid records");
    table
        .register_async("mem", mem::mem(), MemState::new)
        .expect("valid records");
    table
        .register_async("nics", nics::nics(), NicsState::new)
        .expect("valid records");
    table
        .register_async("procs", procs::procs(), ProcsState::new)
        .expect("valid records");
    table
        .register_async("disks", disks::disks(), DisksState::new)
        .expect("valid records");
    table
}

#[cfg(feature = "export")]
metor_fsw_3::export_pack!(pack);
