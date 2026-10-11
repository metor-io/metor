use zerocopy::little_endian::U32;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

/// Where framework error codes start. Codes below it are Linux errnos.
const FRAMEWORK: u32 = 0x1_0000;

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
        let code = match e {
            IoError::Os(Errno(0)) => u32::from(Errno::EIO.0),
            IoError::Os(errno) => u32::from(errno.0),
            IoError::QueueFull => FRAMEWORK,
            IoError::Stale => FRAMEWORK + 1,
            IoError::Canceled => FRAMEWORK + 2,
            IoError::Refused => FRAMEWORK + 3,
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
            Some(5) => IoError::Unsupported,
            Some(6) => IoError::BadBuf,
            Some(7) => IoError::BadAddr,
            Some(_) => IoError::Refused,
        };
        Err(e)
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
pub struct Errno(pub(crate) u16);

macro_rules! errnos {
    ($($name:ident = $n:literal),* $(,)?) => {
        impl Errno {
            $(pub const $name: Self = Self($n);)*
            const NAMED: &[(Self, &'static str)] = &[$((Self::$name, stringify!($name))),*];
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
    pub const fn new(n: u16) -> Self {
        Self(n)
    }

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
