//! Channels: systems commanded from outside the target by name.
//!
//! A channel's commands arrive as `SequenceCommand` messages. [`Abort`] is
//! how a system hears an abort, and [`ChannelStatus`] is the state a channel
//! reports each cycle.

mod records;

use metor_fsw_3_ring::{NoWake, WakeSink};
use serde::{Deserialize, Serialize};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::coordinator::{End, RunState};
use crate::port::Input;
use crate::{Frame, FrameStr, MaxSize, Record, Schema, Timestamp};

/// Asks a system to finish what it is doing and stop.
#[derive(Record, MaxSize, Schema, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct AbortRequest {}

/// The port an abort arrives on, whatever the parameter is called.
pub const ABORT_PORT: &str = "abort";

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
    pub hash: [u8; 32],
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
    pub fn of(run: RunState, end: Option<&End>) -> Self {
        match (end, run) {
            (Some(End::Completed), _) => Self::Completed,
            (Some(End::Aborted), _) => Self::Aborted,
            (Some(End::Failed(_)), _) => Self::Failed,
            (Some(End::Panicked), _) => Self::Panicked,
            (Some(End::Unloaded), _) => Self::Unloaded,
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
