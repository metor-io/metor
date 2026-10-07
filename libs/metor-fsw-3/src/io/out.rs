use zerocopy::little_endian::U32;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, TryFromBytes, Unaligned};

use super::{RawAddr, RawHandle, Token};

/// One completion, as a system reads it.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, IntoBytes, TryFromBytes, KnownLayout, Immutable, Unaligned,
)]
#[repr(C)]
pub struct Cqe {
    pub token: Token,
    pub status: IoStatus,
    pub out: Out,
    _p: [u8; 3],
}

impl Cqe {
    pub fn ok(token: Token, out: Out) -> Self {
        Self {
            token,
            status: IoStatus::OK,
            out,
            _p: [0; 3],
        }
    }

    pub fn err(token: Token, e: IoError) -> Self {
        Self {
            token,
            status: IoStatus::err(e),
            out: Out::UNIT,
            _p: [0; 3],
        }
    }
}

/// What a successful op gives back. Every variant is 28 bytes.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, IntoBytes, TryFromBytes, KnownLayout, Immutable, Unaligned,
)]
#[repr(u8)]
pub enum Out {
    Unit { _p: [u8; 28] },
    Len { len: U32, _p: [u8; 24] },
    Handle { handle: RawHandle, _p: [u8; 20] },
    Addr { addr: RawAddr },
}

impl Out {
    pub const UNIT: Self = Self::Unit { _p: [0; 28] };

    pub fn len(n: u32) -> Self {
        Self::Len {
            len: U32::new(n),
            _p: [0; 24],
        }
    }

    pub fn handle(handle: RawHandle) -> Self {
        Self::Handle {
            handle,
            _p: [0; 20],
        }
    }

    pub fn addr(addr: RawAddr) -> Self {
        Self::Addr { addr }
    }

    pub fn as_len(&self) -> Option<u32> {
        match self {
            Self::Len { len, .. } => Some(len.get()),
            _ => None,
        }
    }

    pub fn as_handle(&self) -> Option<RawHandle> {
        match self {
            Self::Handle { handle, .. } => Some(*handle),
            _ => None,
        }
    }

    pub fn as_addr(&self) -> Option<RawAddr> {
        match self {
            Self::Addr { addr } => Some(*addr),
            _ => None,
        }
    }
}

const FRAMEWORK: u32 = 0x1_0000;

/// 0 is success, `1..=0xFFFF` a Linux errno, and `0x1_0000..` a framework code.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, IntoBytes, FromBytes, KnownLayout, Immutable, Unaligned,
)]
#[repr(transparent)]
pub struct IoStatus(U32);

impl IoStatus {
    pub const OK: Self = Self(U32::ZERO);

    pub fn err(e: IoError) -> Self {
        let code = match e {
            IoError::Os(errno) => u32::from(errno.0),
            IoError::QueueFull => FRAMEWORK,
            IoError::Stale => FRAMEWORK + 1,
            IoError::Canceled => FRAMEWORK + 2,
            IoError::Refused => FRAMEWORK + 3,
            IoError::Device => FRAMEWORK + 4,
            IoError::Unsupported => FRAMEWORK + 5,
            IoError::BadBuf => FRAMEWORK + 6,
            IoError::BadAddr => FRAMEWORK + 7,
        };
        Self(U32::new(code))
    }

