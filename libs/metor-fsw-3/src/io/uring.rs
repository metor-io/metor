//! An `Os` built on its own io_uring instance.
//!
//! `UringOs::new` registers each frozen table as one fixed buffer. Dialed
//! sockets are direct descriptors in a sparse fixed-file table: `Connect`
//! submits a `Socket` op linked to a `Connect` op on the new slot. Tty and GPIO
//! ioctls and `Close` run synchronously.

use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};

use io_uring::squeue::{Entry, Flags};
use io_uring::types::{DestinationSlot, Fd, Fixed};
use io_uring::{IoUring, Probe, opcode};

use super::os::{self, FdClass, Os, OsError, open_flags, sockaddr};
use super::{
    BufRef, Cqe, Errno, IoError, Limits, Op, OpenFlags, Out, RawAddr, RawHandle, Sqe, TableId,
    Tables, Token,
};

const OWN: u32 = 0x8000_0000;
/// Set in the `user_data` of a dial's `Socket` op.
const SOCKET: u64 = 1 << 32;
/// The longest `Open` path, including the NUL.
const PATH_MAX: usize = 4096;
/// The most `Open` paths held between submits.
const PATHS: usize = 4;

#[derive(Clone, Copy)]
enum Target {
    Raw(RawFd),
    Fixed(u32),
}

enum Res {
    Free,
    Dial(RawAddr),
    Raw(OwnedFd, FdClass),
    Fixed(u32),
}

struct Slot {
    generation: u32,
    res: Res,
}

/// How to turn a completion's `res` into an `Out`.
#[derive(Clone, Copy)]
enum Then {
    Unit,
    Len,
    Filled(BufRef),
    Opened(FdClass),
    Connected { file: u32, failed: i32 },
}

/// Per-token memory that the kernel reads after submit.
struct Mem {
    then: Option<Then>,
    addr: libc::sockaddr_storage,
    msg: libc::msghdr,
    iov: libc::iovec,
}

/// The `Os` for Linux targets.
pub struct UringOs {
    ring: IoUring,
    mem: Box<[Mem]>,
    ops: u32,
    tables: Tables,
    fixed: Vec<Option<u16>>,
    slots: Vec<Slot>,
    free: Vec<u32>,
    files: Vec<u32>,
    paths: Box<[[u8; PATH_MAX]; PATHS]>,
    paths_used: usize,
    landed: Vec<(u64, i32)>,
    done: Vec<Cqe>,
}

impl UringOs {
    /// Creates the backend. Dial target `i` gets the handle
    /// `RawHandle::new(i, 0)`. Each table in `tables` that is already frozen gets
    /// a fixed buffer.
    pub fn new(limits: &Limits, tables: Tables, dials: &[RawAddr]) -> Result<Self, OsError> {
        let ops = limits.ops.max(1) as u32;
        let ring = IoUring::new((3 * ops).next_power_of_two())?;
        probe(&ring)?;
        ring.submitter().register_files_sparse(ops)?;
        let fixed = register_tables(&ring, &tables)?;
        let mut slots = Vec::with_capacity(dials.len() + 2 * 3 * ops as usize);
        slots.extend(dials.iter().map(|a| Slot {
            generation: 0,
            res: Res::Dial(*a),
        }));
        Ok(Self {
            ring,
            mem: (0..3 * ops).map(|_| Mem::new()).collect(),
            ops,
            tables,
            fixed,
            slots,
            free: Vec::with_capacity(2 * 3 * ops as usize),
            files: (0..ops).rev().collect(),
            paths: Box::new([[0; PATH_MAX]; PATHS]),
            paths_used: 0,
            landed: Vec::with_capacity(4 * 3 * ops as usize),
            done: Vec::with_capacity(3 * ops as usize),
        })
    }

    fn mem_index(&self, token: Token) -> Option<usize> {
        let t = token.get();
        let i = if t & OWN != 0 {
            self.ops + (t & !OWN)
        } else {
            t
        };
        (i < 3 * self.ops).then_some(i as usize)
    }

    fn issue(&mut self, res: Res) -> RawHandle {
        if let Some(i) = self.free.pop() {
            let slot = &mut self.slots[i as usize];
            slot.res = res;
            return RawHandle::new(i, slot.generation);
        }
        self.slots.push(Slot { generation: 0, res });
        RawHandle::new(self.slots.len() as u32 - 1, 0)
    }

