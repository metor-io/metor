//! Channels: systems commanded from outside the target by name.
//!
//! A channel's commands arrive as `SequenceCommand` messages. [`Abort`] is
//! how a system hears an abort, and [`ChannelStatus`] is the state a channel
//! reports each cycle.

#[cfg(feature = "host")]
mod adapter;
mod apply;
mod records;
mod registry;

use metor_fsw_3_ring::{NoWake, WakeSink};
use serde::{Deserialize, Serialize};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::coordinator::{EndReason, RunState};
use crate::port::Input;
use crate::{Frame, FrameBytes, FrameStr, MaxSize, Record, Schema, Timestamp};

/// Asks a system to finish what it is doing and stop.
#[derive(Record, MaxSize, Schema, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct AbortRequest {}

/// The port an abort arrives on, whatever the parameter is called.
pub const ABORT_PORT: &str = "abort";
/// The input a channel reads its commands from.
pub const COMMANDS_PORT: &str = "commands";
/// The output a channel reports each accepted or refused command on.
pub const EVENTS_PORT: &str = "events";
/// The output a channel writes its [`ChannelStatus`] to each cycle.
pub const STATUS_PORT: &str = "channel_status";

#[cfg(feature = "host")]
pub use adapter::Channel;
#[cfg(feature = "host")]
pub(crate) use adapter::{def, make, ports};
pub use apply::{Command, Refusal, State, apply};
pub use registry::{ChannelSpec, Registry, RegistryParams, channel_registry};

/// Whether an abort has been asked for, read from the system's `abort` input.
pub struct Abort<W: WakeSink = NoWake> {
    input: Input<AbortRequest, W>,
    requested: bool,
}

impl<W: WakeSink> Abort<W> {
    pub(crate) fn new(input: Input<AbortRequest, W>) -> Self {
        Self {
            input,
            requested: false,
        }
    }

    /// Whether an abort has arrived; once true it stays true.
    pub fn requested(&mut self) -> bool {
        if self.input.drain().count() > 0 {
            self.requested = true;
        }
        self.requested
    }
}

/// One channel's state, written every cycle.
#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy, Debug)]
#[repr(C)]
pub struct ChannelStatus {
    #[frame(timestamp)]
    pub timestamp: Timestamp,
    /// A [`ChannelState`] code.
    pub state: u32,
    /// The occupant's index in the registry's `available`, or `u32::MAX`.
    pub loaded: u32,
    /// The occupant's library file name; empty when there is none.
    pub artifact: FrameStr<64>,
    /// The blake3 of the occupant's library; zeros when there is none.
    pub hash: FrameBytes<32>,
}

/// What [`ChannelStatus::state`] reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelState {
    Running,
    Stopped,
    Completed,
    Aborted,
    Failed,
    Panicked,
    Unloaded,
}

impl ChannelState {
    /// A channel's state: its end if it has one, otherwise its run state.
    pub fn of(run: RunState, end: Option<&EndReason>) -> Self {
        match (end, run) {
            (Some(EndReason::Completed), _) => Self::Completed,
            (Some(EndReason::Aborted), _) => Self::Aborted,
            (Some(EndReason::Failed(_)), _) => Self::Failed,
            (Some(EndReason::Panicked), _) => Self::Panicked,
            (Some(EndReason::Unloaded), _) => Self::Unloaded,
            (None, RunState::Running) => Self::Running,
            (None, RunState::Stopped) => Self::Stopped,
        }
    }

    /// The word written to [`ChannelStatus::state`].
    pub fn code(self) -> u32 {
        self as u32
    }
}

#[cfg(test)]
mod tests;