    /// An unknown framework code reads as `Refused`.
    pub fn result(self) -> Result<(), IoError> {
        let code = self.0.get();
        let e = match code.checked_sub(FRAMEWORK) {
            None if code == 0 => return Ok(()),
            None => IoError::Os(Errno(code as u16)),
            Some(0) => IoError::QueueFull,
            Some(1) => IoError::Stale,
            Some(2) => IoError::Canceled,
            Some(4) => IoError::Device,
            Some(5) => IoError::Unsupported,
            Some(6) => IoError::BadBuf,
            Some(7) => IoError::BadAddr,
            Some(_) => IoError::Refused,
        };
        Err(e)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum IoError {
    #[error("{0}")]
    Os(Errno),
    #[error("queue full")]
    QueueFull,
    #[error("stale handle")]
    Stale,
    #[error("canceled")]
    Canceled,
    #[error("refused")]
    Refused,
    #[error("device error")]
    Device,
    #[error("unsupported op")]
    Unsupported,
    #[error("bad buffer")]
    BadBuf,
    #[error("bad address")]
    BadAddr,
}

/// An error number, in Linux numbering.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Errno(pub(crate) u16);

impl Errno {
    pub const EPERM: Self = Self(1);
    pub const ENOENT: Self = Self(2);
    pub const EINTR: Self = Self(4);
    pub const EIO: Self = Self(5);
    pub const ENXIO: Self = Self(6);
    pub const EBADF: Self = Self(9);
    pub const EAGAIN: Self = Self(11);
    pub const ENOMEM: Self = Self(12);
    pub const EACCES: Self = Self(13);
    pub const EFAULT: Self = Self(14);
    pub const EBUSY: Self = Self(16);
    pub const EEXIST: Self = Self(17);
    pub const ENODEV: Self = Self(19);
    pub const ENOTDIR: Self = Self(20);
    pub const EISDIR: Self = Self(21);
    pub const EINVAL: Self = Self(22);
    pub const EMFILE: Self = Self(24);
    pub const ENOTTY: Self = Self(25);
    pub const EFBIG: Self = Self(27);
    pub const ENOSPC: Self = Self(28);
    pub const ESPIPE: Self = Self(29);
    pub const EROFS: Self = Self(30);
    pub const EPIPE: Self = Self(32);
    pub const ENAMETOOLONG: Self = Self(36);
    pub const ENOSYS: Self = Self(38);
    pub const ENOTEMPTY: Self = Self(39);
    pub const ELOOP: Self = Self(40);
    pub const ETIME: Self = Self(62);
    pub const EOVERFLOW: Self = Self(75);
    pub const ENOTSOCK: Self = Self(88);
    pub const EMSGSIZE: Self = Self(90);
    pub const EOPNOTSUPP: Self = Self(95);
    pub const EAFNOSUPPORT: Self = Self(97);
    pub const EADDRINUSE: Self = Self(98);
    pub const EADDRNOTAVAIL: Self = Self(99);
    pub const ENETDOWN: Self = Self(100);
    pub const ENETUNREACH: Self = Self(101);
    pub const ECONNABORTED: Self = Self(103);
    pub const ECONNRESET: Self = Self(104);
    pub const ENOBUFS: Self = Self(105);
    pub const EISCONN: Self = Self(106);
    pub const ENOTCONN: Self = Self(107);
    pub const ETIMEDOUT: Self = Self(110);
    pub const ECONNREFUSED: Self = Self(111);
    pub const EHOSTUNREACH: Self = Self(113);
    pub const EALREADY: Self = Self(114);
    pub const EINPROGRESS: Self = Self(115);
    pub const ECANCELED: Self = Self(125);

    pub(crate) const NAMED: [(Self, &'static str); 48] = [
        (Self::EPERM, "EPERM"),
        (Self::ENOENT, "ENOENT"),
        (Self::EINTR, "EINTR"),
        (Self::EIO, "EIO"),
        (Self::ENXIO, "ENXIO"),
        (Self::EBADF, "EBADF"),
        (Self::EAGAIN, "EAGAIN"),
        (Self::ENOMEM, "ENOMEM"),
        (Self::EACCES, "EACCES"),
        (Self::EFAULT, "EFAULT"),
        (Self::EBUSY, "EBUSY"),
        (Self::EEXIST, "EEXIST"),
        (Self::ENODEV, "ENODEV"),
        (Self::ENOTDIR, "ENOTDIR"),
        (Self::EISDIR, "EISDIR"),
        (Self::EINVAL, "EINVAL"),
        (Self::EMFILE, "EMFILE"),
        (Self::ENOTTY, "ENOTTY"),
        (Self::EFBIG, "EFBIG"),
        (Self::ENOSPC, "ENOSPC"),
        (Self::ESPIPE, "ESPIPE"),
        (Self::EROFS, "EROFS"),
        (Self::EPIPE, "EPIPE"),
        (Self::ENAMETOOLONG, "ENAMETOOLONG"),
        (Self::ENOSYS, "ENOSYS"),
        (Self::ENOTEMPTY, "ENOTEMPTY"),
        (Self::ELOOP, "ELOOP"),
        (Self::ETIME, "ETIME"),
        (Self::EOVERFLOW, "EOVERFLOW"),
        (Self::ENOTSOCK, "ENOTSOCK"),
        (Self::EMSGSIZE, "EMSGSIZE"),
        (Self::EOPNOTSUPP, "EOPNOTSUPP"),
        (Self::EAFNOSUPPORT, "EAFNOSUPPORT"),
        (Self::EADDRINUSE, "EADDRINUSE"),
        (Self::EADDRNOTAVAIL, "EADDRNOTAVAIL"),
        (Self::ENETDOWN, "ENETDOWN"),
        (Self::ENETUNREACH, "ENETUNREACH"),
        (Self::ECONNABORTED, "ECONNABORTED"),
        (Self::ECONNRESET, "ECONNRESET"),
        (Self::ENOBUFS, "ENOBUFS"),
        (Self::EISCONN, "EISCONN"),
        (Self::ENOTCONN, "ENOTCONN"),
        (Self::ETIMEDOUT, "ETIMEDOUT"),
        (Self::ECONNREFUSED, "ECONNREFUSED"),
        (Self::EHOSTUNREACH, "EHOSTUNREACH"),
        (Self::EALREADY, "EALREADY"),
        (Self::EINPROGRESS, "EINPROGRESS"),
        (Self::ECANCELED, "ECANCELED"),
    ];

    pub fn get(self) -> u16 {
        self.0
    }

    pub fn name(self) -> Option<&'static str> {
        Self::NAMED
            .iter()
            .find(|(e, _)| *e == self)
            .map(|(_, n)| *n)
    }

    /// Parses a name such as `"EIO"`.
    pub fn parse(name: &str) -> Option<Self> {
        Self::NAMED
            .iter()
            .find(|(_, n)| *n == name)
            .map(|(e, _)| *e)
    }
}

impl core::fmt::Debug for Errno {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.name() {
            Some(n) => f.write_str(n),
            None => write!(f, "Errno({})", self.0),
        }
    }
}

impl core::fmt::Display for Errno {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Debug::fmt(self, f)
    }
}

const _: () = {
    assert!(size_of::<Cqe>() == 40);
    assert!(size_of::<Out>() == 29);
};
