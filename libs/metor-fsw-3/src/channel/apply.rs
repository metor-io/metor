//! The rules for which commands a channel accepts in each state.

use metor_proto_wkt::{SequenceAccepts, SequenceChannelState, SequenceCommandKind};

/// Where a channel is, as [`apply`] sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Running,
    Stopped,
    Ended,
    /// A reset or load waits for the system's ops in flight.
    Quiescing,
}

impl Phase {
    /// The phase of a channel reporting `state`.
    pub fn of(state: SequenceChannelState) -> Self {
        match state {
            SequenceChannelState::Running => Self::Running,
            SequenceChannelState::Stopped => Self::Stopped,
            SequenceChannelState::Quiescing => Self::Quiescing,
            SequenceChannelState::Completed
            | SequenceChannelState::Aborted
            | SequenceChannelState::Failed
            | SequenceChannelState::Panicked
            | SequenceChannelState::Unloaded => Self::Ended,
        }
    }
}

/// The parts of a channel's state that [`apply`] checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct State {
    pub phase: Phase,
    /// Whether the system has an `abort` input.
    pub abortable: bool,
    /// Whether the system exists and its step loads systems.
    pub loadable: bool,
}

/// A channel command, without its payload.
///
/// It is the Kani proof's input: the proof generates it where it could not
/// cheaply generate a `Load`'s strings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Load,
    Start,
    Abort,
    Stop,
    Reset,
}

impl Command {
    pub const ALL: [Self; 5] = [
        Self::Load,
        Self::Start,
        Self::Abort,
        Self::Stop,
        Self::Reset,
    ];

    pub fn of(kind: &SequenceCommandKind) -> Self {
        match kind {
            SequenceCommandKind::Load { .. } => Self::Load,
            SequenceCommandKind::Start => Self::Start,
            SequenceCommandKind::Abort => Self::Abort,
            SequenceCommandKind::Stop => Self::Stop,
            SequenceCommandKind::Reset => Self::Reset,
        }
    }

    /// The command's [`SequenceAccepts`] bit.
    pub fn bit(self) -> u32 {
        match self {
            Self::Load => SequenceAccepts::LOAD,
            Self::Start => SequenceAccepts::START,
            Self::Abort => SequenceAccepts::ABORT,
            Self::Stop => SequenceAccepts::STOP,
            Self::Reset => SequenceAccepts::RESET,
        }
    }
}

/// The reason a channel refused a command.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("the channel is running")]
    Running,
    #[error("the channel is not running")]
    NotRunning,
    #[error("the channel has ended; reset or load it first")]
    Ended,
    #[error("the system takes no abort")]
    NotAbortable,
    #[error("the system is quiescing")]
    Quiescing,
    #[error("nothing can be loaded; a system that is gone needs a reset first")]
    NotLoadable,
}

/// Checks whether a channel in `state` accepts `command`.
///
/// # Errors
/// Returns the reason the channel refuses the command.
pub fn apply(state: State, command: Command) -> Result<(), Refusal> {
    match (state.phase, command) {
        (Phase::Quiescing, Command::Start | Command::Load | Command::Abort) => {
            Err(Refusal::Quiescing)
        }
        (Phase::Ended, Command::Start | Command::Stop | Command::Abort) => Err(Refusal::Ended),
        (Phase::Running, Command::Start | Command::Reset | Command::Load) => Err(Refusal::Running),
        (Phase::Stopped, Command::Stop | Command::Abort) => Err(Refusal::NotRunning),
        (Phase::Running, Command::Abort) if !state.abortable => Err(Refusal::NotAbortable),
        (_, Command::Load) if !state.loadable => Err(Refusal::NotLoadable),
        _ => Ok(()),
    }
}

