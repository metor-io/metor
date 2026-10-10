//! `UringOs`: ops on its own io_uring instance.
//!
//! Reads, writes, sends, receives, accepts, and fsyncs go to the kernel.
//! Tty and GPIO ioctls and opens run inline in `push`, since io_uring has no
//! generic ioctl op, and their results come back from the next `reap`.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use io_uring::squeue::Entry;
use io_uring::types::{CancelBuilder, Fd};
use io_uring::{IoUring, opcode};

use super::{Class, OpId, Os, OsDone, OsOp, Out, sys};
use crate::io::{Errno, IoError, Op};

/// Set in the `user_data` of a cancel, whose completion is dropped.
const CANCEL: u64 = 1 << 40;
/// Set in the `user_data` of an accept, whose result is a descriptor.
const ACCEPT: u64 = 1 << 41;

/// What the kernel reads for a `SendTo` after `push` returns.
struct Msg {
    addr: libc::sockaddr_storage,
    iov: libc::iovec,
    hdr: libc::msghdr,
}

/// The `Os` for Linux targets.
pub struct UringOs {
    ring: IoUring,
    /// One per op table entry, for `SendTo`.
    msgs: Box<[Msg]>,
    done: Vec<OsDone>,
}

impl UringOs {
    /// A backend for at most `ops` ops in flight.
    pub fn new(ops: usize) -> std::io::Result<Self> {
        let ops = ops.max(1);
        let entries = u32::try_from(2 * ops)
            .unwrap_or(u32::MAX)
            .next_power_of_two();
        // SAFETY: all-zero is a valid `Msg`; `push` fills it before use.
        let zeroed = || unsafe { core::mem::zeroed::<Msg>() };
        Ok(Self {
            ring: IoUring::new(entries)?,
            msgs: (0..ops).map(|_| zeroed()).collect(),
            done: Vec::with_capacity(2 * ops),
        })
    }

    fn entry(&mut self, op: &OsOp) -> Option<Entry> {
        let fd = Fd(op.fd);
        let buf = op.bytes();
        let (ptr, len) = (buf.cast::<u8>().as_ptr(), buf.len() as u32);
        let at = |offset: Option<u64>| offset.unwrap_or(u64::MAX);
        let mut user_data = u64::from(op.id.get());
        let entry = match op.op {
            Op::Read { offset } => opcode::Read::new(fd, ptr, len).offset(at(offset)).build(),
            Op::Write { offset } => opcode::Write::new(fd, ptr, len).offset(at(offset)).build(),
            Op::Recv => opcode::Recv::new(fd, ptr, len).build(),
            Op::Send => opcode::Send::new(fd, ptr, len)
                .flags(sys::SEND_FLAGS)
                .build(),
            Op::SendTo(addr) => {
                let msg = self.msgs.get_mut(usize::from(op.id.index()))?;
                let (ss, sl) = sys::sockaddr(addr);
                msg.addr = ss;
                msg.iov = libc::iovec {
                    iov_base: ptr.cast(),
                    iov_len: len as usize,
                };
                msg.hdr.msg_name = (&raw mut msg.addr).cast();
                msg.hdr.msg_namelen = sl;
                msg.hdr.msg_iov = &raw mut msg.iov;
                msg.hdr.msg_iovlen = 1;
                opcode::SendMsg::new(fd, &raw const msg.hdr)
                    .flags(sys::SEND_FLAGS as u32)
                    .build()
            }
            Op::Accept => {
                user_data |= ACCEPT;
                let flags = libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC;
                let (addr, len) = (core::ptr::null_mut(), core::ptr::null_mut());
                opcode::Accept::new(fd, addr, len).flags(flags).build()
            }
            Op::Fsync => opcode::Fsync::new(fd).build(),
            _ => return None,
        };
        Some(entry.user_data(user_data))
    }

    /// Queues `entry`, submitting first if the queue is full. False if the
    /// kernel still has no room.
    fn queue(&mut self, entry: &Entry) -> bool {
        // SAFETY: every pointer in `entry` stays valid until its completion:
        // arenas never move, and `msgs` lives as long as the ring.
        let pushed = |ring: &mut IoUring| unsafe { ring.submission().push(entry).is_ok() };
        pushed(&mut self.ring) || (self.ring.submit().is_ok() && pushed(&mut self.ring))
    }
}

impl Os for UringOs {
    fn push(&mut self, op: OsOp) {
        let Some(entry) = self.entry(&op) else {
            let result = sys::run(&op);
            self.done.push(OsDone { id: op.id, result });
            return;
        };
        if !self.queue(&entry) {
            self.done.push(OsDone {
                id: op.id,
                result: Err(IoError::QueueFull),
            });
        }
    }

    fn cancel(&mut self, id: OpId) {
        let target = u64::from(id.get());
        let entry = opcode::AsyncCancel::new(target)
            .build()
            .user_data(CANCEL | target);
        // A cancel the ring has no room for is dropped; the op still lands.
        self.queue(&entry);
    }

    fn submit(&mut self) {
        let _ = self.ring.submit();
    }

    fn reap(&mut self, done: &mut Vec<OsDone>) {
        done.append(&mut self.done);
        for cqe in self.ring.completion() {
            let data = cqe.user_data();
            if data & CANCEL != 0 {
                continue;
            }
            let res = cqe.result();
            let result = match res {
                ..0 if res == -libc::ECANCELED => Err(IoError::Canceled),
                ..0 => Err(IoError::Os(Errno::new(-res as u16))),
                // SAFETY: an accept's result is a new descriptor, now owned here.
                _ if data & ACCEPT != 0 => Ok(Out::Opened(
                    unsafe { OwnedFd::from_raw_fd(res) },
                    Class::Stream,
                )),
                _ => Ok(Out::Len(res as u32)),
            };
            done.push(OsDone {
                id: OpId::from_raw(data as u32),
                result,
            });
        }
    }

    fn close(&mut self, fd: OwnedFd) {
        let every = CancelBuilder::fd(Fd(fd.as_raw_fd())).all();
        let entry = opcode::AsyncCancel2::new(every).build().user_data(CANCEL);
        self.queue(&entry);
        let _ = self.ring.submit();
    }
}
