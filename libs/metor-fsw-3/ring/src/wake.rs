//! Cross-process wait/wake on a shared `AtomicU32`, a futex in its
//! shared-memory form.
//!
//! This is the wake primitive for regions mapped into more than one process:
//! a waiter parks on a word inside a `MAP_SHARED` mapping and a peer process
//! wakes it through its own mapping of the same page. A private futex
//! backend cannot serve this: Linux's `FUTEX_PRIVATE_FLAG` keys the futex by
//! `(mm, uaddr)` rather than by the underlying page, macOS's default path
//! goes through libc++'s per-process contention table, and Windows'
//! `WaitOnAddress` is documented process-local with no shared form at all.
//!
//! The backends:
//!
//! - **Linux**: the `futex(2)` syscall with plain `FUTEX_WAIT`/`FUTEX_WAKE`
//!   (no private flag), keyed by the underlying page.
//! - **macOS**: `os_sync_wait_on_address` and friends with the `SHARED`
//!   flag, the public futex API available since macOS 14.4.
//!
//! # Contract
//!
//! [`wait`] returns once the word's value differs from `expected` at wait
//! time, when a peer calls [`wake_one`]/[`wake_all`], on signal delivery, or
//! **spuriously**. Callers must re-check their predicate in a loop; none of
//! these functions communicate state on their own. [`wait_timeout`]
//! additionally returns [`WaitOutcome::TimedOut`] once the timeout lapses,
//! and that verdict is likewise advisory: a wake can race the timeout, so the
//! caller's predicate, not the outcome, decides what happened.
//!
//! Waking is stateless and never blocks. Waking with no waiter is a no-op,
//! and a waker may pass a word the waiter's process has since unmapped (the
//! address is only a key).

use core::sync::atomic::AtomicU32;
use std::time::Duration;

/// How a [`wait_timeout`] returned. Advisory only; see the module contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitOutcome {
    /// Woken, value changed, interrupted, or spurious.
    Woken,
    /// The timeout lapsed while parked.
    TimedOut,
}

/// If the word holds `expected`, park until a wake (or spuriously).
#[inline]
pub fn wait(a: &AtomicU32, expected: u32) {
    let _ = platform::wait(a, expected, None);
}

/// If the word holds `expected`, park until a wake or `timeout`, whichever
/// comes first (or spuriously).
#[inline]
pub fn wait_timeout(a: &AtomicU32, expected: u32, timeout: Duration) -> WaitOutcome {
    platform::wait(a, expected, Some(timeout))
}

/// Wake one waiter parked on the word, in any process mapping it.
#[inline]
pub fn wake_one(a: &AtomicU32) {
    platform::wake(a, false);
}

/// Wake every waiter parked on the word, in any process mapping it.
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
        let rc = unsafe {
            libc::syscall(
                libc::SYS_futex,
                a as *const AtomicU32,
                libc::FUTEX_WAIT, // deliberately not FUTEX_PRIVATE_FLAG'd
                expected,
                ts_ptr,
            )
        };
        // EAGAIN (value already differed) and EINTR both fold into `Woken`;
        // the caller's predicate loop sorts out what actually happened.
        if rc == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ETIMEDOUT) {
            WaitOutcome::TimedOut
        } else {
            WaitOutcome::Woken
        }
    }

    pub fn wake(a: &AtomicU32, all: bool) {
        let n: i32 = if all { i32::MAX } else { 1 };
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

    // The public wait-on-address surface from <os/os_sync_wait_on_address.h>,
    // available since macOS 14.4. The SHARED flags are what make waits match
    // across processes mapping the same page; the default (non-shared) forms
    // key by process-local address, like every other platform's futex.
    const OS_SYNC_WAIT_ON_ADDRESS_SHARED: u32 = 0x1;
    const OS_SYNC_WAKE_BY_ADDRESS_SHARED: u32 = 0x1;
    /// `OS_CLOCK_MACH_ABSOLUTE_TIME`, the one defined `os_clockid_t`.
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
        // Success returns the count of remaining waiters (>= 0). Everything
        // except a timeout (EINTR, EFAULT on a racing unmap, a value
        // mismatch) folds into `Woken` for the caller's predicate loop.
        if rc < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ETIMEDOUT) {
            WaitOutcome::TimedOut
        } else {
            WaitOutcome::Woken
        }
    }

    pub fn wake(a: &AtomicU32, all: bool) {
        let addr = a as *const AtomicU32 as *mut c_void;
        // ENOENT (no waiter) is the expected idle case; nothing to handle.
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

    /// A wait against a value the word no longer holds returns immediately.
    #[test]
    fn changed_value_returns_immediately() {
        let a = AtomicU32::new(1);
        wait(&a, 0); // must not hang
        assert_eq!(
            wait_timeout(&a, 0, Duration::from_secs(5)),
            WaitOutcome::Woken
        );
    }

    /// A wait on a silent word times out, and not meaningfully early.
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

    /// A store + wake releases a parked waiter. The store-before-wake order
    /// means a waiter that has not parked yet sees the new value and never
    /// parks, so the test cannot deadlock on the race.
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

    /// A store+wake / check+wait pair never loses an update: two threads
    /// ping-pong a counter through two futex words for many rounds. A single
    /// lost wake deadlocks the test (caught by the harness timeout).
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

    /// `wake_all` releases every parked waiter, not just one.
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
