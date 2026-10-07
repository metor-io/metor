//! `PollOs`: ops as nonblocking syscalls, with `polling` for readiness.
//!
//! An op on a polled descriptor is tried at once; `EAGAIN` parks it until its
//! descriptor is ready. Each descriptor's interest is the union of its parked
//! ops'. File, directory, tty-ioctl, and GPIO ops run inline and blocking.
//! Results wait for `reap`.

use core::num::NonZeroUsize;
use core::time::Duration;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};

use polling::{Event, Events, Poller};

use super::os::{self, FdClass, Os, OsError, cvt, open_flags, sockaddr};
use super::{BufRef, Cqe, Errno, IoError, Limits, Op, Out, RawAddr, RawHandle, Sqe, Tables, Token};

/// The longest path `Open` takes, with its NUL.
const PATH_MAX: usize = 4096;

enum Res {
    Free,
    Dial(RawAddr),
    Fd {
        fd: OwnedFd,
        class: FdClass,
        added: bool,
    },
}

struct Slot {
    generation: u32,
    res: Res,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Need {
    Read,
    Write,
}

#[derive(Clone, Copy)]
struct Parked {
    sqe: Sqe,
    fd: u32,
    need: Need,
}

enum Step {
    Done(Result<Out, IoError>),
    Park(u32, Need),
}

/// The readiness backend for Unix development hosts.
pub struct PollOs {
    poller: Poller,
    events: Events,
    slots: Vec<Slot>,
    free: Vec<u32>,
    queued: Vec<Sqe>,
    parked: Vec<Parked>,
    fired: Vec<u32>,
    done: Vec<Cqe>,
    tables: Tables,
}

impl PollOs {
    /// Dial target `i` gets the handle `RawHandle::new(i, 0)`.
    pub fn new(limits: &Limits, tables: Tables, dials: &[RawAddr]) -> Result<Self, OsError> {
        let ops = 3 * limits.ops.max(1);
        let mut slots = Vec::with_capacity(dials.len() + 2 * ops);
        slots.extend(dials.iter().map(|a| Slot {
            generation: 0,
            res: Res::Dial(*a),
        }));
        // PANIC Safety: `ops` is at least 3.
        let cap = NonZeroUsize::new(ops).expect("nonzero");
        Ok(Self {
            poller: Poller::new()?,
            events: Events::with_capacity(cap),
            slots,
            free: Vec::with_capacity(2 * ops),
            queued: Vec::with_capacity(ops),
            parked: Vec::with_capacity(ops),
            fired: Vec::with_capacity(ops),
            done: Vec::with_capacity(ops),
            tables,
        })
    }

    fn issue(&mut self, fd: OwnedFd, class: FdClass) -> RawHandle {
        prepare(&fd, class);
        let res = Res::Fd {
            fd,
            class,
            added: false,
        };
        if let Some(i) = self.free.pop() {
            let slot = &mut self.slots[i as usize];
            slot.res = res;
            return RawHandle::new(i, slot.generation);
        }
        self.slots.push(Slot { generation: 0, res });
        RawHandle::new(self.slots.len() as u32 - 1, 0)
    }

    /// Closes a descriptor and cancels the ops parked on it.
    fn release(&mut self, i: u32) {
        let slot = &mut self.slots[i as usize];
        if let Res::Fd {
            fd, added: true, ..
        } = &slot.res
        {
            let _ = self.poller.delete(fd);
        }
        slot.res = Res::Free;
        slot.generation = slot.generation.wrapping_add(1);
        self.free.push(i);
        while let Some(at) = self.parked.iter().position(|p| p.fd == i) {
            let p = self.parked.remove(at);
            self.done.push(Cqe::err(p.sqe.token, IoError::Canceled));
        }
    }

    fn lookup(&self, h: RawHandle) -> Option<&Res> {
        let slot = self.slots.get(h.index() as usize)?;
        let live = slot.generation == h.generation() && !matches!(slot.res, Res::Free);
        live.then_some(&slot.res)
    }

    fn run(&mut self, sqe: Sqe) {
        match self.attempt(&sqe) {
            Step::Done(r) => self.finish(sqe.token, r),
            Step::Park(fd, need) => {
                self.parked.push(Parked { sqe, fd, need });
                self.arm(fd);
            }
        }
    }

