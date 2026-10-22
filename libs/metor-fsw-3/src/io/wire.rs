//! `Sqe` and `Cqe`, the records on a system's queue pair, and `Op`, the
//! decoded form of an `Sqe`.

use core::marker::PhantomData;
use core::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use core::time::Duration;

use serde::{Deserialize, Serialize};

use zerocopy::little_endian::{U16, U32, U64};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, TryFromBytes, Unaligned};

use super::{DEFAULT_OPS, Errno, IoError, IoStatus};
use crate::record::{DecodeError, EncodeError, MsgCodec, Payload, Record, RecordSchema};

/// Bytes in the submitting system's arena.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    Hash,
    IntoBytes,
    FromBytes,
    KnownLayout,
    Immutable,
    Unaligned,
)]
#[repr(C)]
pub struct Span {
    off: U32,
    len: U32,
}

impl Span {
    pub const fn new(off: u32, len: u32) -> Self {
        Self {
            off: U32::new(off),
            len: U32::new(len),
        }
    }

    pub fn off(self) -> u32 {
        self.off.get()
    }

    pub fn len(self) -> u32 {
        self.len.get()
    }

    pub fn is_empty(self) -> bool {
        self.len() == 0
    }

    pub fn end(self) -> Option<u32> {
        self.off().checked_add(self.len())
    }
}

/// An index into a table, and the generation that claimed it, packed as
/// `index | generation << 16`. `Tag` names the table.
#[derive(IntoBytes, FromBytes, KnownLayout, Immutable, Unaligned)]
#[repr(transparent)]
pub struct Generational<Tag>(U32, PhantomData<Tag>);

/// The table a [`Token`] indexes.
pub enum OpTable {}
/// The table a [`RawHandle`] indexes.
pub enum HandleTable {}

/// A slot in a system's op table. A completion carries its op's token.
pub type Token = Generational<OpTable>;
/// An entry of a system's handle table.
pub type RawHandle = Generational<HandleTable>;

impl<Tag> Generational<Tag> {
    pub const fn new(index: u16, generation: u16) -> Self {
        Self::from_raw(index as u32 | (generation as u32) << 16)
    }

    pub const fn from_raw(raw: u32) -> Self {
        Self(U32::new(raw), PhantomData)
    }

    pub fn index(self) -> u16 {
        self.get() as u16
    }

    pub fn generation(self) -> u16 {
        (self.get() >> 16) as u16
    }

    pub fn get(self) -> u32 {
        self.0.get()
    }

    /// The same index and generation, naming another table.
    pub fn retag<Other>(self) -> Generational<Other> {
        Generational::from_raw(self.get())
    }
}

impl<Tag> Clone for Generational<Tag> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<Tag> Copy for Generational<Tag> {}

impl<Tag> PartialEq for Generational<Tag> {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl<Tag> Eq for Generational<Tag> {}

impl<Tag> core::hash::Hash for Generational<Tag> {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

impl<Tag> core::fmt::Debug for Generational<Tag> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Generational")
            .field("index", &self.index())
            .field("generation", &self.generation())
            .finish()
    }
}

/// An op's code on the wire.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, TryFromBytes, KnownLayout, Immutable)]
pub enum OpKind {
    Read,
    Write,
    Recv,
    Send,
    SendTo,
    Open,
    Close,
    Fsync,
    Stream,
    TtyConfigure,
    TtyFlush,
    GpioGet,
    GpioSet,
    Cancel,
    Timeout,
}

/// `TryFrom<u8>` for a one-byte code, failing with `Unsupported`.
macro_rules! code_from_u8 {
    ($($ty:ty),*) => {$(
        impl TryFrom<u8> for $ty {
            type Error = IoError;

            fn try_from(code: u8) -> Result<Self, IoError> {
                Self::try_read_from_bytes(&[code]).map_err(|_| IoError::Unsupported)
            }
        }
    )*};
}

code_from_u8!(OpKind, TtyQueue);

/// Which of a serial port's queues a flush discards.
#[repr(u8)]
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, TryFromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub enum TtyQueue {
    Input,
    Output,
    Both,
}

