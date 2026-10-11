//! Placing an [`AsyncSystem`](crate::async_system::AsyncSystem): on a
//! background thread behind an adapter that copies its ports across, or,
//! in a build without OS threads, nowhere.

#[cfg_attr(
    not(all(feature = "threads", not(target_family = "wasm"))),
    path = "unsupported.rs"
)]
mod os;

pub use os::Threads;
pub(crate) use os::place;

/// The thread every async system lands on unless its config names another.
pub const DEFAULT_THREAD: &str = "default";