    fn finish(&mut self, token: Token, r: Result<Out, IoError>) {
        self.done.push(match r {
            Ok(out) => Cqe::ok(token, out),
            Err(e) => Cqe::err(token, e),
        });
    }

    fn attempt(&mut self, sqe: &Sqe) -> Step {
        if let Op::Cancel { target, .. } = sqe.op {
            return Step::Done(self.cancel(target));
        }
        if sqe.handle == RawHandle::CWD {
            return Step::Done(match sqe.op {
                Op::Open { path, flags, .. } => self.open(libc::AT_FDCWD, path, flags),
                _ => Err(IoError::Stale),
            });
        }
        match self.lookup(sqe.handle) {
            None => Step::Done(Err(IoError::Stale)),
            Some(Res::Dial(addr)) => self.dial_op(sqe.op, *addr),
            Some(Res::Fd { fd, class, .. }) => {
                let (fd, class) = (fd.as_raw_fd(), *class);
                self.fd_op(sqe.handle.index(), fd, class, sqe.op)
            }
            Some(Res::Free) => Step::Done(Err(IoError::Stale)),
        }
    }

    fn dial_op(&mut self, op: Op, addr: RawAddr) -> Step {
        match op {
            Op::Resolve { .. } => Step::Done(Ok(Out::addr(addr))),
            Op::Connect { addr, .. } => self.connect(addr),
            _ => Step::Done(Err(IoError::Unsupported)),
        }
    }

    fn fd_op(&mut self, i: u32, fd: RawFd, class: FdClass, op: Op) -> Step {
        // SAFETY: `fd` stays open for the call; only `Close` drops it, after use.
        let bfd = unsafe { BorrowedFd::borrow_raw(fd) };
        let r = match op {
            Op::Read { buf, .. } => self.read(fd, buf, op.offset()),
            Op::Write { buf, .. } => self.write(fd, buf, op.offset()),
            Op::Recv { buf, .. } if class.socket() => self.recv(fd, buf),
            Op::Recv { buf, .. } => self.read(fd, buf, None),
            Op::Send { buf, .. } if class.socket() => self.send(fd, buf, None),
            Op::Send { buf, .. } => self.write(fd, buf, None),
            Op::SendTo { buf, addr } => self.send(fd, buf, Some(addr)),
            Op::Open { path, flags, .. } => self.open(fd, path, flags),
            Op::Accept { .. } => self.accept(fd),
            Op::Close { .. } => {
                self.release(i);
                Ok(Out::UNIT)
            }
            // SAFETY: `fd` is open.
            Op::Fsync { .. } => cvt(unsafe { libc::fsync(fd) }).map(|_| Out::UNIT),
            Op::Tty { op, .. } => os::tty(bfd, &op),
            Op::Gpio { op, .. } => os::gpio(bfd, &op),
            Op::Resolve { .. } | Op::Connect { .. } | Op::Cancel { .. } => {
                Err(IoError::Unsupported)
            }
        };
        match r {
            Err(IoError::Os(Errno::EAGAIN)) if class.polled() => Step::Park(i, need(&op)),
            r => Step::Done(r),
        }
    }

    fn read(&self, fd: RawFd, buf: BufRef, offset: Option<u64>) -> Result<Out, IoError> {
        let raw = self.tables.raw(buf).ok_or(IoError::BadBuf)?;
        let (ptr, cap) = (raw.ptr.as_ptr().cast(), raw.cap as usize);
        // SAFETY: the slot's `cap` bytes are the system's and no other op uses them.
        let n = cvt(unsafe {
            match offset {
                Some(o) => libc::pread(fd, ptr, cap, o as libc::off_t),
                None => libc::read(fd, ptr, cap),
            }
        })?;
        self.filled(buf, n)
    }

    fn recv(&self, fd: RawFd, buf: BufRef) -> Result<Out, IoError> {
        let raw = self.tables.raw(buf).ok_or(IoError::BadBuf)?;
        // SAFETY: as in `read`.
        let n = cvt(unsafe { libc::recv(fd, raw.ptr.as_ptr().cast(), raw.cap as usize, 0) })?;
        self.filled(buf, n)
    }

    fn filled(&self, buf: BufRef, n: isize) -> Result<Out, IoError> {
        let n = n as u32;
        self.tables.set_len(buf, n).map_err(|_| IoError::BadBuf)?;
        Ok(Out::len(n))
    }

