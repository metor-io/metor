//! `Pending<O>`, an op in flight, and the markers that turn its completion
//! into a typed result.

use core::future::Future;
use core::marker::PhantomData;
use core::pin::Pin;
use core::task::{Context, Poll};

use super::cap::{File, Handle, Stream};
use super::queue::Io;
use super::slot::Buf;
use super::{IoError, IoStatus, Op, Out, RawHandle, Token};

mod sealed {
    pub trait Sealed {}
}

/// How an op's completion becomes the value its `Pending` resolves to.
pub trait IoOp: sealed::Sealed {
    type Output;

    #[doc(hidden)]
    fn finish(io: &Io, status: IoStatus, out: Out, buf: Option<Buf>) -> Self::Output;
}

/// An op that lends a buffer and hands it back with its result.
pub trait BufOp: IoOp<Output = (Result<Self::Ok, IoError>, Buf)> {
    type Ok;
}

pub(crate) enum Inner {
    Queued(Token),
    /// Finished without reaching the queue, such as a `QueueFull`.
    Local(IoStatus, Option<Buf>),
    Taken,
}

/// An op in flight.
///
/// Poll it once per cycle, or await it in a sequence. Dropping it leaves the
/// op running and discards its result; [`Pending::cancel`] cancels it.
#[must_use]
pub struct Pending<O: IoOp> {
    io: Io,
    raw: RawHandle,
    inner: Inner,
    _o: PhantomData<fn() -> O>,
}

impl<O: IoOp> Pending<O> {
    pub(crate) fn new(io: &Io, raw: RawHandle, op: Op, buf: Option<Buf>) -> Self {
        Self {
            inner: io.submit(raw, op, buf),
            io: io.clone(),
            raw,
            _o: PhantomData,
        }
    }

    /// The result, once the op has completed. Returns `None` before that and
    /// after the result was taken.
    pub fn poll(&mut self) -> Option<O::Output> {
        let (status, out, buf) = match core::mem::replace(&mut self.inner, Inner::Taken) {
            Inner::Queued(token) => match self.io.take(token) {
                Some(done) => done,
                None => {
                    self.inner = Inner::Queued(token);
                    return None;
                }
            },
            Inner::Local(status, buf) => (status, Out::UNIT, buf),
            Inner::Taken => return None,
        };
        Some(O::finish(&self.io, status, out, buf))
    }

    /// The result if the op has completed, or the op back.
    pub fn try_take(mut self) -> Result<O::Output, Self> {
        self.poll().ok_or(self)
    }

    /// Cancels the op. Its completion still arrives, usually `Canceled`.
    ///
    /// The cancel needs no slot of its own, so it goes out even when every
    /// slot is in use.
    pub fn cancel(&self) {
        if let Inner::Queued(token) = self.inner {
            self.io.untracked(self.raw, Op::cancel(token));
        }
    }
}

/// Resolves once the op has completed. Registers no waker: a sequence's step
/// polls it every cycle.
impl<O: IoOp> Future for Pending<O> {
    type Output = O::Output;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<O::Output> {
        match Pending::poll(self.get_mut()) {
            Some(out) => Poll::Ready(out),
            None => Poll::Pending,
        }
    }
}

impl<O: IoOp> Drop for Pending<O> {
    fn drop(&mut self) {
        if let Inner::Queued(token) = self.inner {
            self.io.orphan(token);
        }
    }
}

/// The buffer back, with the length a read or write reports.
fn with_len(
    status: IoStatus,
    out: Out,
    buf: Option<Buf>,
    fill: bool,
) -> (Result<usize, IoError>, Buf) {
    let mut buf = buf.unwrap_or_else(Buf::empty);
    let result = status.result().and_then(|()| match out.as_len() {
        Some(n) => Ok(n as usize),
        None => Err(IoError::Device),
    });
    if let (Ok(n), true) = (result, fill) {
        buf.set_len(n);
    }
    (result, buf)
}

fn unit(status: IoStatus, out: Out) -> Result<(), IoError> {
    status.result()?;
    match out {
        Out::Unit { .. } => Ok(()),
        _ => Err(IoError::Device),
    }
}

fn handle<K>(io: &Io, status: IoStatus, out: Out) -> Result<Handle<K>, IoError> {
    status.result()?;
    let raw = out.as_handle().ok_or(IoError::Device)?;
    Ok(Handle::new(raw, io.clone()))
}

fn gpio(status: IoStatus, out: Out) -> Result<u32, IoError> {
    status.result()?;
    match out {
        Out::Len { .. } => Ok(out.as_len().unwrap_or(0)),
        Out::Unit { .. } => Ok(0),
        _ => Err(IoError::Device),
    }
}