    fn release(&mut self, i: u32) {
        let slot = &mut self.slots[i as usize];
        if let Res::Fixed(file) = slot.res {
            self.drop_file(file);
        }
        let slot = &mut self.slots[i as usize];
        slot.res = Res::Free;
        slot.generation = slot.generation.wrapping_add(1);
        self.free.push(i);
    }

    fn drop_file(&mut self, file: u32) {
        let _ = self.ring.submitter().register_files_update(file, &[-1]);
        self.files.push(file);
    }

    fn lookup(&self, h: RawHandle) -> Option<&Res> {
        let slot = self.slots.get(h.index() as usize)?;
        let live = slot.generation == h.generation() && !matches!(slot.res, Res::Free);
        live.then_some(&slot.res)
    }

    fn inline(&mut self, token: Token, r: Result<Out, IoError>) {
        self.done.push(match r {
            Ok(out) => Cqe::ok(token, out),
            Err(e) => Cqe::err(token, e),
        });
    }

    fn start(&mut self, sqe: &Sqe) -> Result<Option<Out>, IoError> {
        let m = self.mem_index(sqe.token).ok_or(IoError::Refused)?;
        if let Op::Cancel { target, .. } = sqe.op {
            let e = opcode::AsyncCancel::new(u64::from(target.get())).build();
            return self.queue(m, sqe.token, Then::Unit, &[e]).map(|_| None);
        }
        if sqe.handle == RawHandle::CWD {
            let Op::Open { path, flags, .. } = sqe.op else {
                return Err(IoError::Stale);
            };
            return self
                .open(m, sqe.token, libc::AT_FDCWD, path, flags)
                .map(|_| None);
        }
        match self.lookup(sqe.handle).ok_or(IoError::Stale)? {
            Res::Dial(addr) => {
                let addr = *addr;
                self.dial_op(m, sqe.token, sqe.op, addr)
            }
            Res::Raw(fd, class) => {
                let (fd, class) = (fd.as_raw_fd(), *class);
                self.fd_op(m, sqe, Target::Raw(fd), class)
            }
            Res::Fixed(file) => {
                let file = *file;
                self.fd_op(m, sqe, Target::Fixed(file), FdClass::Stream)
            }
            Res::Free => Err(IoError::Stale),
        }
    }

    fn dial_op(
        &mut self,
        m: usize,
        token: Token,
        op: Op,
        addr: RawAddr,
    ) -> Result<Option<Out>, IoError> {
        match op {
            Op::Resolve { .. } => Ok(Some(Out::addr(addr))),
            Op::Connect { addr, .. } => self.connect(m, token, addr).map(|_| None),
            _ => Err(IoError::Unsupported),
        }
    }

    fn fd_op(
        &mut self,
        m: usize,
        sqe: &Sqe,
        fd: Target,
        class: FdClass,
    ) -> Result<Option<Out>, IoError> {
        let i = sqe.handle.index();
        let entry = match sqe.op {
            Op::Read { buf, .. } | Op::Recv { buf, .. } => {
                let e = self.read(fd, buf, sqe.op.offset(), class.socket())?;
                (e, Then::Filled(buf))
            }
            Op::Write { buf, .. } | Op::Send { buf, .. } => (
                self.write(fd, buf, sqe.op.offset(), class.socket())?,
                Then::Len,
            ),
            Op::SendTo { buf, addr } => (self.send_to(m, fd, buf, addr)?, Then::Len),
            Op::Open { path, flags, .. } => {
                let Target::Raw(dir) = fd else {
                    return Err(IoError::Unsupported);
                };
                return self.open(m, sqe.token, dir, path, flags).map(|_| None);
            }
            Op::Accept { .. } => {
                let (addr, len) = (core::ptr::null_mut(), core::ptr::null_mut());
                let e = with_fd(
                    opcode::Accept::new(Fd(0), addr, len)
                        .flags(libc::SOCK_CLOEXEC)
                        .build(),
                    fd,
                );
                (e, Then::Opened(FdClass::Stream))
            }
            Op::Fsync { .. } => (with_fd(opcode::Fsync::new(Fd(0)).build(), fd), Then::Unit),
            // Synchronous ops run after every op queued before them.
            Op::Close { .. } => {
                self.flush();
                self.release(i);
                return Ok(Some(Out::UNIT));
            }
            Op::Tty { op, .. } => {
                self.flush();
                return quick(fd, |b| os::tty(b, &op));
            }
            Op::Gpio { op, .. } => {
                self.flush();
                return quick(fd, |b| os::gpio(b, &op));
            }
            Op::Resolve { .. } | Op::Connect { .. } | Op::Cancel { .. } => {
                return Err(IoError::Unsupported);
            }
        };
        self.queue(m, sqe.token, entry.1, &[entry.0]).map(|_| None)
    }

