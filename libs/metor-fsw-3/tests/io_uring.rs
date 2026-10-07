//! `UringOs` against real descriptors.
#![cfg(target_os = "linux")]

mod os_cases;

use std::net::TcpListener;
use std::os::fd::OwnedFd;

use metor_fsw_3::io::*;
use os_cases::{Rig, SRC, err, ok};

fn uring(limits: &Limits, tables: Tables, dials: &[RawAddr]) -> Result<Box<dyn Os>, OsError> {
    Ok(Box::new(UringOs::new(limits, tables, dials)?))
}

fn pipe() -> (OwnedFd, OwnedFd) {
    let (r, w) = std::io::pipe().unwrap();
    (r.into(), w.into())
}

#[test]
fn uring_os_cases() {
    os_cases::run_all(uring);
}

#[test]
fn async_cancel_of_idle_pipe_read() {
    let (r, _w) = pipe();
    let mut rig = Rig::new(uring, &[], vec![(r, FdClass::File)], &[b""]);
    let h = rig.handles[0];
    let rx = rig.start(h, Op::read(BufRef::new(0), None));
    assert!(rig.poll(rx).is_none());
    let cancel = rig.start(h, Op::cancel(rx));
    assert_eq!(err(rig.wait(rx)), IoError::Canceled);
    assert_eq!(ok(rig.wait(cancel)), Out::UNIT);
    assert_eq!(rig.reactor.in_flight(SRC), 0);
}

#[test]
fn full_fixed_file_table_is_queue_full() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let limits = Limits { ops: 1, sources: 1 };
    let mut rig = Rig::with_limits(uring, limits, &[addr], vec![], &[]);
    let dial = RawHandle::new(0, 0);
    let first = ok(rig.run(dial, Op::connect(addr.into())))
        .as_handle()
        .unwrap();
    assert_eq!(
        err(rig.run(dial, Op::connect(addr.into()))),
        IoError::QueueFull
    );
    ok(rig.run(first, Op::CLOSE));
    let again = ok(rig.run(dial, Op::connect(addr.into())))
        .as_handle()
        .unwrap();
    assert_ne!(again, first);
}
