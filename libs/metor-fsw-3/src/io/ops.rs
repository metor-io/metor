//! The op types a `Pending` resolves to, and how each turns a completion
//! into its result.

use super::arena::Buf;
use super::cap::{Handle, RegularFile, Stream};
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
pub struct ReadOp;
/// Sends a buffer's bytes; resolves to the bytes written.
pub struct WriteOp;
/// Receives one message into a buffer.
pub struct RecvOp;
/// Sends one message from a buffer.
pub struct SendOp;
/// Opens a file under a directory.
pub struct OpenOp;
/// Accepts a connection on a listener.
pub struct AcceptOp;
/// Closes a handle.
pub struct CloseOp;
/// Flushes a file to storage.
pub struct FsyncOp;
/// Configures or flushes a serial port.
pub struct TtyOp;
/// Reads or sets a GPIO line; resolves to the line's level.
pub struct GpioOp;
/// Any op, with its raw result.
pub struct RawOp;

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

impl sealed::Sealed for ReadOp {}

impl IoOp for ReadOp {
    type Output = (Result<usize, IoError>, Buf);

    fn finish(_io: &Io, status: IoStatus, value: u32, buf: Option<Buf>) -> Self::Output {
        moved(status, value, buf, true)
    }
}

impl sealed::Sealed for RecvOp {}

impl IoOp for RecvOp {
    type Output = (Result<usize, IoError>, Buf);

    fn finish(_io: &Io, status: IoStatus, value: u32, buf: Option<Buf>) -> Self::Output {
        moved(status, value, buf, true)
    }
}

impl sealed::Sealed for WriteOp {}

impl IoOp for WriteOp {
    type Output = (Result<usize, IoError>, Buf);

    fn finish(_io: &Io, status: IoStatus, value: u32, buf: Option<Buf>) -> Self::Output {
        moved(status, value, buf, false)
    }
}

impl sealed::Sealed for SendOp {}

impl IoOp for SendOp {
    type Output = (Result<usize, IoError>, Buf);

    fn finish(_io: &Io, status: IoStatus, value: u32, buf: Option<Buf>) -> Self::Output {
        moved(status, value, buf, false)
    }
}

impl sealed::Sealed for OpenOp {}

impl IoOp for OpenOp {
    type Output = Result<Handle<RegularFile>, IoError>;

    fn finish(io: &Io, status: IoStatus, value: u32, _buf: Option<Buf>) -> Self::Output {
        opened(io, status, value)
    }
}

impl sealed::Sealed for AcceptOp {}

impl IoOp for AcceptOp {
    type Output = Result<Handle<Stream>, IoError>;

    fn finish(io: &Io, status: IoStatus, value: u32, _buf: Option<Buf>) -> Self::Output {
        opened(io, status, value)
    }
}

impl sealed::Sealed for CloseOp {}

impl IoOp for CloseOp {
    type Output = Result<(), IoError>;

    fn finish(_io: &Io, status: IoStatus, _value: u32, _buf: Option<Buf>) -> Self::Output {
        status.result()
    }
}

impl sealed::Sealed for FsyncOp {}

impl IoOp for FsyncOp {
    type Output = Result<(), IoError>;

    fn finish(_io: &Io, status: IoStatus, _value: u32, _buf: Option<Buf>) -> Self::Output {
        status.result()
    }
}

impl sealed::Sealed for TtyOp {}

impl IoOp for TtyOp {
    type Output = Result<(), IoError>;

    fn finish(_io: &Io, status: IoStatus, _value: u32, _buf: Option<Buf>) -> Self::Output {
        status.result()
    }
}

impl sealed::Sealed for GpioOp {}

impl IoOp for GpioOp {
    type Output = Result<u32, IoError>;

    fn finish(_io: &Io, status: IoStatus, value: u32, _buf: Option<Buf>) -> Self::Output {
        status.result().map(|()| value)
    }
}

impl sealed::Sealed for RawOp {}

impl IoOp for RawOp {
    type Output = (Result<u32, IoError>, Option<Buf>);

    fn finish(_io: &Io, status: IoStatus, value: u32, buf: Option<Buf>) -> Self::Output {
        (status.result().map(|()| value), buf)
    }
}

impl BufOp for ReadOp {
    type Ok = usize;
}
impl BufOp for RecvOp {
    type Ok = usize;
}
impl BufOp for WriteOp {
    type Ok = usize;
}
impl BufOp for SendOp {
    type Ok = usize;
}
