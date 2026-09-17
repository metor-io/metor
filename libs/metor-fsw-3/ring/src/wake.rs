//! Wait and wake on a shared atomic word across processes.
//!
//! Linux uses shared futexes; macOS requires the shared wait-on-address API
//! available since 14.4. Both processes must map the same underlying memory.
//! Waits can return spuriously or race a timeout. Always recheck the predicate.
//! Waking without a waiter has no effect.

use core::sync::atomic::AtomicU32;
use std::time::Duration;

/// Why a timed wait returned. Recheck the predicate in either case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitOutcome {
    /// A wake, value change, interruption, or spurious return.
    Woken,
    /// The timeout expired; a concurrent wake may still have occurred.
    TimedOut,
}

/// Wait while the word equals `expected`. May return spuriously.
#[inline]
pub fn wait(a: &AtomicU32, expected: u32) {
    let _ = platform::wait(a, expected, None);
}

/// Wait while the word equals `expected`, for at most `timeout`. May return early.
#[inline]
pub fn wait_timeout(a: &AtomicU32, expected: u32, timeout: Duration) -> WaitOutcome {
    platform::wait(a, expected, Some(timeout))
}

/// Wake one waiter on this word, including in another process.
#[inline]
pub fn wake_one(a: &AtomicU32) {
    platform::wake(a, false);
}

/// Wake all waiters on this word, including in other processes.
#[inline]
pub fn wake_all(a: &AtomicU32) {
    platform::wake(a, true);
}

#[cfg(any(target_os = "linux", target_os = "android"))]
mod platform {
    use super::WaitOutcome;
    use core::sync::atomic::AtomicU32;
    use std::time::Duration;