    fn write(&self, fd: RawFd, buf: BufRef, offset: Option<u64>) -> Result<Out, IoError> {
        let raw = self.tables.raw(buf).ok_or(IoError::BadBuf)?;
        let (ptr, len) = (raw.ptr.as_ptr().cast(), raw.len as usize);
        // SAFETY: the slot's `len` bytes are filled.
        let n = cvt(unsafe {
            match offset {
                Some(o) => libc::pwrite(fd, ptr, len, o as libc::off_t),
                None => libc::write(fd, ptr, len),
            }
        })?;
        Ok(Out::len(n as u32))
    }

    fn send(&self, fd: RawFd, buf: BufRef, to: Option<RawAddr>) -> Result<Out, IoError> {
        let raw = self.tables.raw(buf).ok_or(IoError::BadBuf)?;
        let (ptr, len) = (raw.ptr.as_ptr().cast(), raw.len as usize);
        let n = match to {
            // SAFETY: the slot's `len` bytes are filled.
            None => unsafe { libc::send(fd, ptr, len, SEND_FLAGS) },
            Some(addr) => {
                let (ss, sl) = sockaddr(addr)?;
                let sa = (&raw const ss).cast::<libc::sockaddr>();
                // SAFETY: as above, and `sa` is `sl` bytes long.
                unsafe { libc::sendto(fd, ptr, len, SEND_FLAGS, sa, sl) }
            }
        };
        Ok(Out::len(cvt(n)? as u32))
    }

    fn open(&mut self, dir: RawFd, path: BufRef, flags: super::OpenFlags) -> Result<Out, IoError> {
        let raw = self.tables.raw(path).ok_or(IoError::BadBuf)?;
        // SAFETY: the slot's `len` bytes are filled.
        let path = unsafe { core::slice::from_raw_parts(raw.ptr.as_ptr(), raw.len as usize) };
        if path.len() >= PATH_MAX {
            return Err(IoError::Os(Errno::ENAMETOOLONG));
        }
        if path.contains(&0) {
            return Err(IoError::Os(Errno::EINVAL));
        }
        let mut cpath = [0u8; PATH_MAX];
        cpath[..path.len()].copy_from_slice(path);
        let (oflags, mode) = open_flags(flags);
        // SAFETY: `cpath` is NUL-terminated.
        let fd = cvt(unsafe { libc::openat(dir, cpath.as_ptr().cast(), oflags, mode as u32) })?;
        // SAFETY: `openat` returned a new descriptor.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let dir = flags.contains(super::OpenFlags::DIRECTORY);
        let class = if dir { FdClass::Dir } else { FdClass::File };
        Ok(Out::handle(self.issue(fd, class)))
    }

    fn accept(&mut self, fd: RawFd) -> Result<Out, IoError> {
        // SAFETY: `fd` is open; null addresses are allowed.
        let new = cvt(unsafe { libc::accept(fd, core::ptr::null_mut(), core::ptr::null_mut()) })?;
        // SAFETY: `accept` returned a new descriptor.
        let new = unsafe { OwnedFd::from_raw_fd(new) };
        Ok(Out::handle(self.issue(new, FdClass::Stream)))
    }

    fn connect(&mut self, addr: RawAddr) -> Step {
        let (ss, len) = match sockaddr(addr) {
            Ok(s) => s,
            Err(e) => return Step::Done(Err(e)),
        };
        // SAFETY: plain socket creation.
        let fd = match cvt(unsafe { libc::socket(ss.ss_family.into(), libc::SOCK_STREAM, 0) }) {
            // SAFETY: `socket` returned a new descriptor.
            Ok(fd) => unsafe { OwnedFd::from_raw_fd(fd) },
            Err(e) => return Step::Done(Err(e)),
        };
        let raw = fd.as_raw_fd();
        let h = self.issue(fd, FdClass::Stream);
        let sa = (&raw const ss).cast::<libc::sockaddr>();
        // SAFETY: `sa` is `len` bytes long.
        match cvt(unsafe { libc::connect(raw, sa, len) }) {
            Ok(_) => Step::Done(Ok(Out::handle(h))),
            Err(IoError::Os(Errno::EINPROGRESS)) => Step::Park(h.index(), Need::Write),
            Err(e) => {
                self.release(h.index());
                Step::Done(Err(e))
            }
        }
    }