/// A serial line rate, in bits per second.
#[repr(u8)]
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    Hash,
    TryFromBytes,
    IntoBytes,
    KnownLayout,
    Immutable,
    Unaligned,
    Serialize,
    Deserialize,
)]
#[serde(try_from = "u32", into = "u32")]
pub enum Baud {
    B1200,
    B2400,
    B4800,
    B9600,
    B19200,
    B38400,
    B57600,
    B115200,
    B230400,
    B460800,
    B921600,
    B1000000,
}

impl Baud {
    const ALL: [Self; 12] = [
        Self::B1200,
        Self::B2400,
        Self::B4800,
        Self::B9600,
        Self::B19200,
        Self::B38400,
        Self::B57600,
        Self::B115200,
        Self::B230400,
        Self::B460800,
        Self::B921600,
        Self::B1000000,
    ];

    pub const fn bits_per_second(self) -> u32 {
        match self {
            Self::B1200 => 1200,
            Self::B2400 => 2400,
            Self::B4800 => 4800,
            Self::B9600 => 9600,
            Self::B19200 => 19200,
            Self::B38400 => 38400,
            Self::B57600 => 57600,
            Self::B115200 => 115_200,
            Self::B230400 => 230_400,
            Self::B460800 => 460_800,
            Self::B921600 => 921_600,
            Self::B1000000 => 1_000_000,
        }
    }
}

/// A rate no [`Baud`] has.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0} baud is not a supported rate")]
pub struct UnsupportedBaud(pub u32);

impl TryFrom<u32> for Baud {
    type Error = UnsupportedBaud;

    fn try_from(bits_per_second: u32) -> Result<Self, UnsupportedBaud> {
        Self::ALL
            .into_iter()
            .find(|b| b.bits_per_second() == bits_per_second)
            .ok_or(UnsupportedBaud(bits_per_second))
    }
}

impl From<Baud> for u32 {
    fn from(baud: Baud) -> u32 {
        baud.bits_per_second()
    }
}

#[repr(u8)]
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, TryFromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub enum DataBits {
    Five = 5,
    Six = 6,
    Seven = 7,
    Eight = 8,
}

#[repr(u8)]
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, TryFromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub enum Parity {
    None,
    Odd,
    Even,
}

#[repr(u8)]
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, TryFromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub enum StopBits {
    One = 1,
    Two = 2,
}

#[repr(u8)]
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, TryFromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub enum Flow {
    None,
    RtsCts,
}

/// Serial port settings, applied with `tcsetattr`.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, TryFromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
#[repr(C)]
pub struct TtyConfig {
    pub baud: Baud,
    pub data_bits: DataBits,
    pub parity: Parity,
    pub stop_bits: StopBits,
    pub flow: Flow,
    pub vmin: u8,
    pub vtime: u8,
}

impl TtyConfig {
    /// 8N1 with no flow control.
    pub const fn new(baud: Baud) -> Self {
        Self {
            baud,
            data_bits: DataBits::Eight,
            parity: Parity::None,
            stop_bits: StopBits::One,
            flow: Flow::None,
            vmin: 0,
            vtime: 0,
        }
    }
}

/// Flags for `Open`. Each backend maps them to its own.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, IntoBytes, FromBytes, KnownLayout, Immutable, Unaligned,
)]
#[repr(C)]
pub struct OpenFlags {
    bits: U32,
    mode: U32,
}

impl OpenFlags {
    pub const READ: Self = Self::bit(0);
    pub const WRITE: Self = Self::bit(1);
    pub const APPEND: Self = Self::bit(2);
    pub const CREATE: Self = Self::bit(3);
    pub const TRUNCATE: Self = Self::bit(4);
    pub const EXCLUSIVE: Self = Self::bit(5);
    pub const DIRECTORY: Self = Self::bit(6);

    const fn bit(n: u32) -> Self {
        Self {
            bits: U32::new(1 << n),
            mode: U32::ZERO,
        }
    }

    /// The permission bits of a created file when no mode is set.
    pub const DEFAULT_MODE: u32 = 0o644;

