//! Channels: named handles for commanding a system from outside the target.
//!
//! An operator starts, stops, aborts, resets, or loads a system by sending
//! `SequenceCommand` messages addressed to its channel's name. Each cycle
//! the channel reports its state as a [`ChannelStatus`], and each thing that
//! happens on it as a [`ChannelMessage`]. A system hears abort requests
//! through an [`Abort`] parameter.

#[cfg(feature = "host")]
mod adapter;
mod apply;
mod records;
mod registry;

use metor_fsw_3_ring::NoWake;
use metor_proto_wkt::SequenceChannelState;
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
/// The output that a channel writes its [`ChannelStatus`] to each cycle.
pub const STATUS_PORT: &str = "channel_status";
/// The output that a channel writes each [`ChannelMessage`] to.
pub const MESSAGE_PORT: &str = "channel_message";

#[cfg(feature = "host")]
pub use adapter::Channel;
#[cfg(feature = "host")]
pub(crate) use adapter::{channel_ports, def, make_channel};
pub use apply::{Command, Phase, Refusal, State, accepts, apply};
pub use registry::{ChannelSpec, Registry, RegistryParams, StatusComponents, channel_registry};

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
    /// A [`SequenceChannelState`] code.
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
    /// Seconds the loaded system has run since its last reset or load. It
    /// pauses while the channel is stopped and holds once the run ends.
    pub elapsed: f64,
    /// Seconds since an accepted abort, while the system winds down; zero
    /// when no abort is in progress.
    pub abort_elapsed: f64,
    /// The commands the channel would accept now, as
    /// [`SequenceAccepts`](metor_proto_wkt::SequenceAccepts) bits.
    pub accepts: u32,
    #[frame(skip)]
    _pad: u32,
}

/// Something that happened on a channel: a transition, a refused command, or
/// how its system ended.
///
/// Written once per message, only when one is raised, and stamped with the
/// cycle that raised it, so its samples are the channel's history. A repeated
/// text is still a new sample. A cycle raises one per command it applies and
/// one when the system ends, so its ring is as deep as the commands'.
#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy, Debug)]
#[frame(depth = 4)]
#[repr(C)]
pub struct ChannelMessage {
    #[frame(timestamp)]
    pub timestamp: Timestamp,
    /// What happened. Its length keeps the frame free of padding.
    pub text: FrameStr<MESSAGE_LEN>,
}

/// The bytes [`ChannelMessage::text`] takes, terminator included.
pub const MESSAGE_LEN: usize = 128;

/// Returns a channel's state.
///
/// If the channel has ended, this is how it ended. Otherwise it is the
/// channel's run state.
pub fn channel_state(run: RunState, end: Option<&EndReason>) -> SequenceChannelState {
    match (end, run) {
        (Some(EndReason::Completed), _) => SequenceChannelState::Completed,
        (Some(EndReason::Aborted), _) => SequenceChannelState::Aborted,
        (Some(EndReason::Failed(_)), _) => SequenceChannelState::Failed,
        (Some(EndReason::Panicked), _) => SequenceChannelState::Panicked,
        (Some(EndReason::Unloaded), _) => SequenceChannelState::Unloaded,
        (None, RunState::Running) => SequenceChannelState::Running,
        (None, RunState::Stopped) => SequenceChannelState::Stopped,
    }
}

#[cfg(test)]
mod tests;