    fn read(
        &self,
        fd: Target,
        buf: BufRef,
        offset: Option<u64>,
        socket: bool,
    ) -> Result<Entry, IoError> {
        let raw = self.tables.raw(buf).ok_or(IoError::BadBuf)?;
        let (ptr, cap) = (raw.ptr.as_ptr(), raw.cap);
        let off = offset.unwrap_or(u64::MAX);
        let e = match (socket, self.fixed_index(buf.table())) {
            (true, _) => opcode::Recv::new(Fd(0), ptr, cap).build(),
            (false, Some(b)) => opcode::ReadFixed::new(Fd(0), ptr, cap, b)
                .offset(off)
                .build(),
            (false, None) => opcode::Read::new(Fd(0), ptr, cap).offset(off).build(),
        };
        Ok(with_fd(e, fd))
    }

    fn write(
        &self,
        fd: Target,
        buf: BufRef,
        offset: Option<u64>,
        socket: bool,
    ) -> Result<Entry, IoError> {
        let raw = self.tables.raw(buf).ok_or(IoError::BadBuf)?;
        let (ptr, len) = (raw.ptr.as_ptr().cast_const(), raw.len);
        let off = offset.unwrap_or(u64::MAX);
        let e = match (socket, self.fixed_index(buf.table())) {
            (true, _) => opcode::Send::new(Fd(0), ptr, len)
                .flags(libc::MSG_NOSIGNAL)
                .build(),
            (false, Some(b)) => opcode::WriteFixed::new(Fd(0), ptr, len, b)
                .offset(off)
                .build(),
            (false, None) => opcode::Write::new(Fd(0), ptr, len).offset(off).build(),
        };
        Ok(with_fd(e, fd))
    }

    fn send_to(
        &mut self,
        m: usize,
        fd: Target,
        buf: BufRef,
        addr: RawAddr,
    ) -> Result<Entry, IoError> {
        let raw = self.tables.raw(buf).ok_or(IoError::BadBuf)?;
        let (ss, len) = sockaddr(addr)?;
        let mem = &mut self.mem[m];
        mem.addr = ss;
        mem.iov = libc::iovec {
            iov_base: raw.ptr.as_ptr().cast(),
            iov_len: raw.len as usize,
        };
        mem.msg.msg_name = (&raw mut mem.addr).cast();
        mem.msg.msg_namelen = len;
        mem.msg.msg_iov = &raw mut mem.iov;
        mem.msg.msg_iovlen = 1;
        let e = opcode::SendMsg::new(Fd(0), &raw const mem.msg).flags(libc::MSG_NOSIGNAL as u32);
        Ok(with_fd(e.build(), fd))
    }

    fn open(
        &mut self,
        m: usize,
        token: Token,
        dir: RawFd,
        path: BufRef,
        flags: OpenFlags,
    ) -> Result<(), IoError> {
        let raw = self.tables.raw(path).ok_or(IoError::BadBuf)?;
        // SAFETY: the slot's `len` bytes are filled.
        let path = unsafe { core::slice::from_raw_parts(raw.ptr.as_ptr(), raw.len as usize) };
        if path.len() >= PATH_MAX {
            return Err(IoError::Os(Errno::ENAMETOOLONG));
        }
        if path.contains(&0) {
            return Err(IoError::Os(Errno::EINVAL));
        }
        // The kernel copies each path during submit, which frees the pool.
        // Flushing first also keeps `queue` from flushing past this path.
        if self.paths_used == PATHS || self.sq_free() == 0 {
            self.flush();
        }
        let cpath = &mut self.paths[self.paths_used];
        self.paths_used += 1;
        cpath[..path.len()].copy_from_slice(path);
        cpath[path.len()] = 0;
        let (oflags, mode) = open_flags(flags);
        let e = opcode::OpenAt::new(Fd(dir), cpath.as_ptr().cast())
            .flags(oflags)
            .mode(mode);
        let dir = flags.contains(OpenFlags::DIRECTORY);
        let class = if dir { FdClass::Dir } else { FdClass::File };
        self.queue(m, token, Then::Opened(class), &[e.build()])
    }