    /// Sets the permission bits of a created file. Zero means
    /// [`DEFAULT_MODE`](Self::DEFAULT_MODE).
    pub fn with_mode(mut self, mode: u32) -> Self {
        self.mode = U32::new(mode);
        self
    }

    pub fn contains(self, other: Self) -> bool {
        self.bits.get() & other.bits.get() == other.bits.get()
    }

    /// The permission bits of a created file.
    pub fn mode(self) -> u32 {
        match self.mode.get() {
            0 => Self::DEFAULT_MODE,
            mode => mode,
        }
    }
}

/// Joins the flags. The left side's mode wins if both sides set one.
impl core::ops::BitOr for OpenFlags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self {
            bits: U32::new(self.bits.get() | rhs.bits.get()),
            mode: match self.mode.get() {
                0 => rhs.mode,
                _ => self.mode,
            },
        }
    }
}

/// The `Sqe` offset that reads or writes at the file's current position.
///
/// It is io_uring's own convention: an offset of -1.
pub const CURRENT_POSITION: u64 = u64::MAX;

const AF_INET: u16 = 2;
const AF_INET6: u16 = 10;

/// A socket address in a fixed 28 bytes, with a Linux address family.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, IntoBytes, FromBytes, KnownLayout, Immutable, Unaligned,
)]
#[repr(C)]
pub struct RawAddr {
    family: U16,
    port: U16,
    flowinfo: U32,
    ip: [u8; 16],
    scope_id: U32,
}

impl RawAddr {
    pub const LEN: usize = size_of::<Self>();
}

impl From<SocketAddr> for RawAddr {
    fn from(addr: SocketAddr) -> Self {
        let (family, ip, flowinfo, scope_id) = match addr {
            SocketAddr::V4(a) => {
                let mut ip = [0; 16];
                ip[..4].copy_from_slice(&a.ip().octets());
                (AF_INET, ip, 0, 0)
            }
            SocketAddr::V6(a) => (AF_INET6, a.ip().octets(), a.flowinfo(), a.scope_id()),
        };
        Self {
            family: U16::new(family),
            port: U16::new(addr.port()),
            flowinfo: U32::new(flowinfo),
            ip,
            scope_id: U32::new(scope_id),
        }
    }
}

impl TryFrom<RawAddr> for SocketAddr {
    type Error = IoError;

    fn try_from(a: RawAddr) -> Result<Self, IoError> {
        let port = a.port.get();
        match a.family.get() {
            AF_INET => {
                let [o0, o1, o2, o3, ..] = a.ip;
                Ok(SocketAddrV4::new(Ipv4Addr::new(o0, o1, o2, o3), port).into())
            }
            AF_INET6 => {
                let ip = Ipv6Addr::from(a.ip);
                let (flow, scope) = (a.flowinfo.get(), a.scope_id.get());
                Ok(SocketAddrV6::new(ip, port, flow, scope).into())
            }
            _ => Err(IoError::BadAddr),
        }
    }
}

/// An IO operation, decoded from an `Sqe`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// `None` uses the current file position.
    Read {
        offset: Option<u64>,
    },
    Write {
        offset: Option<u64>,
    },
    Recv,
    Send,
    SendTo(SocketAddr),
    /// Opens the path in the op's buffer, under a directory.
    Open(OpenFlags),
    Close,
    Fsync,
    /// Opens the endpoint's next stream, and writes its peer's `RawAddr`
    /// into the op's buffer.
    Stream,
    TtyConfigure(TtyConfig),
    TtyFlush(TtyQueue),
    GpioGet,
    GpioSet(bool),
    /// Cancels the op whose token the `Sqe` carries.
    Cancel,
    /// Lands once this long has passed on the cycle's clock.
    Timeout(Duration),
}

impl Op {
    pub fn kind(&self) -> OpKind {
        match self {
            Self::Read { .. } => OpKind::Read,
            Self::Write { .. } => OpKind::Write,
            Self::Recv => OpKind::Recv,
            Self::Send => OpKind::Send,
            Self::SendTo(_) => OpKind::SendTo,
            Self::Open(_) => OpKind::Open,
            Self::Close => OpKind::Close,
            Self::Fsync => OpKind::Fsync,
            Self::Stream => OpKind::Stream,
            Self::TtyConfigure(_) => OpKind::TtyConfigure,
            Self::TtyFlush(_) => OpKind::TtyFlush,
            Self::GpioGet => OpKind::GpioGet,
            Self::GpioSet(_) => OpKind::GpioSet,
            Self::Cancel => OpKind::Cancel,
            Self::Timeout(_) => OpKind::Timeout,
        }
    }