    pub fn wait(a: &AtomicU32, expected: u32, timeout: Option<Duration>) -> WaitOutcome {
        // A relative timeout for FUTEX_WAIT; null means wait forever.
        let ts = timeout.map(|t| libc::timespec {
            tv_sec: t.as_secs().min(libc::time_t::MAX as u64) as libc::time_t,
            tv_nsec: t.subsec_nanos() as _,
        });
        let ts_ptr = ts
            .as_ref()
            .map_or(core::ptr::null(), |ts| ts as *const libc::timespec);
        // SAFETY: `a` is a live, aligned atomic; `ts_ptr` is null or valid here.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_futex,
                a as *const AtomicU32,
                libc::FUTEX_WAIT,
                expected,
                ts_ptr,
            )
        };
        // Only timeout needs a separate result; callers recheck after other returns.
        if rc == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ETIMEDOUT) {
            WaitOutcome::TimedOut
        } else {
            WaitOutcome::Woken
        }
    }

    pub fn wake(a: &AtomicU32, all: bool) {
        let n: i32 = if all { i32::MAX } else { 1 };
        // SAFETY: `a` is a live, aligned atomic.
        unsafe {
            libc::syscall(libc::SYS_futex, a as *const AtomicU32, libc::FUTEX_WAKE, n);
        }
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::WaitOutcome;
    use core::ffi::{c_int, c_void};
    use core::sync::atomic::AtomicU32;
    use std::time::Duration;

    // Shared flags match waiters across mappings of the same page.
    const OS_SYNC_WAIT_ON_ADDRESS_SHARED: u32 = 0x1;
    const OS_SYNC_WAKE_BY_ADDRESS_SHARED: u32 = 0x1;
    const OS_CLOCK_MACH_ABSOLUTE_TIME: u32 = 32;

    unsafe extern "C" {
        fn os_sync_wait_on_address(addr: *mut c_void, value: u64, size: usize, flags: u32)
        -> c_int;
        fn os_sync_wait_on_address_with_timeout(
            addr: *mut c_void,
            value: u64,
            size: usize,
            flags: u32,
            clockid: u32,
            timeout_ns: u64,
        ) -> c_int;
        fn os_sync_wake_by_address_any(addr: *mut c_void, size: usize, flags: u32) -> c_int;
        fn os_sync_wake_by_address_all(addr: *mut c_void, size: usize, flags: u32) -> c_int;
    }

    pub fn wait(a: &AtomicU32, expected: u32, timeout: Option<Duration>) -> WaitOutcome {
        let addr = a as *const AtomicU32 as *mut c_void;
        // SAFETY: `addr` points to a live, aligned four-byte atomic for each call.
        let rc = match timeout {
            None => unsafe {
                os_sync_wait_on_address(addr, expected as u64, 4, OS_SYNC_WAIT_ON_ADDRESS_SHARED)
            },
            Some(t) => unsafe {
                os_sync_wait_on_address_with_timeout(
                    addr,
                    expected as u64,
                    4,
                    OS_SYNC_WAIT_ON_ADDRESS_SHARED,
                    OS_CLOCK_MACH_ABSOLUTE_TIME,
                    // A relative timeout in nanoseconds; zero is rejected
                    // (EINVAL), so clamp to the shortest real wait.
                    t.as_nanos().clamp(1, u64::MAX as u128) as u64,
                )
            },
        };
        // Nonnegative results succeed; callers recheck after other errors.
        if rc < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ETIMEDOUT) {
            WaitOutcome::TimedOut
        } else {
            WaitOutcome::Woken
        }
    }

    pub fn wake(a: &AtomicU32, all: bool) {
        let addr = a as *const AtomicU32 as *mut c_void;
        // SAFETY: `addr` points to a live, aligned four-byte atomic.
        // No waiter is an expected result and needs no handling.
        unsafe {
            if all {
                os_sync_wake_by_address_all(addr, 4, OS_SYNC_WAKE_BY_ADDRESS_SHARED);
            } else {
                os_sync_wake_by_address_any(addr, 4, OS_SYNC_WAKE_BY_ADDRESS_SHARED);
            }
        }
    }
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicU32, Ordering::SeqCst};
    use std::time::{Duration, Instant};

    #[test]
    fn changed_value_returns_immediately() {
        let a = AtomicU32::new(1);
        wait(&a, 0); // must not hang
        assert_eq!(
            wait_timeout(&a, 0, Duration::from_secs(5)),
            WaitOutcome::Woken
        );
    }

    #[test]
    fn timeout_lapses_on_silence() {
        let a = AtomicU32::new(0);
        let start = Instant::now();
        let outcome = wait_timeout(&a, 0, Duration::from_millis(50));
        assert_eq!(outcome, WaitOutcome::TimedOut);
        assert!(
            start.elapsed() >= Duration::from_millis(40),
            "{:?}",
            start.elapsed()
        );
    }

    #[test]
    fn wake_one_releases_waiter() {
        let a = AtomicU32::new(0);
        std::thread::scope(|s| {
            s.spawn(|| {
                while a.load(SeqCst) == 0 {
                    wait(&a, 0);
                }
            });
            a.store(1, SeqCst);
            wake_one(&a);
        });
    }

    #[test]
    fn handoff_loses_no_wakeups() {
        const ROUNDS: u32 = 10_000;
        let ping = AtomicU32::new(0);
        let pong = AtomicU32::new(0);
        std::thread::scope(|s| {
            s.spawn(|| {
                for i in 1..=ROUNDS {
                    while ping.load(SeqCst) < i {
                        wait(&ping, i - 1);
                    }
                    pong.store(i, SeqCst);
                    wake_one(&pong);
                }
            });
            for i in 1..=ROUNDS {
                ping.store(i, SeqCst);
                wake_one(&ping);
                while pong.load(SeqCst) < i {
                    wait(&pong, i - 1);
                }
            }
        });
    }

    #[test]
    fn wake_all_releases_every_waiter() {
        let a = AtomicU32::new(0);
        std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(|| {
                    while a.load(SeqCst) == 0 {
                        wait(&a, 0);
                    }
                });
            }
            a.store(1, SeqCst);
            wake_all(&a);
        });
    }
}