    /// Ends a parked connect: `SO_ERROR` gives its result.
    fn connected(&mut self, i: u32) -> Result<Out, IoError> {
        let slot = &self.slots[i as usize];
        let Res::Fd { fd, .. } = &slot.res else {
            return Err(IoError::Stale);
        };
        let mut err: libc::c_int = 0;
        let mut len = size_of::<libc::c_int>() as libc::socklen_t;
        let (level, name) = (libc::SOL_SOCKET, libc::SO_ERROR);
        let p = (&raw mut err).cast();
        // SAFETY: `err` is a `c_int` of `len` bytes.
        cvt(unsafe { libc::getsockopt(fd.as_raw_fd(), level, name, p, &mut len) })?;
        if err == 0 {
            return Ok(Out::handle(RawHandle::new(i, slot.generation)));
        }
        self.release(i);
        Err(IoError::Os(errno(err)))
    }

    fn cancel(&mut self, target: Token) -> Result<Out, IoError> {
        let Some(at) = self.parked.iter().position(|p| p.sqe.token == target) else {
            let ran = self.done.iter().any(|c| c.token == target);
            return Err(IoError::Os(if ran {
                Errno::EALREADY
            } else {
                Errno::ENOENT
            }));
        };
        let p = self.parked.remove(at);
        self.done.push(Cqe::err(target, IoError::Canceled));
        if matches!(p.sqe.op, Op::Connect { .. }) {
            self.release(p.fd);
        } else {
            self.arm(p.fd);
        }
        Ok(Out::UNIT)
    }

    /// Sets a descriptor's interest to the union of its parked ops'.
    fn arm(&mut self, i: u32) {
        let parked = self.parked.iter().filter(|p| p.fd == i);
        let (mut r, mut w) = (false, false);
        for p in parked {
            r |= p.need == Need::Read;
            w |= p.need == Need::Write;
        }
        let key = i as usize;
        let ev = match (r, w) {
            (true, true) => Event::all(key),
            (true, false) => Event::readable(key),
            (false, true) => Event::writable(key),
            (false, false) => Event::none(key),
        };
        let Some(Slot {
            res: Res::Fd { fd, added, .. },
            ..
        }) = self.slots.get_mut(key)
        else {
            return;
        };
        if *added {
            let _ = self.poller.modify(&*fd, ev);
        } else if r || w {
            // SAFETY: the descriptor is deleted from the poller before it closes.
            *added = unsafe { self.poller.add(fd.as_raw_fd(), ev) }.is_ok();
        }
    }

    /// Retries every op parked on descriptor `i`.
    fn retry(&mut self, i: u32) {
        let mut at = 0;
        while at < self.parked.len() {
            if self.parked[at].fd != i {
                at += 1;
                continue;
            }
            let p = self.parked.remove(at);
            let step = match p.sqe.op {
                Op::Connect { .. } => Step::Done(self.connected(p.fd)),
                _ => self.attempt(&p.sqe),
            };
            match step {
                Step::Done(r) => self.finish(p.sqe.token, r),
                Step::Park(fd, need) => {
                    self.parked.insert(at, Parked { fd, need, ..p });
                    at += 1;
                }
            }
        }
        self.arm(i);
    }
}

impl Os for PollOs {
    fn push(&mut self, sqe: Sqe) {
        self.queued.push(sqe);
    }

    fn submit(&mut self) {
        for i in 0..self.queued.len() {
            let sqe = self.queued[i];
            self.run(sqe);
        }
        self.queued.clear();
    }

    fn reap(&mut self, done: &mut Vec<Cqe>) {
        self.events.clear();
        let _ = self.poller.wait(&mut self.events, Some(Duration::ZERO));
        self.fired.clear();
        self.fired.extend(self.events.iter().map(|e| e.key as u32));
        for k in 0..self.fired.len() {
            let fd = self.fired[k];
            self.retry(fd);
        }
        done.append(&mut self.done);
    }

