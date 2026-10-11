//! `Sqe` and `Cqe`, the records on a system's queue pair, and `Op`, the
//! decoded form of an `Sqe`.

use core::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

use zerocopy::little_endian::{U16, U32, U64};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, TryFromBytes, Unaligned};

use super::{IoError, IoStatus, MAX_OPS};
use crate::record::{DecodeError, EncodeError, MsgCodec, Record, RecordSchema};

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

/// A slot in a system's op table, and the generation that claimed it. A
/// completion carries its op's token.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, IntoBytes, FromBytes, KnownLayout, Immutable, Unaligned,
)]
#[repr(transparent)]
pub struct Token(U32);

/// An index into a system's handle table, and the generation that issued
/// it.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, IntoBytes, FromBytes, KnownLayout, Immutable, Unaligned,
)]
#[repr(transparent)]
pub struct RawHandle(U32);

impl Token {
    pub const fn new(slot: u16, generation: u16) -> Self {
        Self(U32::new(slot as u32 | (generation as u32) << 16))
    }

    pub fn slot(self) -> u16 {
        self.0.get() as u16
    }

    pub fn generation(self) -> u16 {
        (self.0.get() >> 16) as u16
    }
}

impl RawHandle {
    pub const fn new(index: u16, generation: u16) -> Self {
        Self(U32::new(index as u32 | (generation as u32) << 16))
    }

    pub fn index(self) -> u16 {
        self.0.get() as u16
    }

    pub fn generation(self) -> u16 {
        (self.0.get() >> 16) as u16
    }

    pub fn get(self) -> u32 {
        self.0.get()
    }

    pub fn from_raw(raw: u32) -> Self {
        Self(U32::new(raw))
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
    Accept,
    TtyConfigure,
    TtyFlush,
    GpioGet,
    GpioSet,
    Cancel,
}

impl TryFrom<u8> for OpKind {
    type Error = IoError;

    fn try_from(code: u8) -> Result<Self, IoError> {
        Self::try_read_from_bytes(&[code]).map_err(|_| IoError::Unsupported)
    }
}

/// Which of a serial port's queues a flush discards.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TtyQueue {
    Input,
    Output,
    Both,
}

/// Serial port settings, applied with `tcsetattr`.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, IntoBytes, FromBytes, KnownLayout, Immutable, Unaligned,
)]
#[repr(C)]
pub struct TtyConfig {
    baud: U32,
    pub data_bits: u8,
    /// 0 none, 1 odd, 2 even.
    pub parity: u8,
    pub stop_bits: u8,
    /// 0 none, 1 RTS/CTS.
    pub flow: u8,
    pub vmin: u8,
    pub vtime: u8,
}

impl TtyConfig {
    /// 8N1 with no flow control.
    pub const fn new(baud: u32) -> Self {
        Self {
            baud: U32::new(baud),
            data_bits: 8,
            parity: 0,
            stop_bits: 1,
            flow: 0,
            vmin: 0,
            vtime: 0,
        }
    }

    pub fn baud(&self) -> u32 {
        self.baud.get()
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

    /// Sets the permission bits of a created file. Zero means `0o644`.
    pub fn with_mode(mut self, mode: u32) -> Self {
        self.mode = U32::new(mode);
        self
    }

    pub fn contains(self, other: Self) -> bool {
        self.bits.get() & other.bits.get() == other.bits.get()
    }

    pub fn mode(self) -> u32 {
        self.mode.get()
    }
}

impl core::ops::BitOr for OpenFlags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self {
            bits: U32::new(self.bits.get() | rhs.bits.get()),
            mode: U32::new(self.mode.get() | rhs.mode.get()),
        }
    }
}

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
    Accept,
    TtyConfigure(TtyConfig),
    TtyFlush(TtyQueue),
    GpioGet,
    GpioSet(bool),
    /// Cancels the op whose token the `Sqe` carries.
    Cancel,
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
            Self::Accept => OpKind::Accept,
            Self::TtyConfigure(_) => OpKind::TtyConfigure,
            Self::TtyFlush(_) => OpKind::TtyFlush,
            Self::GpioGet => OpKind::GpioGet,
            Self::GpioSet(_) => OpKind::GpioSet,
            Self::Cancel => OpKind::Cancel,
        }
    }

    /// Whether the op writes into its buffer, up to its capacity.
    pub fn fills(&self) -> bool {
        matches!(self, Self::Read { .. } | Self::Recv)
    }

    /// Whether the op reads its buffer's filled bytes.
    pub fn drains(&self) -> bool {
        matches!(
            self,
            Self::Write { .. } | Self::Send | Self::SendTo(_) | Self::Open(_)
        )
    }

    /// Whether a success completes with a new handle.
    pub fn opens(&self) -> bool {
        matches!(self, Self::Open(_) | Self::Accept)
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
    /// `u64::MAX` uses the current file position.
    pub offset: U64,
    /// A `TtyConfig`, `OpenFlags`, or `RawAddr`.
    pub arg: [u8; 28],
}

