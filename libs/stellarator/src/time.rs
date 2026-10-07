//! Executor clocks.

use std::cell::Cell;
use std::time::Duration;

/// The clock behind an executor's timer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Clock {
    /// The host's monotonic clock.
    Os,
    /// This thread's simulated time. Only [`sim_set`] advances it.
    Sim,
}

thread_local! {
    static SIM_NOW: Cell<u64> = const { Cell::new(0) };
}

/// Returns this thread's simulated time in microseconds.
pub fn sim_now() -> u64 {
    SIM_NOW.with(Cell::get)
}

/// Sets this thread's simulated time in microseconds.
pub fn sim_set(micros: u64) {
    SIM_NOW.with(|t| t.set(micros));
}

impl Clock {
    pub(crate) fn maitake(self) -> maitake::time::Clock {
        match self {
            Self::Os => crate::os::os_clock(),
            Self::Sim => maitake::time::Clock::new(Duration::from_micros(1), sim_now).named("sim"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;
    use std::time::Duration;

    use super::{Clock, sim_now, sim_set};
    use crate::{Executor, Turned};

    fn sim() -> Rc<Executor> {
        Rc::new(Executor::new(Clock::Sim).unwrap())
    }

    fn flag() -> Rc<Cell<bool>> {
        Rc::new(Cell::new(false))
    }

    struct SetOnDrop(Rc<Cell<bool>>);

    impl Drop for SetOnDrop {
        fn drop(&mut self) {
            self.0.set(true);
        }
    }

    #[test]
    fn sim_sleep_ends_when_the_clock_moves() {
        sim_set(1_000);
        let exec = sim();
        let done = flag();
        let d = done.clone();
        let task = exec.spawn(async move {
            crate::sleep(Duration::from_millis(10)).await;
            d.set(true);
        });
        exec.turn();
        sim_set(1_000 + 9_999);
        exec.turn();
        assert!(!done.get());
        sim_set(1_000 + 10_000);
        exec.turn();
        exec.turn();
        assert!(done.get());
        drop(task);
    }

    #[test]
    fn alternating_executors_keep_their_tasks() {
        let (a, b) = (sim(), sim());
        let (in_a, in_b) = (flag(), flag());
        let inner = in_a.clone();
        let ta = a.spawn(async move {
            crate::yield_now().await;
            crate::spawn(async move { inner.set(true) }).await.unwrap();
        });
        let inner = in_b.clone();
        let tb = b.spawn(async move { inner.set(true) });
        for _ in 0..4 {
            b.turn();
            assert!(!in_a.get());
        }
        assert!(in_b.get());
        for _ in 0..4 {
            a.turn();
        }
        assert!(in_a.get());
        drop((ta, tb));
    }

    #[test]
    fn empty_turn_is_idle() {
        let exec = sim();
        let t = exec.turn();
        assert_eq!(
            t,
            Turned {
                runnable: false,
                next_deadline: None
            }
        );
    }

    #[test]
    fn turn_reports_deadline_and_leaves_sim_time() {
        sim_set(500);
        let exec = sim();
        let task = exec.spawn(async { crate::sleep(Duration::from_millis(3)).await });
        let t = exec.turn();
        assert_eq!(sim_now(), 500);
        // maitake reports the wheel slot's deadline, which can precede the sleep's.
        let d = t.next_deadline.unwrap();
        assert!(d > Duration::from_millis(2) && d <= Duration::from_millis(3));
        drop(task);
    }

    #[test]
    fn turned_inside_run_spawns_onto_itself() {
        let landed = crate::run(|| async {
            let inner = sim();
            let hit = flag();
            let h = hit.clone();
            let task = inner.spawn(async move {
                crate::spawn(async move { h.set(true) }).await.unwrap();
            });
            for _ in 0..4 {
                inner.turn();
            }
            drop(task);
            hit.get()
        });
        assert!(landed);
    }

    #[test]
    fn run_after_run() {
        assert_eq!(crate::run(|| async { 1 }), 1);
        assert_eq!(crate::run(|| async { 2 }), 2);
    }

    #[test]
    fn os_clock_turn_does_not_block() {
        let exec = Rc::new(Executor::new(Clock::Os).unwrap());
        let task = exec.spawn(async { crate::sleep(Duration::from_secs(60)).await });
        let start = std::time::Instant::now();
        let t = exec.turn();
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(t.next_deadline.is_some_and(|d| d > Duration::from_secs(50)));
        drop(task);
    }

    #[test]
    fn drop_cancels_sleeping_task() {
        let exec = sim();
        let dropped = flag();
        let guard = SetOnDrop(dropped.clone());
        let task = exec.spawn(async move {
            let _guard = guard;
            crate::sleep(Duration::from_secs(5)).await;
        });
        exec.turn();
        drop(task);
        assert!(!dropped.get());
        drop(exec);
        assert!(dropped.get());
    }
}
