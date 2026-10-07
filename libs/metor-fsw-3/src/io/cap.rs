//! Handles to resources, typed by the interface the resource provides.

use core::marker::PhantomData;
use core::ops::Deref;

use super::pending::{Accept, Close, Fsync, Gpio, Open, Pending, Read, Recv, Send, Tty, Write};
use super::queue::Io;
use super::slot::Buf;
use super::{GpioOp, Op, OpenFlags, RawHandle, TtyConfig, TtyOp};

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
pub struct File;
/// A TCP listener.
pub struct Listen;
/// A TCP address to dial.
pub struct Dial;
/// A GPIO line.
pub struct GpioLine;
/// A connected stream, from `Listen::accept` or `Dial::connect`.
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
impl Interface for File {
    const NAME: &'static str = "file";
}
impl Interface for Listen {
    const NAME: &'static str = "listen";
}
impl Interface for Dial {
    const NAME: &'static str = "dial";
}
impl Interface for GpioLine {
    const NAME: &'static str = "gpio_line";
}
impl Interface for Stream {
    const NAME: &'static str = "stream";
}

/// Which of a serial port's queues `flush` discards.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TtyQueue {
    Input,
    Output,
    Both,
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

    fn submit<O: super::pending::IoOp>(&self, op: Op, buf: Option<Buf>) -> Pending<O> {
        Pending::new(&self.io, self.raw, op, buf)
    }

    fn with_buf<O: super::pending::IoOp>(
        &self,
        op: fn(super::BufRef) -> Op,
        buf: Buf,
    ) -> Pending<O> {
        let op = op(buf.buf_ref());
        self.submit(op, Some(buf))
    }

    /// Ends this capability. Later ops on it fail with `Stale`.
    pub fn close(&self) -> Pending<Close> {
        self.submit(Op::CLOSE, None)
    }
}

impl Handle<Uart> {
    /// Reads whatever bytes have arrived, up to the buffer's capacity.
    pub fn read(&self, buf: Buf) -> Pending<Read> {
        self.with_buf(|b| Op::read(b, None), buf)
    }

    pub fn write(&self, buf: Buf) -> Pending<Write> {
        self.with_buf(|b| Op::write(b, None), buf)
    }

    pub fn configure(&self, config: TtyConfig) -> Pending<Tty> {
        self.submit(Op::tty(TtyOp::Configure(config)), None)
    }

    pub fn flush(&self, queue: TtyQueue) -> Pending<Tty> {
        let queue = match queue {
            TtyQueue::Input => 0,
            TtyQueue::Output => 1,
            TtyQueue::Both => 2,
        };
        self.submit(Op::tty(TtyOp::flush(queue)), None)
    }
}

/// Message handles: `Can`, `Udp`, and `Stream`.
pub trait Messages {}
impl Messages for Can {}
impl Messages for Udp {}
impl Messages for Stream {}

impl<K: Messages> Handle<K> {
    pub fn recv(&self, buf: Buf) -> Pending<Recv> {
        self.with_buf(Op::recv, buf)
    }

    pub fn send(&self, buf: Buf) -> Pending<Send> {
        self.with_buf(Op::send, buf)
    }
}

impl Handle<File> {
    pub fn read(&self, buf: Buf, offset: u64) -> Pending<Read> {
        let op = Op::read(buf.buf_ref(), Some(offset));
        self.submit(op, Some(buf))
    }

    pub fn write(&self, buf: Buf, offset: u64) -> Pending<Write> {
        let op = Op::write(buf.buf_ref(), Some(offset));
        self.submit(op, Some(buf))
    }

    pub fn fsync(&self) -> Pending<Fsync> {
        self.submit(Op::FSYNC, None)
    }
}

impl Handle<Dir> {
    /// Opens the file at the path in `path`, relative to this directory.
    pub fn open(&self, path: Buf, flags: OpenFlags) -> Pending<Open> {
        let op = Op::open(path.buf_ref(), flags);
        self.submit(op, Some(path))
    }
}

impl Handle<Listen> {
    pub fn accept(&self) -> Pending<Accept> {
        self.submit(Op::ACCEPT, None)
    }
}

impl Handle<GpioLine> {
    pub fn get(&self) -> Pending<Gpio> {
        self.submit(Op::gpio(GpioOp::GET), None)
    }

    /// Drives the line high for any nonzero `value`.
    pub fn set(&self, value: u32) -> Pending<Gpio> {
        self.submit(Op::gpio(GpioOp::set(u8::from(value != 0))), None)
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