impl Sqe {
    pub fn new(token: Token, handle: RawHandle, op: &Op, buf: Span) -> Self {
        let mut sqe = Self {
            token,
            handle,
            opcode: op.kind() as u8,
            aux: 0,
            _p: [0; 2],
            buf,
            offset: U64::new(u64::MAX),
            arg: [0; 28],
        };
        match *op {
            Op::Read { offset } | Op::Write { offset } => {
                sqe.offset = U64::new(offset.unwrap_or(u64::MAX));
            }
            Op::SendTo(addr) => put(&mut sqe.arg, &RawAddr::from(addr)),
            Op::Open(flags) => put(&mut sqe.arg, &flags),
            Op::TtyConfigure(cfg) => put(&mut sqe.arg, &cfg),
            Op::TtyFlush(queue) => sqe.aux = queue as u8,
            Op::GpioSet(level) => sqe.aux = u8::from(level),
            _ => {}
        }
        sqe
    }

    /// Decodes the op. An unknown code is `Unsupported`.
    pub fn op(&self) -> Result<Op, IoError> {
        let offset = Some(self.offset.get()).filter(|o| *o != u64::MAX);
        Ok(match OpKind::try_from(self.opcode)? {
            OpKind::Read => Op::Read { offset },
            OpKind::Write => Op::Write { offset },
            OpKind::Recv => Op::Recv,
            OpKind::Send => Op::Send,
            OpKind::SendTo => Op::SendTo(take::<RawAddr>(&self.arg).try_into()?),
            OpKind::Open => Op::Open(take(&self.arg)),
            OpKind::Close => Op::Close,
            OpKind::Fsync => Op::Fsync,
            OpKind::Accept => Op::Accept,
            OpKind::TtyConfigure => Op::TtyConfigure(take(&self.arg)),
            OpKind::TtyFlush => Op::TtyFlush(match self.aux {
                0 => TtyQueue::Input,
                1 => TtyQueue::Output,
                2 => TtyQueue::Both,
                _ => return Err(IoError::Unsupported),
            }),
            OpKind::GpioGet => Op::GpioGet,
            OpKind::GpioSet => Op::GpioSet(self.aux != 0),
            OpKind::Cancel => Op::Cancel,
        })
    }
}

fn put<T: IntoBytes + Immutable>(arg: &mut [u8; 28], value: &T) {
    arg[..size_of::<T>()].copy_from_slice(value.as_bytes());
}

fn take<T: FromBytes>(arg: &[u8; 28]) -> T {
    // PANIC Safety: every `T` passed here is at most 28 bytes, asserted below.
    T::read_from_prefix(arg).expect("fits in arg").0
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

impl Record for Sqe {
    const NAME: &'static str = "io_sqe";
    const MAX_LEN: usize = size_of::<Sqe>();
    /// A system's ops in flight, plus a cancel for each.
    const DEPTH: usize = 2 * MAX_OPS;
    type Read<'a> = Sqe;

    fn encode<'a>(&'a self, _buf: &'a mut [u8]) -> Result<&'a [u8], EncodeError> {
        Ok(self.as_bytes())
    }

    fn decode(bytes: &[u8]) -> Result<Sqe, DecodeError> {
        decode(bytes)
    }

    fn schema() -> RecordSchema {
        RecordSchema::msg(Self::NAME, MsgCodec::Other("io.sqe".into()))
    }
}

impl Record for Cqe {
    const NAME: &'static str = "io_cqe";
    const MAX_LEN: usize = size_of::<Cqe>();
    const DEPTH: usize = MAX_OPS;
    type Read<'a> = Cqe;

    fn encode<'a>(&'a self, _buf: &'a mut [u8]) -> Result<&'a [u8], EncodeError> {
        Ok(self.as_bytes())
    }

    fn decode(bytes: &[u8]) -> Result<Cqe, DecodeError> {
        decode(bytes)
    }

    fn schema() -> RecordSchema {
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