    /// Submits a `Socket` op into a free fixed-file slot, linked to a `Connect`
    /// on that slot.
    fn connect(&mut self, m: usize, token: Token, addr: RawAddr) -> Result<(), IoError> {
        let (ss, len) = sockaddr(addr)?;
        let file = self.files.pop().ok_or(IoError::QueueFull)?;
        // PANIC Safety: `file` is below the sparse table's size, which fits `u32`.
        let dest = DestinationSlot::try_from_slot_target(file).expect("slot in range");
        let family = i32::from(ss.ss_family);
        // Direct descriptors reject `SOCK_CLOEXEC`, and exec never inherits them.
        let socket = opcode::Socket::new(family, libc::SOCK_STREAM, 0)
            .file_index(Some(dest))
            .build();
        let socket = socket
            .flags(Flags::IO_LINK)
            .user_data(SOCKET | u64::from(token.get()));
        self.mem[m].addr = ss;
        let sa = (&raw const self.mem[m].addr).cast::<libc::sockaddr>();
        let connect = opcode::Connect::new(Fixed(file), sa, len).build();
        let then = Then::Connected { file, failed: 0 };
        let r = self.queue(m, token, then, &[socket, connect]);
        if r.is_err() {
            self.files.push(file);
        }
        r
    }

    /// Pushes the linked `entries` of one op. The last entry carries `token`.
    fn queue(
        &mut self,
        m: usize,
        token: Token,
        then: Then,
        entries: &[Entry],
    ) -> Result<(), IoError> {
        if self.sq_free() < entries.len() {
            self.flush();
        }
        if self.sq_free() < entries.len() {
            return Err(IoError::QueueFull);
        }
        self.mem[m].then = Some(then);
        let mut sq = self.ring.submission();
        for (n, e) in entries.iter().enumerate() {
            let last = n + 1 == entries.len();
            let e = if last {
                e.clone().user_data(u64::from(token.get()))
            } else {
                e.clone()
            };
            // SAFETY: every pointer in `e` points into a frozen table,
            // `self.mem`, or `self.paths`, all of which outlive the request.
            let _ = unsafe { sq.push(&e) };
        }
        Ok(())
    }

    fn sq_free(&mut self) -> usize {
        let sq = self.ring.submission();
        sq.capacity() - sq.len()
    }

    fn flush(&mut self) {
        let _ = self.ring.submit();
        self.paths_used = 0;
    }

    pub(crate) fn fixed_index(&self, table: TableId) -> Option<u16> {
        *self.fixed.get(usize::from(table.0))?
    }

    fn complete(&mut self, user_data: u64, res: i32) {
        let token = Token::new(user_data as u32);
        let Some(m) = self.mem_index(token) else {
            return;
        };
        if user_data & SOCKET != 0 {
            if let Some(Then::Connected { failed, .. }) = &mut self.mem[m].then {
                *failed = res.min(0);
            }
            return;
        }
        let Some(then) = self.mem[m].then.take() else {
            return;
        };
        let r = self.outcome(then, res);
        self.inline(token, r);
    }

