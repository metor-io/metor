//! Mach ports and `sysctl` reads shared by the macOS sources.

use std::ffi::CStr;
use std::sync::OnceLock;

use crate::sample::{Result, SampleError};

/// This host's port, looked up once.
pub fn host() -> libc::host_t {
    static HOST: OnceLock<libc::host_t> = OnceLock::new();
    // SAFETY: `mach_host_self` has no preconditions.
    #[allow(deprecated)]
    *HOST.get_or_init(|| unsafe { libc::mach_host_self() })
}

/// This task's port.
pub fn task() -> libc::mach_port_t {
    // SAFETY: `mach_task_self` reads a port the runtime set at startup.
    #[allow(deprecated)]
    unsafe {
        libc::mach_task_self()
    }
}

/// Reads the `sysctl` `name` into a value of type `T`.
pub fn sysctl<T: zerocopy::FromBytes + zerocopy::IntoBytes>(name: &CStr) -> Result<T> {
    let mut value = T::new_zeroed();
    let mut len = size_of::<T>();
    // SAFETY: `value` is `len` writable bytes, and any bytes are a valid `T`.
    let code = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            value.as_mut_bytes().as_mut_ptr().cast(),
            &mut len,
            core::ptr::null_mut(),
            0,
        )
    };
    if code != 0 || len != size_of::<T>() {
        return Err(SampleError::Os {
            call: "sysctlbyname",
            code: std::io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(code),
        });
    }
    Ok(value)
}
