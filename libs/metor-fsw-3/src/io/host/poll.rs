//! `PollOs`: ops as nonblocking syscalls, with `polling` for readiness.
//!
//! An op runs when it is pushed. One whose descriptor is not ready parks,
//! and runs again once the descriptor polls ready for the union of its
//! parked ops' interests. File, directory, and GPIO ops never park. A dial
//! starts its connect and parks on its new socket until it takes writes.

use core::num::NonZeroUsize;
use core::time::Duration;
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd, RawFd};

use polling::{Event, Events, Poller};

use super::{Class, OpId, Os, OsDone, OsOp, Out, sys};
use crate::io::{Errno, IoError};

/// The `Os` for Unix development hosts.
pub struct PollOs {
    poller: Poller,
    events: Events,
    parked: Vec<OsOp>,
    /// The socket of each dial in flight, by op.
    dialing: Vec<(OpId, OwnedFd)>,
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
            dialing: Vec::with_capacity(cap.get()),
            added: Vec::with_capacity(cap.get()),
            fired: Vec::with_capacity(cap.get()),
            done: Vec::with_capacity(cap.get()),
        })
    }

    fn attempt(&mut self, op: OsOp) {
        let result = match op.class {
            Class::Dialer => self.dialed(&op),
            _ => sys::run(&op),
        };
        match result {
            Err(IoError::Os(Errno::EAGAIN)) if op.class.polled() => {
                let fd = op.fd;
                self.parked.push(op);
                self.arm(fd);
            }
            result => self.done.push(OsDone { id: op.id, result }),
        }
    }

    /// Starts a dial's connect, then parks it on its new socket.
    fn dial(&mut self, mut op: OsOp) {
        let socket = op.dial.ok_or(IoError::Unsupported).and_then(sys::connect);
        let socket = match socket {
            Ok(socket) => socket,
            Err(e) => {
                return self.done.push(OsDone {
                    id: op.id,
                    result: Err(e),
                });
            }
        };
        op.fd = socket.as_raw_fd();
        self.dialing.push((op.id, socket));
        self.parked.push(op);
        self.arm(op.fd);
    }

    /// A dial's stream once its connect lands. Its socket closes on an
    /// error.
    fn dialed(&mut self, op: &OsOp) -> Result<Out, IoError> {
        let result = sys::connected(op.fd);
        self.prune(op.fd);
        let socket = self.end_dial(op.id).ok_or(IoError::Os(Errno::EBADF))?;
        result.map(|()| Out::Opened {
            fd: socket,
            class: Class::Stream,
            peer: op.dial,
        })
    }

    /// Takes op `id`'s dial socket, if it has one.
    fn end_dial(&mut self, id: OpId) -> Option<OwnedFd> {
        let position = self.dialing.iter().position(|(d, _)| *d == id)?;
        Some(self.dialing.swap_remove(position).1)
    }

    /// Polls `fd` once for every interest its parked ops have.
    fn arm(&mut self, fd: RawFd) {
        let parked = self.parked.iter().filter(|p| p.fd == fd);
        let (reads, writes) = parked.fold((false, false), |(r, w), p| match p.waits_to_write() {
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
        let Some(position) = self.added.iter().position(|&a| a == fd).filter(|_| !parked) else {
            return;
        };
        self.added.swap_remove(position);
        // SAFETY: as in `arm`.
        let _ = self.poller.delete(unsafe { BorrowedFd::borrow_raw(fd) });
    }

    /// Completes every parked op `hit` picks with `e`, closing a dial's
    /// socket once the poller no longer holds it.
    fn finish_where(&mut self, hit: impl Fn(&OsOp) -> bool, e: IoError) {
        while let Some(position) = self.parked.iter().position(&hit) {
            let op = self.parked.remove(position);
            if op.class == Class::Dialer {
                self.prune(op.fd);
                drop(self.end_dial(op.id));
            }
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
            if let Some(position) = self.parked.iter().position(|p| p.fd == fd) {
                let op = self.parked.remove(position);
                self.attempt(op);
            }
        }
        self.prune(fd);
    }
}

impl Os for PollOs {
    fn push(&mut self, op: OsOp) {
        match op.class {
            Class::Dialer => self.dial(op),
            _ => self.attempt(op),
        }
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
        for index in 0..self.fired.len() {
            self.retry(self.fired[index]);
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
