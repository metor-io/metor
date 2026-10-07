use core::ops::BitOr;

use zerocopy::little_endian::{U32, U64};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, TryFromBytes, Unaligned};

use super::{BufRef, RawAddr, RawHandle, Token};

/// One op, as a system submits it.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, IntoBytes, TryFromBytes, KnownLayout, Immutable, Unaligned,
)]
#[repr(C)]
pub struct Sqe {
    pub token: Token,
    pub handle: RawHandle,
    pub op: Op,
    _p: [u8; 3],
}

impl Sqe {
    pub fn new(token: Token, handle: RawHandle, op: Op) -> Self {
        Self {
            token,
            handle,
            op,
            _p: [0; 3],
        }
    }
}

/// What an op does. Every variant is 32 bytes, padded by hand.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, IntoBytes, TryFromBytes, KnownLayout, Immutable, Unaligned,
)]
#[repr(u8)]
pub enum Op {
    /// An offset of `u64::MAX` reads at the current position.
    Read {
        buf: BufRef,
        _p: [u8; 4],
        offset: U64,
        _q: [u8; 16],
    },
    Write {
        buf: BufRef,
        _p: [u8; 4],
        offset: U64,
        _q: [u8; 16],
    },
    Recv {
        buf: BufRef,
        _p: [u8; 28],
    },
    Send {
        buf: BufRef,
        _p: [u8; 28],
    },
    SendTo {
        buf: BufRef,
        addr: RawAddr,
    },
    Open {
        path: BufRef,
        flags: OpenFlags,
        _p: [u8; 16],
    },
    Accept {
        _p: [u8; 32],
    },
    Resolve {
        _p: [u8; 32],
    },
    Connect {
        addr: RawAddr,
        _p: [u8; 4],
    },
    Close {
        _p: [u8; 32],
    },
    Fsync {
        _p: [u8; 32],
    },
    Tty {
        op: TtyOp,
        _p: [u8; 20],
    },
    Gpio {
        op: GpioOp,
        _p: [u8; 24],
    },
    Cancel {
        target: Token,
        _p: [u8; 28],
    },
}

impl Op {
    pub const ACCEPT: Self = Self::Accept { _p: [0; 32] };
    pub const RESOLVE: Self = Self::Resolve { _p: [0; 32] };
    pub const CLOSE: Self = Self::Close { _p: [0; 32] };
    pub const FSYNC: Self = Self::Fsync { _p: [0; 32] };

    /// `None` reads at the current position.
    pub fn read(buf: BufRef, offset: Option<u64>) -> Self {
        Self::Read {
            buf,
            _p: [0; 4],
            offset: U64::new(offset.unwrap_or(u64::MAX)),
            _q: [0; 16],
        }
    }

    /// `None` writes at the current position.
    pub fn write(buf: BufRef, offset: Option<u64>) -> Self {
        Self::Write {
            buf,
            _p: [0; 4],
            offset: U64::new(offset.unwrap_or(u64::MAX)),
            _q: [0; 16],
        }
    }

    pub fn recv(buf: BufRef) -> Self {
        Self::Recv { buf, _p: [0; 28] }
    }

    pub fn send(buf: BufRef) -> Self {
        Self::Send { buf, _p: [0; 28] }
    }

    pub fn send_to(buf: BufRef, addr: RawAddr) -> Self {
        Self::SendTo { buf, addr }
    }

    pub fn open(path: BufRef, flags: OpenFlags) -> Self {
        Self::Open {
            path,
            flags,
            _p: [0; 16],
        }
    }

    pub fn connect(addr: RawAddr) -> Self {
        Self::Connect { addr, _p: [0; 4] }
    }

    pub fn tty(op: TtyOp) -> Self {
        Self::Tty { op, _p: [0; 20] }
    }

    pub fn gpio(op: GpioOp) -> Self {
        Self::Gpio { op, _p: [0; 24] }
    }

    pub fn cancel(target: Token) -> Self {
        Self::Cancel {
            target,
            _p: [0; 28],
        }
    }

    pub fn kind(&self) -> OpKind {
        match self {
            Self::Read { .. } => OpKind::Read,
            Self::Write { .. } => OpKind::Write,
            Self::Recv { .. } => OpKind::Recv,
            Self::Send { .. } => OpKind::Send,
            Self::SendTo { .. } => OpKind::SendTo,
            Self::Open { .. } => OpKind::Open,
            Self::Accept { .. } => OpKind::Accept,
            Self::Resolve { .. } => OpKind::Resolve,
            Self::Connect { .. } => OpKind::Connect,
            Self::Close { .. } => OpKind::Close,
            Self::Fsync { .. } => OpKind::Fsync,
            Self::Tty { .. } => OpKind::Tty,
            Self::Gpio { .. } => OpKind::Gpio,
            Self::Cancel { .. } => OpKind::Cancel,
        }
    }

    /// The buffer the op reads or fills, if any.
    pub fn buf(&self) -> Option<BufRef> {
        match *self {
            Self::Read { buf, .. }
            | Self::Write { buf, .. }
            | Self::Recv { buf, .. }
            | Self::Send { buf, .. }
            | Self::SendTo { buf, .. }
            | Self::Open { path: buf, .. } => Some(buf),
            _ => None,
        }
    }

