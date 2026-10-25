//! Handles to resources, typed by the interface the resource provides.

use core::marker::PhantomData;

use super::arena::Buf;
use super::ops::{
    CloseOp, ConnectOp, FsyncOp, GpioOp, IoOp, ListenOp, OpenOp, ReadOp, RecvOp, SendOp, StreamOp,
    TimeoutOp, TtyOp, WriteOp,
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
/// A connected stream, dialed or accepted.
pub struct Stream;
/// A TCP listener, from `Cap<Endpoint>::listen`.
pub struct Listener;

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

/// An interface whose `connect` gives a handle of the same interface.
pub trait Device: Interface {}
impl Device for Uart {}
impl Device for Can {}
impl Device for Udp {}
impl Device for Dir {}
impl Device for RegularFile {}
impl Device for GpioLine {}

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

    /// Closes the handle's descriptor, ending its ops. Later ops on it fail
    /// with `Stale`.
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

impl Handle<Listener> {
    /// The next stream from an allowed peer. Its peer's `RawAddr` fills
    /// `peer`.
    pub fn accept(&self, peer: Buf) -> Pending<StreamOp> {
        self.submit(Op::Accept, peer)
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

/// The capability a system was granted: a way to reach a resource. Each
/// `connect` or `listen` opens a new handle to it.
pub struct Cap<K: Interface>(Handle<K>);

impl<K: Interface> Clone for Cap<K> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<K: Interface> Cap<K> {
    pub(crate) fn new(handle: Handle<K>) -> Self {
        Self(handle)
    }

    pub fn raw(&self) -> RawHandle {
        self.0.raw
    }

    /// How many ops this capability's system may have in flight at once.
    pub fn budget(&self) -> usize {
        self.0.budget()
    }

    /// Ends this capability until its system's next make. Later ops on it
    /// fail with `Stale`; handles it opened stay open.
    pub fn close(&self) -> Pending<CloseOp> {
        self.0.close()
    }

    /// Lands `Ok` once `after` has passed on the cycle's clock, as
    /// [`Handle::timeout`] does.
    pub fn timeout(&self, after: core::time::Duration) -> Pending<TimeoutOp> {
        self.0.timeout(after)
    }
}

impl<K: Device> Cap<K> {
    /// Opens a new connection to the resource.
    pub fn connect(&self) -> Pending<ConnectOp<K>> {
        self.0.submit(Op::Connect, ())
    }
}

impl Cap<Endpoint> {
    /// Dials the resource's address. The stream's peer `RawAddr` fills
    /// `peer`.
    pub fn connect(&self, peer: Buf) -> Pending<StreamOp> {
        self.0.submit(Op::Connect, peer)
    }

    /// Binds and listens on the resource's address.
    pub fn listen(&self) -> Pending<ListenOp> {
        self.0.submit(Op::Listen, ())
    }
}
