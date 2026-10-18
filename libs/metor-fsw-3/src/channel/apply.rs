//! The rules for which commands a channel accepts in each state.

use metor_proto_wkt::SequenceCommandKind;

/// Where a channel is, as [`apply`] sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Running,
    Stopped,
    Ended,
    /// A reset or load waits for the system's ops in flight.
    Quiescing,
}

/// The parts of a channel's state that [`apply`] checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct State {
    pub phase: Phase,
    /// Whether the system has an `abort` input.
    pub abortable: bool,
}

/// A channel command, without its payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Load,
    Start,
    Abort,
    Stop,
    Reset,
}

impl Command {
    pub fn of(kind: &SequenceCommandKind) -> Self {
        match kind {
            SequenceCommandKind::Load { .. } => Self::Load,
            SequenceCommandKind::Start => Self::Start,
            SequenceCommandKind::Abort => Self::Abort,
            SequenceCommandKind::Stop => Self::Stop,
            SequenceCommandKind::Reset => Self::Reset,
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
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Command; 5] = [
        Command::Load,
        Command::Start,
        Command::Abort,
        Command::Stop,
        Command::Reset,
    ];

    fn state(phase: Phase) -> State {
        State {
            phase,
            abortable: true,
        }
    }

    #[test]
    fn test_quiescing_takes_only_stop_and_reset() {
        let quiescing = state(Phase::Quiescing);
        assert_eq!(accepted(quiescing), [Command::Stop, Command::Reset]);
        assert_eq!(apply(quiescing, Command::Start), Err(Refusal::Quiescing));
    }

    fn accepted(state: State) -> Vec<Command> {
        ALL.into_iter()
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
        }
    }

    /// A live channel accepts only `Stop` and `Abort`, and a channel that is
    /// not live never accepts them. So `Reset` and `Load` never apply to a
    /// running system. A quiescing channel accepts only `Stop` and `Reset`.
    #[kani::proof]
    fn test_live_channel_transitions() {
        let state = any_state();
        let command = any_command();
        let live = state.phase == Phase::Running;
        if apply(state, command).is_ok() {
            if state.phase == Phase::Quiescing {
                assert!(matches!(command, Command::Stop | Command::Reset));
                return;
            }
            let leaves = matches!(command, Command::Stop | Command::Abort);
            assert_eq!(leaves, live);
        }
    }
}