    #[cfg_attr(not(feature = "host"), allow(dead_code))]
    pub(crate) fn buf_mut(&mut self) -> Option<&mut BufRef> {
        match self {
            Self::Read { buf, .. }
            | Self::Write { buf, .. }
            | Self::Recv { buf, .. }
            | Self::Send { buf, .. }
            | Self::SendTo { buf, .. }
            | Self::Open { path: buf, .. } => Some(buf),
            _ => None,
        }
    }

    /// The file offset of a `Read` or `Write`; `None` is the current position.
    pub fn offset(&self) -> Option<u64> {
        match self {
            Self::Read { offset, .. } | Self::Write { offset, .. } => {
                Some(offset.get()).filter(|o| *o != u64::MAX)
            }
            _ => None,
        }
    }
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OpKind {
    Read,
    Write,
    Recv,
    Send,
    SendTo,
    Open,
    Accept,
    Resolve,
    Connect,
    Close,
    Fsync,
    Tty,
    Gpio,
    Cancel,
}

impl OpKind {
    pub const ALL: [OpKind; 14] = [
        Self::Read,
        Self::Write,
        Self::Recv,
        Self::Send,
        Self::SendTo,
        Self::Open,
        Self::Accept,
        Self::Resolve,
        Self::Connect,
        Self::Close,
        Self::Fsync,
        Self::Tty,
        Self::Gpio,
        Self::Cancel,
    ];

    /// The snake-case name Python uses.
    pub fn name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Recv => "recv",
            Self::Send => "send",
            Self::SendTo => "send_to",
            Self::Open => "open",
            Self::Accept => "accept",
            Self::Resolve => "resolve",
            Self::Connect => "connect",
            Self::Close => "close",
            Self::Fsync => "fsync",
            Self::Tty => "tty",
            Self::Gpio => "gpio",
            Self::Cancel => "cancel",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.name() == name)
    }
}

/// Portable open flags; each `Os` translates them.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, IntoBytes, FromBytes, KnownLayout, Immutable, Unaligned,
)]
#[repr(C)]
pub struct OpenFlags {
    bits: U32,
    mode: U32,
    _p: [u8; 4],
}

impl OpenFlags {
    pub const READ: Self = Self::bit(0);
    pub const WRITE: Self = Self::bit(1);
    pub const APPEND: Self = Self::bit(2);
    pub const CREATE: Self = Self::bit(3);
    pub const TRUNCATE: Self = Self::bit(4);
    pub const EXCLUSIVE: Self = Self::bit(5);
    pub const NONBLOCK: Self = Self::bit(6);
    pub const NOCTTY: Self = Self::bit(7);
    pub const DIRECTORY: Self = Self::bit(8);

    const fn bit(n: u32) -> Self {
        Self {
            bits: U32::new(1 << n),
            mode: U32::ZERO,
            _p: [0; 4],
        }
    }

    /// The permission bits a created file gets.
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

impl BitOr for OpenFlags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self {
            bits: U32::new(self.bits.get() | rhs.bits.get()),
            mode: U32::new(self.mode.get() | rhs.mode.get()),
            _p: [0; 4],
        }
    }
}

/// A serial-port setting, applied with `tcsetattr`.
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
    _p: [u8; 1],
}

impl TtyConfig {
    /// 8 data bits, no parity, one stop bit, no flow control.
    pub const fn new(baud: u32) -> Self {
        Self {
            baud: U32::new(baud),
            data_bits: 8,
            parity: 0,
            stop_bits: 1,
            flow: 0,
            vmin: 0,
            vtime: 0,
            _p: [0],
        }
    }

    pub fn baud(&self) -> u32 {
        self.baud.get()
    }
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, IntoBytes, TryFromBytes, KnownLayout, Immutable, Unaligned,
)]
#[repr(u8)]
pub enum TtyOp {
    Configure(TtyConfig),
    /// 0 input, 1 output, 2 both.
    Flush {
        queue: u8,
        _p: [u8; 10],
    },
}

impl TtyOp {
    pub fn flush(queue: u8) -> Self {
        Self::Flush { queue, _p: [0; 10] }
    }
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, IntoBytes, TryFromBytes, KnownLayout, Immutable, Unaligned,
)]
#[repr(u8)]
pub enum GpioOp {
    Get { _p: [u8; 7] },
    Set { value: u8, _p: [u8; 6] },
}

impl GpioOp {
    pub const GET: Self = Self::Get { _p: [0; 7] };

    pub fn set(value: u8) -> Self {
        Self::Set { value, _p: [0; 6] }
    }
}

const _: () = {
    assert!(size_of::<Sqe>() == 48);
    assert!(size_of::<Op>() == 33);
    assert!(size_of::<OpenFlags>() == 12);
    assert!(size_of::<TtyConfig>() == 11);
    assert!(size_of::<TtyOp>() == 12);
    assert!(size_of::<GpioOp>() == 8);
};
