//! Handles to resources, typed by the interface the resource provides.

use core::marker::PhantomData;
use core::ops::Deref;

use super::arena::Buf;
use super::ops::{
    CloseOp, FsyncOp, GpioOp, IoOp, OpenOp, ReadOp, RecvOp, SendOp, StreamOp, TimeoutOp, TtyOp,
    WriteOp,
};
use super::queue::{Io, Pending};
use super::{Op, OpenFlags, RawHandle, TtyConfig, TtyQueue};

/// What a resource lets a handle do. Build and stubgen compare interfaces by
/// `NAME`.
pub trait Interface: 'static {
    const NAME: &'static str;
}

/// A serial port.
pub struct Uart;
/// A CAN bus.
pub struct Can;
/// A UDP socket.
pub struct Udp;
/// A directory files are opened under.
pub struct Dir;
/// A file, granted directly or opened under a `Dir`.
pub struct RegularFile;
/// Where streams come from: a TCP listener or a TCP dialer.
pub struct Endpoint;
/// A GPIO line.
pub struct GpioLine;
/// A connected stream, from `Endpoint::stream`.
pub struct Stream;

impl Interface for Uart {
    const NAME: &'static str = "uart";
}
impl Interface for Can {
    const NAME: &'static str = "can";
}
impl Interface for Udp {
    const NAME: &'static str = "udp";
}
impl Interface for Dir {
    const NAME: &'static str = "dir";
}
impl Interface for RegularFile {
    const NAME: &'static str = "file";
}
impl Interface for Endpoint {
    const NAME: &'static str = "endpoint";
}
impl Interface for GpioLine {
    const NAME: &'static str = "gpio_line";
}
impl Interface for Stream {
    const NAME: &'static str = "stream";
}

/// A handle to a resource. Its methods submit ops on its system's queue.
pub struct Handle<K> {
    raw: RawHandle,
    io: Io,
    _k: PhantomData<K>,
}

impl<K> Clone for Handle<K> {
    fn clone(&self) -> Self {
        Self::new(self.raw, self.io.clone())
    }
}

impl<K> Handle<K> {
    pub(crate) fn new(raw: RawHandle, io: Io) -> Self {
        Self {
            raw,
            io,
            _k: PhantomData,
        }
    }

    pub fn raw(&self) -> RawHandle {
        self.raw
    }

    /// How many ops this handle's system may have in flight at once.
    pub fn budget(&self) -> usize {
        self.io.ops()
    }

    fn submit<O: IoOp>(&self, op: Op, lent: O::Lent) -> Pending<O> {
        self.io.submit(self.raw, op, lent)
    }

    /// Ends this capability. Later ops on it fail with `Stale`.
    pub fn close(&self) -> Pending<CloseOp> {
        self.submit(Op::Close, ())
    }

    /// Lands `Ok` once `after`, rounded up to a microsecond, has passed on
    /// the cycle's clock, or `Canceled`. A wall clock's steps move it.
    pub fn timeout(&self, after: core::time::Duration) -> Pending<TimeoutOp> {
        self.submit(Op::Timeout(after), ())
    }
}

impl Handle<Uart> {
    /// Reads whatever bytes have arrived, up to the buffer's capacity.
    pub fn read(&self, buf: Buf) -> Pending<ReadOp> {
        self.submit(Op::Read { offset: None }, buf)
    }

    pub fn write(&self, buf: Buf) -> Pending<WriteOp> {
        self.submit(Op::Write { offset: None }, buf)
    }

    pub fn configure(&self, config: TtyConfig) -> Pending<TtyOp> {
        self.submit(Op::TtyConfigure(config), ())
    }

    pub fn flush(&self, queue: TtyQueue) -> Pending<TtyOp> {
        self.submit(Op::TtyFlush(queue), ())
    }
}

/// Message handles: `Can`, `Udp`, and `Stream`.
pub trait Messages {}
impl Messages for Can {}
impl Messages for Udp {}
impl Messages for Stream {}

impl<K: Messages> Handle<K> {
    pub fn recv(&self, buf: Buf) -> Pending<RecvOp> {
        self.submit(Op::Recv, buf)
    }

    pub fn send(&self, buf: Buf) -> Pending<SendOp> {
        self.submit(Op::Send, buf)
    }
}

impl Handle<RegularFile> {
    pub fn read(&self, buf: Buf, offset: u64) -> Pending<ReadOp> {
        let offset = Some(offset);
        self.submit(Op::Read { offset }, buf)
    }

    pub fn write(&self, buf: Buf, offset: u64) -> Pending<WriteOp> {
        let offset = Some(offset);
        self.submit(Op::Write { offset }, buf)
    }

    pub fn fsync(&self) -> Pending<FsyncOp> {
        self.submit(Op::Fsync, ())
    }
}

impl Handle<Dir> {
    /// Opens the file at the path in `path`, relative to this directory.
    pub fn open(&self, path: Buf, flags: OpenFlags) -> Pending<OpenOp> {
        self.submit(Op::Open(flags), path)
    }
}

impl Handle<Endpoint> {
    /// The next stream: a listener's next allowed peer, or a dialer's new
    /// connection. Its peer's `RawAddr` fills `peer`.
    pub fn stream(&self, peer: Buf) -> Pending<StreamOp> {
        self.submit(Op::Stream, peer)
    }
}

impl Handle<GpioLine> {
    pub fn get(&self) -> Pending<GpioOp> {
        self.submit(Op::GpioGet, ())
    }

    pub fn set(&self, high: bool) -> Pending<GpioOp> {
        self.submit(Op::GpioSet(high), ())
    }
}

/// The capability a system was granted. It derefs to its `Handle`.
pub struct Cap<K: Interface>(Handle<K>);

impl<K: Interface> Cap<K> {
    pub(crate) fn new(handle: Handle<K>) -> Self {
        Self(handle)
    }
}

impl<K: Interface> Deref for Cap<K> {
    type Target = Handle<K>;

    fn deref(&self) -> &Handle<K> {
        &self.0
    }
}
