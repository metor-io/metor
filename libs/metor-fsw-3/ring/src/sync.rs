//! Select standard atomics or Loom's tracked equivalents.

#[cfg(ring_loom)]
pub(crate) use loom::sync::Arc;
#[cfg(ring_loom)]
pub(crate) use loom::sync::atomic::{AtomicU64, Ordering, fence};

#[cfg(not(ring_loom))]
pub(crate) use std::sync::Arc;
#[cfg(not(ring_loom))]
pub(crate) use std::sync::atomic::{AtomicU64, Ordering, fence};

#[cfg(ring_loom)]
pub(crate) use loom::thread;