    /// Whether the op writes into its buffer, up to its capacity.
    pub fn fills(&self) -> bool {
        matches!(self, Self::Read { .. } | Self::Recv | Self::Stream)
    }

    /// The fewest bytes the op's buffer may span.
    pub fn min_buf(&self) -> usize {
        match self {
            Self::Stream => RawAddr::LEN,
            _ => 0,
        }
    }

    /// Whether the op reads its buffer's filled bytes.
    pub fn drains(&self) -> bool {
        matches!(
            self,
            Self::Write { .. } | Self::Send | Self::SendTo(_) | Self::Open(_)
        )
    }

    /// Whether the op sends bytes out of its descriptor.
    pub fn writes(&self) -> bool {
        matches!(self, Self::Write { .. } | Self::Send | Self::SendTo(_))
    }

    /// Whether a success completes with a new handle.
    pub fn opens(&self) -> bool {
        matches!(self, Self::Open(_) | Self::Stream)
    }
}

/// An entry on a submission queue.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, IntoBytes, FromBytes, KnownLayout, Immutable, Unaligned,
)]
#[repr(C)]
pub struct Sqe {
    /// The op's token, or for `Cancel` the target's.
    pub token: Token,
    pub handle: RawHandle,
    pub opcode: u8,
    /// A flush's queue, or a GPIO level.
    pub aux: u8,
    _p: [u8; 2],
    pub buf: Span,
    /// The file offset, or [`CURRENT_POSITION`]; a timeout's microseconds.
    pub offset: U64,
    /// A `TtyConfig`, `OpenFlags`, or `RawAddr`.
    pub arg: [u8; 28],
}

impl Sqe {
    /// The SQ depth a system with `ops` ops in flight needs: an op and a
    /// cancel for each.
    pub const fn depth(ops: u16) -> usize {
        2 * ops as usize
    }

    pub fn new(token: Token, handle: RawHandle, op: &Op, buf: Span) -> Self {
        let mut sqe = Self {
            token,
            handle,
            opcode: op.kind() as u8,
            aux: 0,
            _p: [0; 2],
            buf,
            offset: U64::new(CURRENT_POSITION),
            arg: [0; 28],
        };
        match *op {
            Op::Read { offset } | Op::Write { offset } => {
                sqe.offset = U64::new(offset.unwrap_or(CURRENT_POSITION));
            }
            Op::SendTo(addr) => sqe.set_arg(&RawAddr::from(addr)),
            Op::Open(flags) => sqe.set_arg(&flags),
            Op::TtyConfigure(cfg) => sqe.set_arg(&cfg),
            Op::TtyFlush(queue) => sqe.aux = queue as u8,
            Op::GpioSet(level) => sqe.aux = u8::from(level),
            Op::Timeout(after) => {
                let micros = u64::try_from(after.as_nanos().div_ceil(1000)).unwrap_or(u64::MAX);
                sqe.offset = U64::new(micros);
            }
            _ => {}
        }
        sqe
    }

    /// Decodes the op. An unknown code is `Unsupported`.
    pub fn op(&self) -> Result<Op, IoError> {
        let offset = Some(self.offset.get()).filter(|o| *o != CURRENT_POSITION);
        Ok(match OpKind::try_from(self.opcode)? {
            OpKind::Read => Op::Read { offset },
            OpKind::Write => Op::Write { offset },
            OpKind::Recv => Op::Recv,
            OpKind::Send => Op::Send,
            OpKind::SendTo => Op::SendTo(self.arg::<RawAddr>().try_into()?),
            OpKind::Open => Op::Open(self.arg()),
            OpKind::Close => Op::Close,
            OpKind::Fsync => Op::Fsync,
            OpKind::Stream => Op::Stream,
            OpKind::TtyConfigure => Op::TtyConfigure(self.try_arg()?),
            OpKind::TtyFlush => Op::TtyFlush(self.aux.try_into()?),
            OpKind::GpioGet => Op::GpioGet,
            OpKind::GpioSet => Op::GpioSet(self.aux != 0),
            OpKind::Cancel => Op::Cancel,
            OpKind::Timeout => Op::Timeout(Duration::from_micros(self.offset.get())),
        })
    }