/// Returns the commands a channel in `state` accepts.
pub fn accepts(state: State) -> SequenceAccepts {
    let bits = Command::ALL
        .into_iter()
        .filter(|&command| apply(state, command).is_ok())
        .fold(0, |bits, command| bits | command.bit());
    SequenceAccepts(bits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(phase: Phase) -> State {
        State {
            phase,
            abortable: true,
            loadable: true,
        }
    }

    #[test]
    fn test_quiescing_takes_only_stop_and_reset() {
        let quiescing = state(Phase::Quiescing);
        assert_eq!(accepted(quiescing), [Command::Stop, Command::Reset]);
        assert_eq!(apply(quiescing, Command::Start), Err(Refusal::Quiescing));
    }

    fn accepted(state: State) -> Vec<Command> {
        Command::ALL
            .into_iter()
            .filter(|&command| apply(state, command).is_ok())
            .collect()
    }

    #[test]
    fn test_running_accepts_stop_and_abort() {
        let running = state(Phase::Running);
        assert_eq!(accepted(running), [Command::Abort, Command::Stop]);
        assert_eq!(apply(running, Command::Start), Err(Refusal::Running));
        assert_eq!(apply(running, Command::Reset), Err(Refusal::Running));
        assert_eq!(apply(running, Command::Load), Err(Refusal::Running));
    }

    #[test]
    fn test_stopped_accepts_start_reset_load() {
        let stopped = state(Phase::Stopped);
        assert_eq!(
            accepted(stopped),
            [Command::Load, Command::Start, Command::Reset]
        );
        assert_eq!(apply(stopped, Command::Stop), Err(Refusal::NotRunning));
        assert_eq!(apply(stopped, Command::Abort), Err(Refusal::NotRunning));
    }

    #[test]
    fn test_ended_accepts_reset_load() {
        let ended = state(Phase::Ended);
        assert_eq!(accepted(ended), [Command::Load, Command::Reset]);
        assert_eq!(apply(ended, Command::Start), Err(Refusal::Ended));
        assert_eq!(apply(ended, Command::Abort), Err(Refusal::Ended));
    }

    #[test]
    fn test_abort_needs_port() {
        let running = State {
            abortable: false,
            ..state(Phase::Running)
        };
        assert_eq!(apply(running, Command::Abort), Err(Refusal::NotAbortable));
        assert_eq!(apply(running, Command::Stop), Ok(()));
    }

    #[test]
    fn test_load_needs_a_loading_step() {
        let stopped = State {
            loadable: false,
            ..state(Phase::Stopped)
        };
        assert_eq!(accepted(stopped), [Command::Start, Command::Reset]);
        assert_eq!(apply(stopped, Command::Load), Err(Refusal::NotLoadable));
        let ended = State {
            loadable: false,
            ..state(Phase::Ended)
        };
        assert_eq!(accepted(ended), [Command::Reset]);
    }

    #[test]
    fn test_phase_of_channel_state() {
        assert_eq!(Phase::of(SequenceChannelState::Running), Phase::Running);
        assert_eq!(Phase::of(SequenceChannelState::Stopped), Phase::Stopped);
        assert_eq!(Phase::of(SequenceChannelState::Quiescing), Phase::Quiescing);
        assert_eq!(Phase::of(SequenceChannelState::Unloaded), Phase::Ended);
    }

    /// A channel publishes `accepts` as is, so its bits are exactly the
    /// commands `apply` lets through, in every state.
    #[test]
    fn test_accepts_matches_apply() {
        for phase in [
            Phase::Running,
            Phase::Stopped,
            Phase::Ended,
            Phase::Quiescing,
        ] {
            for (abortable, loadable) in
                [(true, true), (true, false), (false, true), (false, false)]
            {
                let state = State {
                    phase,
                    abortable,
                    loadable,
                };
                let bits = accepts(state);
                for command in Command::ALL {
                    assert_eq!(
                        bits.0 & command.bit() != 0,
                        apply(state, command).is_ok(),
                        "{state:?} {command:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn test_command_bits_match_wire_kinds() {
        let kinds = [
            SequenceCommandKind::Load { name: "a".into() },
            SequenceCommandKind::Start,
            SequenceCommandKind::Abort,
            SequenceCommandKind::Stop,
            SequenceCommandKind::Reset,
        ];
        for kind in kinds {
            assert_eq!(Command::of(&kind).bit(), SequenceAccepts::bit(&kind));
        }
    }

    #[test]
    fn test_command_of_kind() {
        let load = SequenceCommandKind::Load { name: "a".into() };
        assert_eq!(Command::of(&load), Command::Load);
        assert_eq!(Command::of(&SequenceCommandKind::Reset), Command::Reset);
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;

    fn any_command() -> Command {
        match kani::any::<u8>() % 5 {
            0 => Command::Load,
            1 => Command::Start,
            2 => Command::Abort,
            3 => Command::Stop,
            _ => Command::Reset,
        }
    }

    fn any_state() -> State {
        let phase = match kani::any::<u8>() % 4 {
            0 => Phase::Running,
            1 => Phase::Stopped,
            2 => Phase::Ended,
            _ => Phase::Quiescing,
        };
        State {
            phase,
            abortable: kani::any(),
            loadable: kani::any(),
        }
    }

    /// A live channel accepts only `Stop` and `Abort`, and a channel that is
    /// not live never accepts them. So `Reset` and `Load` never apply to a
    /// running system. A quiescing channel accepts only `Stop` and `Reset`,
    /// and `Load` is accepted only where the step loads. The published
    /// `accepts` bits are the commands accepted here.
    #[kani::proof]
    fn test_live_channel_transitions() {
        let state = any_state();
        let command = any_command();
        let live = state.phase == Phase::Running;
        let accepted = apply(state, command).is_ok();
        assert_eq!(accepts(state).0 & command.bit() != 0, accepted);
        if command == Command::Load && accepted {
            assert!(state.loadable);
        }
        if accepted {
            if state.phase == Phase::Quiescing {
                assert!(matches!(command, Command::Stop | Command::Reset));
                return;
            }
            let leaves = matches!(command, Command::Stop | Command::Abort);
            assert_eq!(leaves, live);
        }
    }
}