/// Fills a buffer; resolves to the bytes read.
pub struct Read;

impl sealed::Sealed for Read {}

impl IoOp for Read {
    type Output = (Result<usize, IoError>, Buf);

    fn finish(
        _io: &Io,
        status: IoStatus,
        out: Out,
        buf: Option<Buf>,
    ) -> (Result<usize, IoError>, Buf) {
        with_len(status, out, buf, true)
    }
}

/// Sends a buffer's bytes; resolves to the bytes written.
pub struct Write;

impl sealed::Sealed for Write {}

impl IoOp for Write {
    type Output = (Result<usize, IoError>, Buf);

    fn finish(
        _io: &Io,
        status: IoStatus,
        out: Out,
        buf: Option<Buf>,
    ) -> (Result<usize, IoError>, Buf) {
        with_len(status, out, buf, false)
    }
}

/// Receives one message into a buffer.
pub struct Recv;

impl sealed::Sealed for Recv {}

impl IoOp for Recv {
    type Output = (Result<usize, IoError>, Buf);

    fn finish(
        _io: &Io,
        status: IoStatus,
        out: Out,
        buf: Option<Buf>,
    ) -> (Result<usize, IoError>, Buf) {
        with_len(status, out, buf, true)
    }
}

/// Sends one message from a buffer.
pub struct Send;

impl sealed::Sealed for Send {}

impl IoOp for Send {
    type Output = (Result<usize, IoError>, Buf);

    fn finish(
        _io: &Io,
        status: IoStatus,
        out: Out,
        buf: Option<Buf>,
    ) -> (Result<usize, IoError>, Buf) {
        with_len(status, out, buf, false)
    }
}

/// Opens a file under a directory.
pub struct Open;

impl sealed::Sealed for Open {}

impl IoOp for Open {
    type Output = Result<Handle<File>, IoError>;

    fn finish(
        io: &Io,
        status: IoStatus,
        out: Out,
        _buf: Option<Buf>,
    ) -> Result<Handle<File>, IoError> {
        handle(io, status, out)
    }
}

/// Accepts a connection on a listener.
pub struct Accept;

impl sealed::Sealed for Accept {}

impl IoOp for Accept {
    type Output = Result<Handle<Stream>, IoError>;

    fn finish(
        io: &Io,
        status: IoStatus,
        out: Out,
        _buf: Option<Buf>,
    ) -> Result<Handle<Stream>, IoError> {
        handle(io, status, out)
    }
}

/// Closes a handle.
pub struct Close;

impl sealed::Sealed for Close {}

impl IoOp for Close {
    type Output = Result<(), IoError>;

    fn finish(_io: &Io, status: IoStatus, out: Out, _buf: Option<Buf>) -> Result<(), IoError> {
        unit(status, out)
    }
}

/// Flushes a file to storage.
pub struct Fsync;

impl sealed::Sealed for Fsync {}

impl IoOp for Fsync {
    type Output = Result<(), IoError>;

    fn finish(_io: &Io, status: IoStatus, out: Out, _buf: Option<Buf>) -> Result<(), IoError> {
        unit(status, out)
    }
}

/// Configures or flushes a serial port.
pub struct Tty;

impl sealed::Sealed for Tty {}

impl IoOp for Tty {
    type Output = Result<(), IoError>;

    fn finish(_io: &Io, status: IoStatus, out: Out, _buf: Option<Buf>) -> Result<(), IoError> {
        unit(status, out)
    }
}

/// Reads or sets a GPIO line; resolves to the line's value.
pub struct Gpio;

impl sealed::Sealed for Gpio {}

impl IoOp for Gpio {
    type Output = Result<u32, IoError>;

    fn finish(_io: &Io, status: IoStatus, out: Out, _buf: Option<Buf>) -> Result<u32, IoError> {
        gpio(status, out)
    }
}

/// Any op, with its raw completion.
pub struct Raw;

impl sealed::Sealed for Raw {}

impl IoOp for Raw {
    type Output = (IoStatus, Out, Option<Buf>);

    fn finish(
        _io: &Io,
        status: IoStatus,
        out: Out,
        buf: Option<Buf>,
    ) -> (IoStatus, Out, Option<Buf>) {
        (status, out, buf)
    }
}

impl BufOp for Read {
    type Ok = usize;
}

impl BufOp for Write {
    type Ok = usize;
}

impl BufOp for Recv {
    type Ok = usize;
}

impl BufOp for Send {
    type Ok = usize;
}
