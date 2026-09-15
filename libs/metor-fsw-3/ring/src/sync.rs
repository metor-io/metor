//! Wrapper module around loom or std sync primitives

#[cfg(ring_loom)]
pub(crate) use loom::sync::Arc;
#[cfg(ring_loom)]
pub(crate) use loom::sync::atomic::{AtomicU64, Ordering, fence};

#[cfg(not(ring_loom))]
pub(crate) use std::sync::Arc;
#[cfg(not(ring_loom))]
pub(crate) use std::sync::atomic::{AtomicU64, Ordering, fence};

/// Only the loom models spawn through the shim; `tests.rs` is std-only and
/// uses `std::thread` directly.
#[cfg(ring_loom)]
pub(crate) use loom::thread;
