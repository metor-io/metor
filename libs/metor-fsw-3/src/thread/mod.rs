//! Runs [`AsyncSystem`](crate::async_system::AsyncSystem)s on background
//! threads.
//!
//! Each async system runs on a named thread, shared with any other system
//! that names the same thread. An adapter step on the coordinator's cycle
//! copies records between the system's ports and the real rings. In a build
//! without OS threads, placing an async system is an error.

#[cfg_attr(
    not(all(feature = "threads", not(target_family = "wasm"))),
    path = "unsupported.rs"
)]
mod os;

pub use os::Threads;
pub(crate) use os::place;

/// The thread an async system runs on when its config names no thread.
pub const DEFAULT_THREAD: &str = "default";