    fn outcome(&mut self, then: Then, res: i32) -> Result<Out, IoError> {
        if let Then::Connected { file, failed } = then {
            let res = if failed < 0 { failed } else { res };
            if res < 0 {
                if failed == 0 {
                    self.drop_file(file);
                } else {
                    self.files.push(file);
                }
                return Err(uring_error(res));
            }
            return Ok(Out::handle(self.issue(Res::Fixed(file))));
        }
        if res < 0 {
            return Err(uring_error(res));
        }
        Ok(match then {
            Then::Unit => Out::UNIT,
            Then::Len => Out::len(res as u32),
            Then::Filled(buf) => {
                self.tables
                    .set_len(buf, res as u32)
                    .map_err(|_| IoError::BadBuf)?;
                Out::len(res as u32)
            }
            Then::Opened(class) => {
                // SAFETY: the kernel returned a new descriptor.
                let fd = unsafe { OwnedFd::from_raw_fd(res) };
                Out::handle(self.issue(Res::Raw(fd, class)))
            }
            Then::Connected { .. } => Out::UNIT,
        })
    }
}

impl Os for UringOs {
    fn push(&mut self, sqe: Sqe) {
        match self.start(&sqe) {
            Ok(None) => {}
            Ok(Some(out)) => self.inline(sqe.token, Ok(out)),
            Err(e) => self.inline(sqe.token, Err(e)),
        }
    }

    fn submit(&mut self) {
        self.flush();
    }

    fn reap(&mut self, done: &mut Vec<Cqe>) {
        let mut landed = core::mem::take(&mut self.landed);
        landed.extend(self.ring.completion().map(|c| (c.user_data(), c.result())));
        for (user_data, res) in landed.drain(..) {
            self.complete(user_data, res);
        }
        self.landed = landed;
        done.append(&mut self.done);
    }

    fn register(&mut self, fd: OwnedFd, class: FdClass) -> RawHandle {
        self.issue(Res::Raw(fd, class))
    }
}

impl Mem {
    fn new() -> Self {
        // SAFETY: all-zero is valid for these C structs.
        let (addr, msg, iov) = unsafe { core::mem::zeroed() };
        Self {
            then: None,
            addr,
            msg,
            iov,
        }
    }
}

fn with_fd(e: Entry, fd: Target) -> Entry {
    match fd {
        Target::Raw(fd) => set_fd(e, fd),
        Target::Fixed(file) => set_fd(e, file as RawFd).flags(Flags::FIXED_FILE),
    }
}

/// Sets an entry's descriptor field.
fn set_fd(e: Entry, fd: RawFd) -> Entry {
    // SAFETY: `Entry` is `repr(C)` over `io_uring_sqe`, whose `fd` is the
    // `i32` at byte 4.
    unsafe {
        let mut e = e;
        let p = (&raw mut e).cast::<u8>().add(4).cast::<i32>();
        p.write_unaligned(fd);
        e
    }
}

fn quick(
    fd: Target,
    f: impl FnOnce(BorrowedFd) -> Result<Out, IoError>,
) -> Result<Option<Out>, IoError> {
    let Target::Raw(fd) = fd else {
        return Err(IoError::Unsupported);
    };
    // SAFETY: `fd` is open for the call.
    f(unsafe { BorrowedFd::borrow_raw(fd) }).map(Some)
}

fn uring_error(res: i32) -> IoError {
    match -res {
        libc::ECANCELED => IoError::Canceled,
        e => IoError::Os(Errno(e as u16)),
    }
}

fn probe(ring: &IoUring) -> Result<(), OsError> {
    let mut probe = Probe::new();
    ring.submitter().register_probe(&mut probe)?;
    if probe.is_supported(opcode::Socket::CODE) {
        Ok(())
    } else {
        Err(OsError::Unsupported("IORING_OP_SOCKET needs Linux 5.19"))
    }
}

/// Registers each frozen table as one fixed buffer and returns each table's
/// buffer index.
fn register_tables(ring: &IoUring, tables: &Tables) -> Result<Vec<Option<u16>>, OsError> {
    let mut iovs = Vec::new();
    let fixed = (0..tables.len())
        .map(|i| {
            let arena = tables.get(TableId(i as u16))?.region()?;
            iovs.push(libc::iovec {
                iov_base: arena.cast().as_ptr(),
                iov_len: arena.len(),
            });
            Some(iovs.len() as u16 - 1)
        })
        .collect();
    if !iovs.is_empty() {
        // SAFETY: each table is frozen and outlives the ring. The reactor
        // leaks any table with ops in flight.
        unsafe { ring.submitter().register_buffers(&iovs)? };
    }
    Ok(fixed)
}
