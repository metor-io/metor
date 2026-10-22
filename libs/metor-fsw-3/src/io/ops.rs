//! The op types a `Pending` resolves to, and how each turns a completion
//! into its result.

use super::arena::Buf;
use super::cap::{Handle, RegularFile, Stream};
use super::queue::Io;
use super::{IoError, IoStatus, RawAddr, RawHandle};

mod sealed {
    pub trait Sealed {}
}

/// How an op's completion becomes the value its `Pending` resolves to.
pub trait IoOp: sealed::Sealed {
    /// What the op holds while it runs.
    type Lent: Lent;
    type Output;

    #[doc(hidden)]
    fn finish(io: &Io, status: IoStatus, value: u32, lent: Self::Lent) -> Self::Output;
}

/// What an op holds while it runs: a `Buf`, nothing, or, for a raw op,
/// either.
pub trait Lent: sealed::Sealed + Unpin {
    /// The buffer the op names, if any.
    fn buf(&self) -> Option<&Buf>;
}

impl sealed::Sealed for Buf {}

impl Lent for Buf {
    fn buf(&self) -> Option<&Buf> {
        Some(self)
    }
}

impl sealed::Sealed for () {}

impl Lent for () {
    fn buf(&self) -> Option<&Buf> {
        None
    }
}

impl sealed::Sealed for Option<Buf> {}

impl Lent for Option<Buf> {
    fn buf(&self) -> Option<&Buf> {
        self.as_ref()
    }
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
/// Opens an endpoint's next stream; resolves to it, with the peer's
/// `RawAddr` in the buffer.
pub struct StreamOp;
/// Closes a handle.
pub struct CloseOp;
/// Flushes a file to storage.
pub struct FsyncOp;
/// Configures or flushes a serial port.
pub struct TtyOp;
/// Reads or sets a GPIO line; resolves to the line's level.
pub struct GpioOp;
/// Lands once its time has passed.
pub struct TimeoutOp;
/// Any op, with its raw result.
pub struct RawOp;

/// The buffer back, with the length the op reports. A fill makes that many
/// bytes from the span's start the filled ones.
fn moved(status: IoStatus, value: u32, mut buf: Buf, fill: bool) -> (Result<usize, IoError>, Buf) {
    let result = status.result().map(|()| value as usize);
    if let (Ok(n), true) = (result, fill) {
        buf.set_filled(n);
    }
    (result, buf)
}

fn opened<K>(io: &Io, status: IoStatus, value: u32) -> Result<Handle<K>, IoError> {
    status.result()?;
    Ok(Handle::new(RawHandle::from_raw(value), io.clone()))
}

impl sealed::Sealed for ReadOp {}

impl IoOp for ReadOp {
    type Lent = Buf;
    type Output = (Result<usize, IoError>, Buf);

    fn finish(_io: &Io, status: IoStatus, value: u32, buf: Buf) -> Self::Output {
        moved(status, value, buf, true)
    }
}

impl sealed::Sealed for RecvOp {}

impl IoOp for RecvOp {
    type Lent = Buf;
    type Output = (Result<usize, IoError>, Buf);

    fn finish(_io: &Io, status: IoStatus, value: u32, buf: Buf) -> Self::Output {
        moved(status, value, buf, true)
    }
}

impl sealed::Sealed for WriteOp {}

impl IoOp for WriteOp {
    type Lent = Buf;
    type Output = (Result<usize, IoError>, Buf);

    fn finish(_io: &Io, status: IoStatus, value: u32, buf: Buf) -> Self::Output {
        moved(status, value, buf, false)
    }
}

impl sealed::Sealed for SendOp {}

impl IoOp for SendOp {
    type Lent = Buf;
    type Output = (Result<usize, IoError>, Buf);

    fn finish(_io: &Io, status: IoStatus, value: u32, buf: Buf) -> Self::Output {
        moved(status, value, buf, false)
    }
}

impl sealed::Sealed for OpenOp {}

impl IoOp for OpenOp {
    type Lent = Buf;
    type Output = Result<Handle<RegularFile>, IoError>;

    fn finish(io: &Io, status: IoStatus, value: u32, _path: Buf) -> Self::Output {
        opened(io, status, value)
    }
}

impl sealed::Sealed for StreamOp {}

impl IoOp for StreamOp {
    type Lent = Buf;
    type Output = (Result<Handle<Stream>, IoError>, Buf);

    fn finish(io: &Io, status: IoStatus, value: u32, mut peer: Buf) -> Self::Output {
        let stream = opened(io, status, value);
        if stream.is_ok() {
            peer.set_filled(RawAddr::LEN);
        }
        (stream, peer)
    }
}

impl sealed::Sealed for CloseOp {}

impl IoOp for CloseOp {
    type Lent = ();
    type Output = Result<(), IoError>;

    fn finish(_io: &Io, status: IoStatus, _value: u32, (): ()) -> Self::Output {
        status.result()
    }
}

impl sealed::Sealed for FsyncOp {}

impl IoOp for FsyncOp {
    type Lent = ();
    type Output = Result<(), IoError>;

    fn finish(_io: &Io, status: IoStatus, _value: u32, (): ()) -> Self::Output {
        status.result()
    }
}

impl sealed::Sealed for TtyOp {}

impl IoOp for TtyOp {
    type Lent = ();
    type Output = Result<(), IoError>;

    fn finish(_io: &Io, status: IoStatus, _value: u32, (): ()) -> Self::Output {
        status.result()
    }
}

impl sealed::Sealed for GpioOp {}

impl IoOp for GpioOp {
    type Lent = ();
    type Output = Result<u32, IoError>;

    fn finish(_io: &Io, status: IoStatus, value: u32, (): ()) -> Self::Output {
        status.result().map(|()| value)
    }
}

impl sealed::Sealed for TimeoutOp {}

impl IoOp for TimeoutOp {
    type Lent = ();
    type Output = Result<(), IoError>;

    fn finish(_io: &Io, status: IoStatus, _value: u32, (): ()) -> Self::Output {
        status.result()
    }
}

impl sealed::Sealed for RawOp {}

impl IoOp for RawOp {
    type Lent = Option<Buf>;
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
impl BufOp for StreamOp {
    type Ok = Handle<Stream>;
}
