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
pub(crate) use os::TurnedGroup;
pub(crate) use os::place;

use metor_fsw_3_ring::Notifier;

use crate::async_system::AsyncSystem;
use crate::coordinator::Params;
use crate::fn_system::Instance;
use crate::system::{InputBinding, OutputBinding};

/// What an async system's make takes on the thread that runs it.
pub(crate) struct LaunchCx<'a> {
    pub(crate) params: Params<'a>,
    pub(crate) inputs: Vec<InputBinding<Notifier>>,
    pub(crate) outputs: Vec<OutputBinding>,
    pub(crate) instance: Instance,
}

/// An async system, its state, and its bound ports.
#[cfg_attr(
    not(all(feature = "threads", not(target_family = "wasm"))),
    allow(dead_code)
)]
pub(crate) struct Launched<A: AsyncSystem> {
    pub(crate) system: A,
    pub(crate) state: A::State,
    pub(crate) inputs: A::Inputs,
    pub(crate) outputs: A::Outputs,
}

/// The thread an async system runs on when its config names no thread.
pub const DEFAULT_THREAD: &str = "default";