    fn register(&mut self, fd: OwnedFd, class: FdClass) -> RawHandle {
        self.issue(fd, class)
    }
}

fn need(op: &Op) -> Need {
    match op {
        Op::Write { .. } | Op::Send { .. } | Op::SendTo { .. } | Op::Connect { .. } => Need::Write,
        _ => Need::Read,
    }
}

#[cfg(target_os = "linux")]
const SEND_FLAGS: libc::c_int = libc::MSG_NOSIGNAL;
#[cfg(not(target_os = "linux"))]
const SEND_FLAGS: libc::c_int = 0;

/// Close-on-exec, nonblocking when polled, and no `SIGPIPE` on sockets.
fn prepare(fd: &OwnedFd, class: FdClass) {
    let fd = fd.as_raw_fd();
    // SAFETY: `fcntl` and `setsockopt` on an open descriptor.
    unsafe {
        libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        if class.polled() {
            let fl = libc::fcntl(fd, libc::F_GETFL);
            libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
        }
        #[cfg(not(target_os = "linux"))]
        if class.socket() {
            let on: libc::c_int = 1;
            let len = size_of::<libc::c_int>() as libc::socklen_t;
            let p = (&raw const on).cast();
            libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_NOSIGPIPE, p, len);
        }
    }
}

/// Host errno numbers paired with Linux's.
pub(crate) const HOST_ERRNOS: [(Errno, i32); 48] = [
    (Errno::EPERM, libc::EPERM),
    (Errno::ENOENT, libc::ENOENT),
    (Errno::EINTR, libc::EINTR),
    (Errno::EIO, libc::EIO),
    (Errno::ENXIO, libc::ENXIO),
    (Errno::EBADF, libc::EBADF),
    (Errno::EAGAIN, libc::EAGAIN),
    (Errno::ENOMEM, libc::ENOMEM),
    (Errno::EACCES, libc::EACCES),
    (Errno::EFAULT, libc::EFAULT),
    (Errno::EBUSY, libc::EBUSY),
    (Errno::EEXIST, libc::EEXIST),
    (Errno::ENODEV, libc::ENODEV),
    (Errno::ENOTDIR, libc::ENOTDIR),
    (Errno::EISDIR, libc::EISDIR),
    (Errno::EINVAL, libc::EINVAL),
    (Errno::EMFILE, libc::EMFILE),
    (Errno::ENOTTY, libc::ENOTTY),
    (Errno::EFBIG, libc::EFBIG),
    (Errno::ENOSPC, libc::ENOSPC),
    (Errno::ESPIPE, libc::ESPIPE),
    (Errno::EROFS, libc::EROFS),
    (Errno::EPIPE, libc::EPIPE),
    (Errno::ENAMETOOLONG, libc::ENAMETOOLONG),
    (Errno::ENOSYS, libc::ENOSYS),
    (Errno::ENOTEMPTY, libc::ENOTEMPTY),
    (Errno::ELOOP, libc::ELOOP),
    (Errno::ETIME, libc::ETIME),
    (Errno::EOVERFLOW, libc::EOVERFLOW),
    (Errno::ENOTSOCK, libc::ENOTSOCK),
    (Errno::EMSGSIZE, libc::EMSGSIZE),
    (Errno::EOPNOTSUPP, libc::EOPNOTSUPP),
    (Errno::EAFNOSUPPORT, libc::EAFNOSUPPORT),
    (Errno::EADDRINUSE, libc::EADDRINUSE),
    (Errno::EADDRNOTAVAIL, libc::EADDRNOTAVAIL),
    (Errno::ENETDOWN, libc::ENETDOWN),
    (Errno::ENETUNREACH, libc::ENETUNREACH),
    (Errno::ECONNABORTED, libc::ECONNABORTED),
    (Errno::ECONNRESET, libc::ECONNRESET),
    (Errno::ENOBUFS, libc::ENOBUFS),
    (Errno::EISCONN, libc::EISCONN),
    (Errno::ENOTCONN, libc::ENOTCONN),
    (Errno::ETIMEDOUT, libc::ETIMEDOUT),
    (Errno::ECONNREFUSED, libc::ECONNREFUSED),
    (Errno::EHOSTUNREACH, libc::EHOSTUNREACH),
    (Errno::EALREADY, libc::EALREADY),
    (Errno::EINPROGRESS, libc::EINPROGRESS),
    (Errno::ECANCELED, libc::ECANCELED),
];

/// The Linux number for a host errno; `EIO` when it has none.
pub(crate) fn errno(host: i32) -> Errno {
    let found = HOST_ERRNOS.iter().find(|(_, h)| *h == host);
    found.map_or(Errno::EIO, |(e, _)| *e)
}

pub(crate) fn last_error() -> IoError {
    let host = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    IoError::Os(errno(host))
}
