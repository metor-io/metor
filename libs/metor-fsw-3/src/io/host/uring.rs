//! `UringOs`: ops on its own io_uring instance.
//!
//! Reads, writes, sends, receives, accepts, connects, and fsyncs go to the
//! kernel.
//! Tty and GPIO ioctls and opens run inline in `push`, since io_uring has no
//! generic ioctl op, and their results come back from the next `reap`.

use core::net::SocketAddr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use io_uring::squeue::Entry;
use io_uring::types::{CancelBuilder, Fd};
use io_uring::{IoUring, opcode};

use super::{Class, OpId, Os, OsDone, OsOp, Out, sys};
use crate::io::{CURRENT_POSITION, Errno, IoError, Op};

/// Set in the `user_data` of a cancel, whose completion is dropped.
const CANCEL: u64 = 1 << 40;

/// What the kernel reads or writes for an op after `push` returns: a
/// `SendTo`'s message, an accept's peer, or a dial's address.
struct Msg {
    addr: libc::sockaddr_storage,
    addr_len: libc::socklen_t,
    iov: libc::iovec,
    hdr: libc::msghdr,
}

/// An op whose success is a new descriptor.
#[derive(Default)]
enum Opening {
    #[default]
    None,
    Accept,
    /// The socket a connect runs on, and where it connects.
    Dial(OwnedFd, SocketAddr),
}

/// The `Os` for Linux targets.
pub struct UringOs {
    ring: IoUring,
    /// One per op table entry.
    msgs: Box<[Msg]>,
    /// Each op table entry's opening, if it is one. A cancel names the op by
    /// its id alone, so the kind is kept here rather than in `user_data`.
    opens: Box<[Opening]>,
    /// Ops the kernel holds, whose entries may still write into `msgs`.
    in_flight: usize,
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
            opens: (0..ops).map(|_| Opening::None).collect(),
            in_flight: 0,
            done: Vec::with_capacity(2 * ops),
        })
    }

    /// The kernel entry for `op`, or `None` for one `sys::run` runs inline.
    fn entry(&mut self, op: &OsOp) -> Result<Option<Entry>, IoError> {
        let index = usize::from(op.id.index());
        let (Some(msg), Some(opening)) = (self.msgs.get_mut(index), self.opens.get_mut(index))
        else {
            return Ok(None);
        };
        *opening = Opening::None;
        let fd = Fd(op.fd);
        let buf = op.bytes();
        let (ptr, len) = (buf.cast::<u8>().as_ptr(), buf.len() as u32);
        let at = |offset: Option<u64>| offset.unwrap_or(CURRENT_POSITION);
        let entry = match (op.op, op.class) {
            (Op::Read { offset }, _) => opcode::Read::new(fd, ptr, len).offset(at(offset)).build(),
            (Op::Write { offset }, _) => {
                opcode::Write::new(fd, ptr, len).offset(at(offset)).build()
            }
            (Op::Recv, _) => opcode::Recv::new(fd, ptr, len).build(),
            (Op::Send, _) => opcode::Send::new(fd, ptr, len)
                .flags(sys::SEND_FLAGS)
                .build(),
            (Op::SendTo(addr), _) => {
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
            (Op::Stream, Class::Listener) => {
                let flags = libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC;
                msg.addr_len = size_of::<libc::sockaddr_storage>() as libc::socklen_t;
                let addr = (&raw mut msg.addr).cast();
                *opening = Opening::Accept;
                opcode::Accept::new(fd, addr, &raw mut msg.addr_len)
                    .flags(flags)
                    .build()
            }
            (Op::Stream, Class::Dialer) => {
                let to = op.dial.ok_or(IoError::Unsupported)?;
                let socket = sys::stream_socket(to)?;
                (msg.addr, msg.addr_len) = sys::sockaddr(to);
                let fd = Fd(socket.as_raw_fd());
                *opening = Opening::Dial(socket, to);
                opcode::Connect::new(fd, (&raw const msg.addr).cast(), msg.addr_len).build()
            }
            (Op::Fsync, _) => opcode::Fsync::new(fd).build(),
            _ => return Ok(None),
        };
        Ok(Some(entry.user_data(u64::from(op.id.get()))))
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
        let entry = match self.entry(&op) {
            Ok(Some(entry)) => entry,
            Ok(None) => {
                let result = sys::run(&op);
                return self.done.push(OsDone { id: op.id, result });
            }
            Err(e) => {
                return self.done.push(OsDone {
                    id: op.id,
                    result: Err(e),
                });
            }
        };
        if self.queue(&entry) {
            self.in_flight += 1;
        } else {
            drop(
                self.opens
                    .get_mut(usize::from(op.id.index()))
                    .map(core::mem::take),
            );
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
        let Self {
            ring,
            opens,
            msgs,
            in_flight,
            ..
        } = self;
        for cqe in ring.completion() {
            let data = cqe.user_data();
            if data & CANCEL != 0 {
                continue;
            }
            *in_flight = in_flight.saturating_sub(1);
            let id = OpId::from_raw(data as u32);
            let result = finished(opens, msgs, id, cqe.result());
            done.push(OsDone { id, result });
        }
    }

    fn close(&mut self, fd: OwnedFd) {
        let every = CancelBuilder::fd(Fd(fd.as_raw_fd())).all();
        let entry = opcode::AsyncCancel2::new(every).build().user_data(CANCEL);
        self.queue(&entry);
        let _ = self.ring.submit();
    }
}

/// The kernel may still write a peer address into `msgs` for an op it
/// holds after the ring closes, so with ops in flight `msgs` outlives this.
impl Drop for UringOs {
    fn drop(&mut self) {
        if self.in_flight > 0 {
            core::mem::forget(core::mem::take(&mut self.msgs));
        }
    }
}

/// What finished op `id` gives back, from the kernel's result `res`.
fn finished(opens: &mut [Opening], msgs: &[Msg], id: OpId, res: i32) -> Result<Out, IoError> {
    let index = usize::from(id.index());
    let opening = opens.get_mut(index).map(core::mem::take);
    match (res, opening) {
        (..0, _) if res == -libc::ECANCELED => Err(IoError::Canceled),
        (..0, _) => Err(IoError::Os(sys::linux(-res).unwrap_or(Errno::EIO))),
        (_, Some(Opening::Accept)) => Ok(Out::Opened {
            // SAFETY: an accept's result is a new descriptor, now owned here.
            fd: unsafe { OwnedFd::from_raw_fd(res) },
            class: Class::Stream,
            peer: msgs.get(index).and_then(|m| sys::socket_addr(&m.addr)),
        }),
        (_, Some(Opening::Dial(fd, to))) => Ok(Out::Opened {
            fd,
            class: Class::Stream,
            peer: Some(to),
        }),
        _ => Ok(Out::Len(res as u32)),
    }
}
