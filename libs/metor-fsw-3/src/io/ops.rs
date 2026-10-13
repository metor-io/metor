//! The op types a `Pending` resolves to, and how each turns a completion
//! into its result.

use super::arena::Buf;
use super::cap::{File, Handle, Stream};
use super::queue::Io;
use super::{IoError, IoStatus, RawHandle};

mod sealed {
    pub trait Sealed {}
}

/// How an op's completion becomes the value its `Pending` resolves to.
pub trait IoOp: sealed::Sealed {
    type Output;

    #[doc(hidden)]
    fn finish(io: &Io, status: IoStatus, value: u32, buf: Option<Buf>) -> Self::Output;
}

/// An op that lends a buffer and hands it back with its result.
pub trait BufOp: IoOp<Output = (Result<Self::Ok, IoError>, Buf)> {
    type Ok;
}

/// Fills a buffer; resolves to the bytes read.
pub struct Read;
/// Sends a buffer's bytes; resolves to the bytes written.
pub struct Write;
/// Receives one message into a buffer.
pub struct Recv;
/// Sends one message from a buffer.
pub struct Send;
/// Opens a file under a directory.
pub struct Open;
/// Accepts a connection on a listener.
pub struct Accept;
/// Closes a handle.
pub struct Close;
/// Flushes a file to storage.
pub struct Fsync;
/// Configures or flushes a serial port.
pub struct Tty;
/// Reads or sets a GPIO line; resolves to the line's level.
pub struct Gpio;
/// Any op, with its raw result.
pub struct Raw;

/// The buffer back, with the length the op reports. A fill sets the
/// buffer's length to it.
fn moved(
    status: IoStatus,
    value: u32,
    buf: Option<Buf>,
    fill: bool,
) -> (Result<usize, IoError>, Buf) {
    let mut buf = buf.unwrap_or_else(Buf::empty);
    let result = status.result().map(|()| value as usize);
    if let (Ok(n), true) = (result, fill) {
        buf.set_len(n);
    }
    (result, buf)
}

fn opened<K>(io: &Io, status: IoStatus, value: u32) -> Result<Handle<K>, IoError> {
    status.result()?;
    Ok(Handle::new(RawHandle::from_raw(value), io.clone()))
}

impl sealed::Sealed for Read {}

impl IoOp for Read {
    type Output = (Result<usize, IoError>, Buf);

    fn finish(_io: &Io, status: IoStatus, value: u32, buf: Option<Buf>) -> Self::Output {
        moved(status, value, buf, true)
    }
}

impl sealed::Sealed for Recv {}

impl IoOp for Recv {
    type Output = (Result<usize, IoError>, Buf);

    fn finish(_io: &Io, status: IoStatus, value: u32, buf: Option<Buf>) -> Self::Output {
        moved(status, value, buf, true)
    }
}

impl sealed::Sealed for Write {}

impl IoOp for Write {
    type Output = (Result<usize, IoError>, Buf);

    fn finish(_io: &Io, status: IoStatus, value: u32, buf: Option<Buf>) -> Self::Output {
        moved(status, value, buf, false)
    }
}

impl sealed::Sealed for Send {}

impl IoOp for Send {
    type Output = (Result<usize, IoError>, Buf);

    fn finish(_io: &Io, status: IoStatus, value: u32, buf: Option<Buf>) -> Self::Output {
        moved(status, value, buf, false)
    }
}

impl sealed::Sealed for Open {}

impl IoOp for Open {
    type Output = Result<Handle<File>, IoError>;

    fn finish(io: &Io, status: IoStatus, value: u32, _buf: Option<Buf>) -> Self::Output {
        opened(io, status, value)
    }
}

impl sealed::Sealed for Accept {}

impl IoOp for Accept {
    type Output = Result<Handle<Stream>, IoError>;

    fn finish(io: &Io, status: IoStatus, value: u32, _buf: Option<Buf>) -> Self::Output {
        opened(io, status, value)
    }
}

impl sealed::Sealed for Close {}

impl IoOp for Close {
    type Output = Result<(), IoError>;

    fn finish(_io: &Io, status: IoStatus, _value: u32, _buf: Option<Buf>) -> Self::Output {
        status.result()
    }
}

impl sealed::Sealed for Fsync {}

impl IoOp for Fsync {
    type Output = Result<(), IoError>;

    fn finish(_io: &Io, status: IoStatus, _value: u32, _buf: Option<Buf>) -> Self::Output {
        status.result()
    }
}

impl sealed::Sealed for Tty {}

impl IoOp for Tty {
    type Output = Result<(), IoError>;

    fn finish(_io: &Io, status: IoStatus, _value: u32, _buf: Option<Buf>) -> Self::Output {
        status.result()
    }
}

impl sealed::Sealed for Gpio {}

impl IoOp for Gpio {
    type Output = Result<u32, IoError>;

    fn finish(_io: &Io, status: IoStatus, value: u32, _buf: Option<Buf>) -> Self::Output {
        status.result().map(|()| value)
    }
}

impl sealed::Sealed for Raw {}

impl IoOp for Raw {
    type Output = (Result<u32, IoError>, Option<Buf>);

    fn finish(_io: &Io, status: IoStatus, value: u32, buf: Option<Buf>) -> Self::Output {
        (status.result().map(|()| value), buf)
    }
}

impl BufOp for Read {
    type Ok = usize;
}
impl BufOp for Recv {
    type Ok = usize;
}
impl BufOp for Write {
    type Ok = usize;
}
impl BufOp for Send {
    type Ok = usize;
}