    fn set_arg<T: IntoBytes + Immutable>(&mut self, value: &T) {
        self.arg[..size_of::<T>()].copy_from_slice(value.as_bytes());
    }

    fn arg<T: FromBytes>(&self) -> T {
        // PANIC Safety: every `T` passed here is at most 28 bytes, asserted below.
        T::read_from_prefix(&self.arg).expect("fits in arg").0
    }

    /// The arg as a `T`, or `EINVAL` if its bytes are not one.
    fn try_arg<T: TryFromBytes + KnownLayout + Immutable>(&self) -> Result<T, IoError> {
        T::try_read_from_prefix(&self.arg)
            .map(|(value, _)| value)
            .map_err(|_| IoError::Os(Errno::EINVAL))
    }
}

/// An entry on a completion queue.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, IntoBytes, FromBytes, KnownLayout, Immutable, Unaligned,
)]
#[repr(C)]
pub struct Cqe {
    pub token: Token,
    pub status: IoStatus,
    /// A length, a GPIO level, or a `RawHandle`.
    pub value: U32,
}

impl Cqe {
    pub fn ok(token: Token, value: u32) -> Self {
        Self {
            token,
            status: IoStatus::OK,
            value: U32::new(value),
        }
    }

    pub fn err(token: Token, e: IoError) -> Self {
        Self {
            token,
            status: IoStatus::err(e),
            value: U32::ZERO,
        }
    }

    pub fn result(&self) -> Result<u32, IoError> {
        self.status.result().map(|()| self.value.get())
    }
}

impl Payload for Sqe {
    type Read<'a> = Sqe;

    fn encode<'a>(&'a self, _buf: &'a mut [u8]) -> Result<&'a [u8], EncodeError> {
        Ok(self.as_bytes())
    }

    fn decode(bytes: &[u8]) -> Result<Sqe, DecodeError> {
        decode(bytes)
    }
}

impl Record for Sqe {
    const NAME: &'static str = "io_sqe";
    const MAX_LEN: usize = size_of::<Sqe>();
    const DEPTH: usize = Sqe::depth(DEFAULT_OPS);

    fn schema_with_root(_root: &str) -> RecordSchema {
        RecordSchema::msg(Self::NAME, MsgCodec::Other("io.sqe".into()))
    }
}

impl Payload for Cqe {
    type Read<'a> = Cqe;

    fn encode<'a>(&'a self, _buf: &'a mut [u8]) -> Result<&'a [u8], EncodeError> {
        Ok(self.as_bytes())
    }

    fn decode(bytes: &[u8]) -> Result<Cqe, DecodeError> {
        decode(bytes)
    }
}

impl Record for Cqe {
    const NAME: &'static str = "io_cqe";
    const MAX_LEN: usize = size_of::<Cqe>();
    const DEPTH: usize = DEFAULT_OPS as usize;

    fn schema_with_root(_root: &str) -> RecordSchema {
        RecordSchema::msg(Self::NAME, MsgCodec::Other("io.cqe".into()))
    }
}

/// Reads a `T` from the front of `bytes`, ignoring any bytes after it.
fn decode<T: FromBytes>(bytes: &[u8]) -> Result<T, DecodeError> {
    T::read_from_prefix(bytes)
        .map(|(value, _)| value)
        .map_err(|_| DecodeError::Truncated)
}

const _: () = {
    assert!(size_of::<Sqe>() == 56);
    assert!(size_of::<Cqe>() == 12);
    assert!(size_of::<RawAddr>() <= 28);
    assert!(size_of::<OpenFlags>() <= 28);
    assert!(size_of::<TtyConfig>() <= 28);
};
