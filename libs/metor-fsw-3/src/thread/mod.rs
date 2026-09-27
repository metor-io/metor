//! Placing an [`AsyncSystem`](crate::async_system::AsyncSystem): on a
//! background thread behind an adapter that copies its ports across, or,
//! in a build without OS threads, nowhere.

#[cfg(all(feature = "threads", not(target_family = "wasm")))]
mod os;
#[cfg(not(all(feature = "threads", not(target_family = "wasm"))))]
mod unsupported;

#[cfg(all(feature = "threads", not(target_family = "wasm")))]
pub(crate) use os::place;
#[cfg(all(feature = "threads", not(target_family = "wasm")))]
pub use os::{Thread, Threads};
#[cfg(not(all(feature = "threads", not(target_family = "wasm"))))]
pub use unsupported::Threads;
#[cfg(not(all(feature = "threads", not(target_family = "wasm"))))]
pub(crate) use unsupported::place;

/// The thread every async system lands on unless its config names another.
pub const DEFAULT_THREAD: &str = "default";
