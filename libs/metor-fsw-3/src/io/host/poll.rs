//! `PollOs`: ops as nonblocking syscalls, with `polling` for readiness.
//!
//! An op runs when it is pushed. One whose descriptor is not ready parks,
//! and runs again once the descriptor polls ready for the union of its
//! parked ops' interests. File, directory, and GPIO ops never park.

use core::num::NonZeroUsize;
use core::time::Duration;
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd, RawFd};

use polling::{Event, Events, Poller};

use super::{OpId, Os, OsDone, OsOp, sys};
use crate::io::{Errno, IoError, Op};

/// The `Os` for Unix development hosts.
pub struct PollOs {
    poller: Poller,
    events: Events,
    parked: Vec<OsOp>,
    /// Descriptors added to the poller: those with parked ops, so at most
    /// one per op in flight.
    added: Vec<RawFd>,
    fired: Vec<RawFd>,
    done: Vec<OsDone>,
}

impl PollOs {
    /// A backend for at most `ops` ops in flight.
    pub fn new(ops: usize) -> std::io::Result<Self> {
        let cap = NonZeroUsize::new(ops).unwrap_or(NonZeroUsize::MIN);
        Ok(Self {
            poller: Poller::new()?,
            events: Events::with_capacity(cap),
            parked: Vec::with_capacity(cap.get()),
            added: Vec::with_capacity(cap.get()),
            fired: Vec::with_capacity(cap.get()),
            done: Vec::with_capacity(cap.get()),
        })
    }

    fn attempt(&mut self, op: OsOp) {
        match sys::run(&op) {
            Err(IoError::Os(Errno::EAGAIN)) if op.class.polled() => {
                let fd = op.fd;
                self.parked.push(op);
                self.arm(fd);
            }
            result => self.done.push(OsDone { id: op.id, result }),
        }
    }

    /// Polls `fd` once for every interest its parked ops have.
    fn arm(&mut self, fd: RawFd) {
        let parked = self.parked.iter().filter(|p| p.fd == fd);
        let (reads, writes) = parked.fold((false, false), |(r, w), p| match writes(&p.op) {
            true => (r, true),
            false => (true, w),
        });
        let event = Event::new(fd as usize, reads, writes);
        let armed = match self.added.contains(&fd) {
            // SAFETY: `fd` is open: its owner closes it through `close`,
            // which deletes it first, or after this backend is dropped.
            true => self
                .poller
                .modify(unsafe { BorrowedFd::borrow_raw(fd) }, event),
            // SAFETY: as above.
            false => unsafe { self.poller.add(fd, event) }.inspect(|()| self.added.push(fd)),
        };
        if armed.is_err() {
            let e = sys::errno();
            self.finish_where(|p| p.fd == fd, e);
            self.prune(fd);
        }
    }

    /// Takes `fd` out of the poller once no op is parked on it.
    fn prune(&mut self, fd: RawFd) {
        let parked = self.parked.iter().any(|p| p.fd == fd);
        let Some(at) = self.added.iter().position(|&a| a == fd).filter(|_| !parked) else {
            return;
        };
        self.added.swap_remove(at);
        // SAFETY: as in `arm`.
        let _ = self.poller.delete(unsafe { BorrowedFd::borrow_raw(fd) });
    }

    /// Completes every parked op `hit` picks with `e`.
    fn finish_where(&mut self, hit: impl Fn(&OsOp) -> bool, e: IoError) {
        while let Some(at) = self.parked.iter().position(&hit) {
            let op = self.parked.remove(at);
            self.done.push(OsDone {
                id: op.id,
                result: Err(e),
            });
        }
    }

    /// Runs again each op parked on `fd`, in the order they parked.
    fn retry(&mut self, fd: RawFd) {
        let waiting = self.parked.iter().filter(|p| p.fd == fd).count();
        for _ in 0..waiting {
            if let Some(at) = self.parked.iter().position(|p| p.fd == fd) {
                let op = self.parked.remove(at);
                self.attempt(op);
            }
        }
        self.prune(fd);
    }
}

fn writes(op: &Op) -> bool {
    matches!(op, Op::Write { .. } | Op::Send | Op::SendTo(_))
}

impl Os for PollOs {
    fn push(&mut self, op: OsOp) {
        self.attempt(op);
    }

    fn cancel(&mut self, id: OpId) {
        let Some(fd) = self.parked.iter().find(|p| p.id == id).map(|p| p.fd) else {
            return;
        };
        self.finish_where(|p| p.id == id, IoError::Canceled);
        self.prune(fd);
    }

    fn submit(&mut self) {
        if self.parked.is_empty() {
            return;
        }
        self.events.clear();
        if self
            .poller
            .wait(&mut self.events, Some(Duration::ZERO))
            .is_err()
        {
            return;
        }
        self.fired
            .extend(self.events.iter().map(|e| e.key as RawFd));
        for at in 0..self.fired.len() {
            self.retry(self.fired[at]);
        }
        self.fired.clear();
    }

    fn reap(&mut self, done: &mut Vec<OsDone>) {
        done.append(&mut self.done);
    }

    fn close(&mut self, fd: OwnedFd) {
        let raw = fd.as_raw_fd();
        self.finish_where(|p| p.fd == raw, IoError::Canceled);
        self.prune(raw);
    }
}
