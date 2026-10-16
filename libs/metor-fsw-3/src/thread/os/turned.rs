//! A turned group: async systems whose executor the coordinator turns once a
//! cycle on its own thread, for a deterministic run.

use core::cell::{Cell, RefCell};
use std::rc::Rc;

use metor_proto::types::Timestamp;
use stellarator::{Executor, JoinHandle};

use crate::async_system::{StopHandle, stop_pair};
use crate::coordinator::ParamError;

use super::group::{Member, build, task};

/// How many turns a dropped group gives its tasks to finish.
const DROP_TURNS: usize = 64;

/// Turns a stellarator executor on the coordinator thread, under simulated
/// time the coordinator sets.
pub(crate) struct TurnedGroup {
    stop: StopHandle,
    stop_rx: crate::async_system::Stop,
    /// Declared before `executor`, so the handles drop first.
    tasks: RefCell<Vec<JoinHandle<()>>>,
    executor: Rc<Executor>,
    /// The cycle time this group was last turned at; later steps that cycle
    /// skip it.
    turned_at: Cell<Option<Timestamp>>,
}

impl TurnedGroup {
    pub(crate) fn new(thread: &str) -> Result<Rc<Self>, ParamError> {
        let executor =
            Executor::new(stellarator::time::Clock::Sim).map_err(|_| ParamError::ThreadStart {
                thread: thread.to_string(),
            })?;
        let (stop, stop_rx) = stop_pair();
        Ok(Rc::new(Self {
            stop,
            stop_rx,
            tasks: RefCell::default(),
            executor: Rc::new(executor),
            turned_at: Cell::new(None),
        }))
    }

    /// Builds `member` on this thread, inside the executor's scope, and
    /// spawns its task.
    pub(crate) fn add(&self, member: Member) -> Result<(), ParamError> {
        let Member { launch, panicked } = member;
        let stop = self.stop_rx.clone();
        let running = self.executor.enter(|| build(launch, stop))?;
        let handle = self.executor.spawn(task(running, panicked));
        self.tasks.borrow_mut().push(handle);
        Ok(())
    }

    /// Sets simulated time to `now` and turns the executor once. A second
    /// call in the same cycle does nothing.
    pub(crate) fn turn(&self, now: Timestamp) {
        if self.turned_at.replace(Some(now)) == Some(now) {
            return;
        }
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
            if self.tasks.borrow().iter().all(|t| t.0.is_complete()) {
                break;
            }
            self.executor.turn();
        }
        self.tasks.borrow_mut().clear();
    }
}
