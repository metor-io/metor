//! Control-block protocol tests. The protocol is address-space agnostic
//! (every shared word lives in the mapped file), so two threads over one
//! file exercise exactly the code paths two processes would; the real
//! two-process runs live in the `proc_integration` test.

use std::sync::mpsc;
use std::time::Duration;

use metor_proto::types::Timestamp;

use super::ctl::{CtlError, CtlHost, CtlWorker, StepOutcome, WorkerCmd, WorkerState};
use crate::abi::FswStatus;

const GENEROUS: Duration = Duration::from_secs(10);

fn ctl_pair(dir: &tempfile::TempDir) -> (CtlHost, CtlWorker) {
    let path = dir.path().join("test.ctl");
    let host = CtlHost::create(&path).unwrap();
    let worker = CtlWorker::attach(&path).unwrap();
    (host, worker)
}

/// The full lifecycle: attach → init → N lockstep steps (each delivering a
/// monotonically increasing sequence and the exact timestamp written) →
/// shutdown while the worker is parked in `next`.
#[test]
fn lifecycle_and_lockstep_steps() {
    const STEPS: u32 = 100;
    let dir = tempfile::tempdir().unwrap();
    let (host, worker) = ctl_pair(&dir);
    let (seen_tx, seen_rx) = mpsc::channel::<(u32, Timestamp)>();

    std::thread::scope(|s| {
        s.spawn(move || {
            worker.report(WorkerState::Attached);
            assert!(worker.wait_init(), "init requested before shutdown");
            worker.report(WorkerState::Ready);
            let mut last = 0;
            loop {
                match worker.next(last) {
                    WorkerCmd::Step { seq, now } => {
                        seen_tx.send((seq, now)).unwrap();
                        worker.done(seq, FswStatus::Running);
                        last = seq;
                    }
                    WorkerCmd::Shutdown => break,
                }
            }
            worker.report(WorkerState::Done);
        });

        host.wait_state(WorkerState::Attached, GENEROUS).unwrap();
        host.request(WorkerState::InitReq);
        host.wait_state(WorkerState::Ready, GENEROUS).unwrap();
        for k in 1..=STEPS {
            let now = Timestamp(1000 + k as i64);
            assert_eq!(host.step(now, GENEROUS), StepOutcome::Acked(FswStatus::Running));
            assert_eq!(seen_rx.recv().unwrap(), (k, now), "step {k} delivered verbatim");
        }
        host.request(WorkerState::ShutdownReq);
        host.wait_state(WorkerState::Done, GENEROUS).unwrap();
    });
}

/// A worker that slept through doorbells serves only the newest one: three
/// timed-out steps are skipped, the late worker acks the freshest sequence,
/// and the host's following step converges normally.
#[test]
fn slow_worker_skips_to_newest() {
    let dir = tempfile::tempdir().unwrap();
    let (host, worker) = ctl_pair(&dir);
    let (gate_tx, gate_rx) = mpsc::channel::<()>();
    let (seen_tx, seen_rx) = mpsc::channel::<u32>();

    std::thread::scope(|s| {
        s.spawn(move || {
            let mut last = 0;
            loop {
                // Blocked "computing" until the test releases it.
                if gate_rx.recv().is_err() {
                    break;
                }
                match worker.next(last) {
                    WorkerCmd::Step { seq, .. } => {
                        seen_tx.send(seq).unwrap();
                        worker.done(seq, FswStatus::Running);
                        last = seq;
                    }
                    WorkerCmd::Shutdown => break,
                }
            }
        });

        // Three doorbells nobody answers.
        for k in 1..=3 {
            assert_eq!(
                host.step(Timestamp(k), Duration::from_millis(5)),
                StepOutcome::TimedOut
            );
        }
        // Release the worker: it must serve the newest sequence (3), not
        // replay 1 and 2.
        gate_tx.send(()).unwrap();
        assert_eq!(seen_rx.recv().unwrap(), 3);
        // The next step converges despite the abandoned timeouts before it.
        gate_tx.send(()).unwrap();
        assert_eq!(
            host.step(Timestamp(4), GENEROUS),
            StepOutcome::Acked(FswStatus::Running)
        );
        assert_eq!(seen_rx.recv().unwrap(), 4);
        drop(gate_tx); // parks → recv err → worker exits
        host.request(WorkerState::ShutdownReq); // unblock a worker parked in next()
    });
}

/// The status word crosses per step: a panicked execute surfaces as
/// `Acked(Panicked)` on that exact step.
#[test]
fn step_status_propagates() {
    let dir = tempfile::tempdir().unwrap();
    let (host, worker) = ctl_pair(&dir);
    std::thread::scope(|s| {
        s.spawn(move || {
            if let WorkerCmd::Step { seq, .. } = worker.next(0) {
                worker.done(seq, FswStatus::Panicked);
            }
        });
        assert_eq!(
            host.step(Timestamp(1), GENEROUS),
            StepOutcome::Acked(FswStatus::Panicked)
        );
    });
}

/// A worker-side failure latches `Failed` plus its code, and every host wait
/// surfaces it instead of timing out.
#[test]
fn failure_surfaces_with_code() {
    let dir = tempfile::tempdir().unwrap();
    let (host, worker) = ctl_pair(&dir);
    worker.fail(7);
    match host.wait_state(WorkerState::Attached, GENEROUS) {
        Err(CtlError::WorkerFailed(7)) => {}
        other => panic!("expected WorkerFailed(7), got {other:?}"),
    }
}

/// A host wait on a silent worker reports which state it last saw.
#[test]
fn state_wait_times_out() {
    let dir = tempfile::tempdir().unwrap();
    let (host, _worker) = ctl_pair(&dir);
    match host.wait_state(WorkerState::Attached, Duration::from_millis(10)) {
        Err(CtlError::StateTimeout { want, seen }) => {
            assert_eq!(want, WorkerState::Attached);
            assert_eq!(seen, Some(WorkerState::Booting));
        }
        other => panic!("expected StateTimeout, got {other:?}"),
    }
}

/// Attach validates the header: junk, a truncated file, and a wrong-magic
/// block are all clean errors.
#[test]
fn attach_rejects_bad_blocks() {
    let dir = tempfile::tempdir().unwrap();

    let junk = dir.path().join("junk.ctl");
    std::fs::write(&junk, vec![0xAB; 64]).unwrap();
    assert!(matches!(
        CtlWorker::attach(&junk),
        Err(CtlError::BadHeader("wrong magic"))
    ));

    let short = dir.path().join("short.ctl");
    std::fs::write(&short, vec![0u8; 16]).unwrap();
    assert!(matches!(
        CtlWorker::attach(&short),
        Err(CtlError::BadHeader(_))
    ));

    let missing = dir.path().join("missing.ctl");
    assert!(matches!(CtlWorker::attach(&missing), Err(CtlError::Io(_))));
}
