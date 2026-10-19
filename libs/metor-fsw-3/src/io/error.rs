use core::num::{NonZeroU16, NonZeroU32};

use zerocopy::little_endian::U32;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, TryFromBytes, Unaligned};

/// An op's status: 0 for success, `1..=0xFFFF` for a Linux errno, and
/// `0x1_0000..` for a framework error.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, IntoBytes, FromBytes, KnownLayout, Immutable, Unaligned,
)]
#[repr(transparent)]
pub struct IoStatus(U32);

impl IoStatus {
    pub const OK: Self = Self(U32::ZERO);

    pub fn err(e: IoError) -> Self {
        Self(U32::new(e.code()))
    }

    /// An unknown framework code reads as `Refused`.
    pub fn result(self) -> Result<(), IoError> {
        let Some(code) = NonZeroU32::new(self.0.get()) else {
            return Ok(());
        };
        Err(match NonZeroU16::try_from(code) {
            Ok(errno) => IoError::Os(Errno(errno)),
            Err(_) => FrameworkError::try_from(code.get()).map_or(IoError::Refused, IoError::from),
        })
    }
}

/// The framework errors, each with a code from `0x1_0000` up.
macro_rules! framework_errors {
    ($first:ident $(, $name:ident)* $(,)?) => {
        #[repr(u32)]
        #[derive(Clone, Copy, Debug, PartialEq, Eq, TryFromBytes, KnownLayout, Immutable)]
        enum FrameworkError {
            $first = 0x1_0000,
            $($name),*
        }

        impl From<FrameworkError> for IoError {
            fn from(e: FrameworkError) -> Self {
                match e {
                    FrameworkError::$first => Self::$first,
                    $(FrameworkError::$name => Self::$name),*
                }
            }
        }

        impl IoError {
            /// The error's status code.
            fn code(self) -> u32 {
                match self {
                    Self::Os(errno) => u32::from(errno.get()),
                    Self::$first => FrameworkError::$first as u32,
                    $(Self::$name => FrameworkError::$name as u32),*
                }
            }
        }
    };
}

framework_errors!(
    QueueFull,
    Stale,
    Canceled,
    Refused,
    Unsupported,
    BadBuf,
    BadAddr
);

impl TryFrom<u32> for FrameworkError {
    type Error = ();

    fn try_from(code: u32) -> Result<Self, ()> {
        Self::try_read_from_bytes(&code.to_ne_bytes()).map_err(drop)
    }
}

impl From<Result<(), IoError>> for IoStatus {
    fn from(r: Result<(), IoError>) -> Self {
        r.map_or_else(Self::err, |()| Self::OK)
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
    #[error("unsupported op")]
    Unsupported,
    #[error("bad buffer")]
    BadBuf,
    #[error("bad address")]
    BadAddr,
}

impl From<IoError> for std::borrow::Cow<'static, str> {
    fn from(e: IoError) -> Self {
        e.to_string().into()
    }
}

/// A Linux error number.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Errno(pub(crate) NonZeroU16);

macro_rules! errnos {
    ($($name:ident = $n:literal),* $(,)?) => {
        impl Errno {
            // PANIC Safety: evaluated at compile time, and every code is nonzero.
            $(pub const $name: Self = Self(NonZeroU16::new($n).expect("nonzero"));)*
            const NAMED: &[(Self, &'static str)] = &[$((Self::$name, stringify!($name))),*];
            /// Each host errno and its Linux one.
            #[cfg(all(feature = "host", unix, not(target_os = "linux")))]
            pub(crate) const HOST: &[(i32, Self)] = &[$((libc::$name, Self::$name)),*];
        }
    };
}

errnos! {
    EPERM = 1, ENOENT = 2, EINTR = 4, EIO = 5, ENXIO = 6, EBADF = 9, EAGAIN = 11,
    ENOMEM = 12, EACCES = 13, EFAULT = 14, EBUSY = 16, EEXIST = 17, ENODEV = 19,
    ENOTDIR = 20, EISDIR = 21, EINVAL = 22, EMFILE = 24, ENOTTY = 25, EFBIG = 27,
    ENOSPC = 28, ESPIPE = 29, EROFS = 30, EPIPE = 32, ENAMETOOLONG = 36, ENOSYS = 38,
    ENOTEMPTY = 39, ELOOP = 40, ETIME = 62, EOVERFLOW = 75, ENOTSOCK = 88,
    EMSGSIZE = 90, EOPNOTSUPP = 95, EAFNOSUPPORT = 97, EADDRINUSE = 98,
    EADDRNOTAVAIL = 99, ENETDOWN = 100, ENETUNREACH = 101, ECONNABORTED = 103,
    ECONNRESET = 104, ENOBUFS = 105, EISCONN = 106, ENOTCONN = 107, ETIMEDOUT = 110,
    ECONNREFUSED = 111, EHOSTUNREACH = 113, EALREADY = 114, EINPROGRESS = 115,
    ECANCELED = 125,
}

impl Errno {
    /// The errno `n`, unless it is 0.
    pub const fn new(n: u16) -> Option<Self> {
        match NonZeroU16::new(n) {
            Some(n) => Some(Self(n)),
            None => None,
        }
    }

    pub fn get(self) -> u16 {
        self.0.get()
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
