//! Which commands a channel accepts, and in which state.

use metor_proto_wkt::SequenceCommandKind;

use crate::coordinator::RunState;

/// What [`apply`] reads of a channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct State {
    pub run: RunState,
    pub ended: bool,
    /// Whether the system has an `abort` input.
    pub abortable: bool,
}

/// A command with its payload set aside.
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

/// Why a channel refused a command.
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
}

/// Whether `state` accepts `command`, or why it refuses it.
pub fn apply(state: State, command: Command) -> Result<(), Refusal> {
    let live = state.run == RunState::Running && !state.ended;
    match command {
        Command::Start if state.ended => Err(Refusal::Ended),
        Command::Start if live => Err(Refusal::Running),
        Command::Stop | Command::Abort if state.ended => Err(Refusal::Ended),
        Command::Stop | Command::Abort if !live => Err(Refusal::NotRunning),
        Command::Abort if !state.abortable => Err(Refusal::NotAbortable),
        Command::Reset | Command::Load if live => Err(Refusal::Running),
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

    fn state(run: RunState, ended: bool) -> State {
        State {
            run,
            ended,
            abortable: true,
        }
    }

    fn accepted(state: State) -> Vec<Command> {
        ALL.into_iter()
            .filter(|&command| apply(state, command).is_ok())
            .collect()
    }

    #[test]
    fn test_running_accepts_stop_and_abort() {
        let running = state(RunState::Running, false);
        assert_eq!(accepted(running), [Command::Abort, Command::Stop]);
        assert_eq!(apply(running, Command::Start), Err(Refusal::Running));
        assert_eq!(apply(running, Command::Reset), Err(Refusal::Running));
        assert_eq!(apply(running, Command::Load), Err(Refusal::Running));
    }

    #[test]
    fn test_stopped_accepts_start_reset_load() {
        let stopped = state(RunState::Stopped, false);
        assert_eq!(
            accepted(stopped),
            [Command::Load, Command::Start, Command::Reset]
        );
        assert_eq!(apply(stopped, Command::Stop), Err(Refusal::NotRunning));
        assert_eq!(apply(stopped, Command::Abort), Err(Refusal::NotRunning));
    }

    #[test]
    fn test_ended_accepts_reset_load() {
        for run in [RunState::Running, RunState::Stopped] {
            let ended = state(run, true);
            assert_eq!(accepted(ended), [Command::Load, Command::Reset]);
            assert_eq!(apply(ended, Command::Start), Err(Refusal::Ended));
            assert_eq!(apply(ended, Command::Abort), Err(Refusal::Ended));
        }
    }

    #[test]
    fn test_abort_needs_port() {
        let running = State {
            abortable: false,
            ..state(RunState::Running, false)
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
        State {
            run: if kani::any() {
                RunState::Running
            } else {
                RunState::Stopped
            },
            ended: kani::any(),
            abortable: kani::any(),
        }
    }

    /// A live channel accepts only `Stop` and `Abort`, and only a live one
    /// accepts them, so `Reset` and `Load` never apply to a running system.
    #[kani::proof]
    fn test_live_channel_transitions() {
        let state = any_state();
        let command = any_command();
        let live = state.run == RunState::Running && !state.ended;
        if apply(state, command).is_ok() {
            let leaves = matches!(command, Command::Stop | Command::Abort);
            assert_eq!(leaves, live);
        }
    }
}
