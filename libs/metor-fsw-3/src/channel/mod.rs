//! Channels: named handles for commanding a system from outside the target.
//!
//! An operator starts, stops, aborts, resets, or loads a system by sending
//! `SequenceCommand` messages addressed to its channel's name. Each cycle
//! the channel reports its state as a [`ChannelStatus`]. A system hears
//! abort requests through an [`Abort`] parameter.

#[cfg(feature = "host")]
mod adapter;
mod apply;
mod records;
mod registry;

use metor_fsw_3_ring::NoWake;
use serde::{Deserialize, Serialize};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::coordinator::{EndReason, RunState};
use crate::port::{Input, InputWake};
use crate::{Frame, FrameBytes, FrameStr, MaxSize, Record, Schema, Timestamp};

/// A request for a system to finish what it is doing and stop.
#[derive(Record, MaxSize, Schema, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct AbortRequest {}

/// The input that abort requests arrive on.
///
/// An [`Abort`] parameter always reads this port, whatever it is called.
pub const ABORT_PORT: &str = "abort";
/// The input that a channel reads commands from.
pub const COMMANDS_PORT: &str = "commands";
/// The output where a channel reports its events.
///
/// Events include each accepted or refused command, and how the system
/// ended.
pub const EVENTS_PORT: &str = "events";
/// The output that a channel writes its [`ChannelStatus`] to each cycle.
pub const STATUS_PORT: &str = "channel_status";

#[cfg(feature = "host")]
pub use adapter::Channel;
#[cfg(feature = "host")]
pub(crate) use adapter::{channel_ports, def, make_channel};
pub use apply::{Command, Phase, Refusal, State, apply};
pub use registry::{ChannelSpec, Registry, RegistryParams, channel_registry};

/// A system parameter that reports whether an abort was requested.
pub struct Abort<W: InputWake = NoWake> {
    input: Input<AbortRequest, W>,
    requested: bool,
}

impl<W: InputWake> Abort<W> {
    pub(crate) fn new(input: Input<AbortRequest, W>) -> Self {
        Self {
            input,
            requested: false,
        }
    }

    /// Returns whether an abort has been requested.
    ///
    /// Once this returns true, it keeps returning true.
    pub fn requested(&mut self) -> bool {
        if self.input.drain().count() > 0 {
            self.requested = true;
        }
        self.requested
    }
}

/// A channel's state, written every cycle.
#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy, Debug)]
#[repr(C)]
pub struct ChannelStatus {
    #[frame(timestamp)]
    pub timestamp: Timestamp,
    /// A [`ChannelState`] code.
    pub state: u32,
    /// The loaded occupant's index in the registry's `available` list, or
    /// `u32::MAX` if nothing is loaded.
    pub loaded: u32,
    /// The file name of the occupant's library, or empty if nothing is
    /// loaded.
    pub artifact: FrameStr<64>,
    /// The blake3 hash of the occupant's library, or zeros if nothing is
    /// loaded.
    pub hash: FrameBytes<32>,
}

/// The states a channel reports in [`ChannelStatus::state`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelState {
    Running,
    Stopped,
    Completed,
    Aborted,
    Failed,
    Panicked,
    Unloaded,
    /// Waiting for the system's ops in flight to finish before a reset or
    /// load takes effect.
    Quiescing,
}

impl ChannelState {
    /// Returns a channel's state.
    ///
    /// If the channel has ended, this is how it ended. Otherwise it is the
    /// channel's run state.
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

    /// Returns the code written to [`ChannelStatus::state`].
    pub fn code(self) -> u32 {
        self as u32
    }
}

#[cfg(test)]
mod tests;
