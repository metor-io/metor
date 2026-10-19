//! A turned group: async systems whose executor the coordinator turns once a
//! cycle on its own thread, for a deterministic run.

use std::rc::Rc;

use metor_proto::types::Timestamp;
use stellarator::{Executor, JoinHandle};

use crate::async_system::{Stop, StopHandle};
use crate::coordinator::MakeError;

use super::group::Member;

/// How many turns a dropped group gives its tasks to finish.
const DROP_TURNS: usize = 64;

/// Turns a stellarator executor on the coordinator thread, under simulated
/// time the coordinator sets.
pub(crate) struct TurnedGroup {
    stop: StopHandle,
    stop_rx: Stop,
    tasks: Vec<JoinHandle<()>>,
    executor: Rc<Executor>,
    /// The id of the group's first member, before whose step it turns.
    first: String,
}

impl TurnedGroup {
    /// Starts the group for `thread`, with `first` as its first member's id.
    pub(crate) fn new(thread: &str, first: &str) -> Result<Self, MakeError> {
        let executor =
            Executor::new(stellarator::time::Clock::Sim).map_err(|_| MakeError::ThreadStart {
                thread: thread.to_string(),
            })?;
        let (stop, stop_rx) = Stop::pair();
        Ok(Self {
            stop,
            stop_rx,
            tasks: Vec::new(),
            executor: Rc::new(executor),
            first: first.to_string(),
        })
    }

    /// The id of the member before whose step the group turns.
    pub(crate) fn first(&self) -> &str {
        &self.first
    }

    /// Builds `member` on this thread, inside the executor's scope, and
    /// spawns its task.
    pub(crate) fn add(&mut self, member: Member) -> Result<(), MakeError> {
        let panicked = member.panicked.clone();
        let stop = self.stop_rx.clone();
        let running = self.executor.enter(|| member.launch(stop))?;
        let handle = self.executor.spawn(Member::run(running, panicked));
        self.tasks.push(handle);
        Ok(())
    }

    /// Sets simulated time to `now` and turns the executor once.
    pub(crate) fn turn(&mut self, now: Timestamp) {
        stellarator::time::sim_set(u64::try_from(now.0).unwrap_or(0));
        self.executor.turn();
    }
}

impl Drop for TurnedGroup {
    /// Stops the members and turns until they finish, or `DROP_TURNS` pass.
    /// The executor's own drop cancels the rest.
    fn drop(&mut self) {
        self.stop.stop();
        for _ in 0..DROP_TURNS {
            if self.tasks.iter().all(|t| t.0.is_complete()) {
                break;
            }
            self.executor.turn();
        }
        self.tasks.clear();
    }
}
