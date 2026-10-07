use core::mem::MaybeUninit;
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};

#[cfg(not(target_os = "linux"))]
use super::poll::PollOs;
use super::poll::last_error;
#[cfg(target_os = "linux")]
use super::uring::UringOs;
use super::{
    Cqe, Errno, GpioOp, IoError, Limits, OpenFlags, Out, RawAddr, RawHandle, Sqe, Tables,
    TtyConfig, TtyOp,
};

/// What a registered descriptor is. It picks the path each op takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FdClass {
    Stream,
    Datagram,
    Listener,
    File,
    Dir,
    Tty,
    Can,
    Gpio,
}

impl FdClass {
    /// Ops on these wait for readiness instead of blocking.
    pub(crate) fn polled(self) -> bool {
        matches!(
            self,
            Self::Stream | Self::Datagram | Self::Listener | Self::Tty | Self::Can
        )
    }

    pub(crate) fn socket(self) -> bool {
        matches!(self, Self::Stream | Self::Datagram | Self::Listener)
    }
}

/// Executes ops against the host. Nothing here blocks.
pub trait Os {
    /// Queues an op. A full queue comes back from `reap` as `QueueFull`.
    fn push(&mut self, sqe: Sqe);
    /// Hands queued ops to the kernel.
    fn submit(&mut self);
    /// Takes every completion that has landed.
    fn reap(&mut self, done: &mut Vec<Cqe>);
    /// Takes ownership of an opened resource; ops name the handle it returns.
    fn register(&mut self, fd: OwnedFd, class: FdClass) -> RawHandle;
}

#[derive(Debug, thiserror::Error)]
pub enum OsError {
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// The host backend: `UringOs` on Linux, `PollOs` on other Unix hosts.
#[cfg(not(target_os = "linux"))]
pub fn host(limits: &Limits, tables: Tables, dials: &[RawAddr]) -> Result<Box<dyn Os>, OsError> {
    Ok(Box::new(PollOs::new(limits, tables, dials)?))
}

/// The host backend: `UringOs` on Linux, `PollOs` on other Unix hosts.
#[cfg(target_os = "linux")]
pub fn host(limits: &Limits, tables: Tables, dials: &[RawAddr]) -> Result<Box<dyn Os>, OsError> {
    Ok(Box::new(UringOs::new(limits, tables, dials)?))
}

/// Returns `r`, or the last host error when it is negative.
pub(crate) fn cvt<T: Copy + PartialOrd + Default>(r: T) -> Result<T, IoError> {
    if r < T::default() {
        Err(last_error())
    } else {
        Ok(r)
    }
}

/// Runs a `Tty` op inline.
pub(crate) fn tty(fd: BorrowedFd, op: &TtyOp) -> Result<Out, IoError> {
    let fd = fd.as_raw_fd();
    match op {
        TtyOp::Configure(cfg) => configure(fd, cfg)?,
        TtyOp::Flush { queue, .. } => {
            let queue = match queue {
                0 => libc::TCIFLUSH,
                1 => libc::TCOFLUSH,
                _ => libc::TCIOFLUSH,
            };
            // SAFETY: `fd` is open for the call.
            cvt(unsafe { libc::tcflush(fd, queue) })?;
        }
    }
    Ok(Out::UNIT)
}

fn configure(fd: i32, cfg: &TtyConfig) -> Result<(), IoError> {
    let mut t = MaybeUninit::<libc::termios>::uninit();
    // SAFETY: `tcgetattr` fills `t` when it succeeds.
    cvt(unsafe { libc::tcgetattr(fd, t.as_mut_ptr()) })?;
    // SAFETY: filled above.
    let mut t = unsafe { t.assume_init() };
    let speed = speed(cfg.baud()).ok_or(IoError::Os(Errno::EINVAL))?;
    // SAFETY: `t` is a valid termios.
    unsafe {
        libc::cfmakeraw(&mut t);
        cvt(libc::cfsetispeed(&mut t, speed))?;
        cvt(libc::cfsetospeed(&mut t, speed))?;
    }
    t.c_cflag &= !(libc::CSIZE | libc::PARENB | libc::PARODD | libc::CSTOPB | libc::CRTSCTS);
    t.c_cflag |= libc::CLOCAL | libc::CREAD | char_size(cfg.data_bits)?;
    t.c_cflag |= match cfg.parity {
        0 => 0,
        1 => libc::PARENB | libc::PARODD,
        _ => libc::PARENB,
    };
    if cfg.stop_bits == 2 {
        t.c_cflag |= libc::CSTOPB;
    }
    if cfg.flow == 1 {
        t.c_cflag |= libc::CRTSCTS;
    }
    t.c_cc[libc::VMIN] = cfg.vmin;
    t.c_cc[libc::VTIME] = cfg.vtime;
    // SAFETY: `t` is a valid termios and `fd` is open.
    cvt(unsafe { libc::tcsetattr(fd, libc::TCSANOW, &t) })?;
    Ok(())
}

fn char_size(bits: u8) -> Result<libc::tcflag_t, IoError> {
    Ok(match bits {
        5 => libc::CS5,
        6 => libc::CS6,
        7 => libc::CS7,
        8 => libc::CS8,
        _ => return Err(IoError::Os(Errno::EINVAL)),
    })
}

const SPEEDS: [(u32, libc::speed_t); 12] = [
    (1200, libc::B1200),
    (2400, libc::B2400),
    (4800, libc::B4800),
    (9600, libc::B9600),
    (19200, libc::B19200),
    (38400, libc::B38400),
    (57600, libc::B57600),
    (115200, libc::B115200),
    (230400, libc::B230400),
    #[cfg(target_os = "linux")]
    (460800, libc::B460800),
    #[cfg(target_os = "linux")]
    (921600, libc::B921600),
    #[cfg(target_os = "linux")]
    (1000000, libc::B1000000),
    #[cfg(not(target_os = "linux"))]
    (300, libc::B300),
    #[cfg(not(target_os = "linux"))]
    (600, libc::B600),
    #[cfg(not(target_os = "linux"))]
    (150, libc::B150),
];

pub(crate) fn speed(baud: u32) -> Option<libc::speed_t> {
    SPEEDS.iter().find(|(b, _)| *b == baud).map(|(_, s)| *s)
}

/// Runs a `Gpio` op inline, with the Linux GPIO v2 line ioctls.
#[cfg(target_os = "linux")]
pub(crate) fn gpio(fd: BorrowedFd, op: &GpioOp) -> Result<Out, IoError> {
    #[repr(C)]
    struct LineValues {
        bits: u64,
        mask: u64,
    }
    const GET: u64 = 0xC010_B40E;
    const SET: u64 = 0xC010_B40F;
    let (req, bits) = match op {
        GpioOp::Get { .. } => (GET, 0),
        GpioOp::Set { value, .. } => (SET, u64::from(*value & 1)),
    };
    let mut values = LineValues { bits, mask: 1 };
    // SAFETY: `values` matches the kernel's `gpio_v2_line_values`.
    cvt(unsafe { libc::ioctl(fd.as_raw_fd(), req as _, &mut values) })?;
    Ok(match op {
        GpioOp::Get { .. } => Out::len((values.bits & 1) as u32),
        GpioOp::Set { .. } => Out::UNIT,
    })
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn gpio(_fd: BorrowedFd, _op: &GpioOp) -> Result<Out, IoError> {
    Err(IoError::Unsupported)
}

/// `open(2)` flags and mode for portable `OpenFlags`.
pub(crate) fn open_flags(f: OpenFlags) -> (i32, libc::mode_t) {
    let rw = (f.contains(OpenFlags::READ), f.contains(OpenFlags::WRITE));
    let mut flags = libc::O_CLOEXEC
        | match rw {
            (true, true) => libc::O_RDWR,
            (false, true) => libc::O_WRONLY,
            _ => libc::O_RDONLY,
        };
    let bits = [
        (OpenFlags::APPEND, libc::O_APPEND),
        (OpenFlags::CREATE, libc::O_CREAT),
        (OpenFlags::TRUNCATE, libc::O_TRUNC),
        (OpenFlags::EXCLUSIVE, libc::O_EXCL),
        (OpenFlags::NONBLOCK, libc::O_NONBLOCK),
        (OpenFlags::NOCTTY, libc::O_NOCTTY),
        (OpenFlags::DIRECTORY, libc::O_DIRECTORY),
    ];
    for (bit, host) in bits {
        if f.contains(bit) {
            flags |= host;
        }
    }
    let mode = match f.mode() {
        0 => 0o644,
        m => m as libc::mode_t,
    };
    (flags, mode)
}

/// A host socket address for `addr`.
pub(crate) fn sockaddr(
    addr: RawAddr,
) -> Result<(libc::sockaddr_storage, libc::socklen_t), IoError> {
    use core::net::SocketAddr;
    // SAFETY: an all-zero `sockaddr_storage` is valid.
    let mut ss: libc::sockaddr_storage = unsafe { core::mem::zeroed() };
    let len = match SocketAddr::try_from(addr)? {
        SocketAddr::V4(a) => {
            // SAFETY: `sockaddr_storage` is larger and more aligned than `sockaddr_in`.
            let sin = unsafe { &mut *(&mut ss as *mut _ as *mut libc::sockaddr_in) };
            sin.sin_family = libc::AF_INET as libc::sa_family_t;
            sin.sin_port = a.port().to_be();
            sin.sin_addr.s_addr = u32::from_ne_bytes(a.ip().octets());
            size_of::<libc::sockaddr_in>()
        }
        SocketAddr::V6(a) => {
            // SAFETY: `sockaddr_storage` is larger and more aligned than `sockaddr_in6`.
            let sin6 = unsafe { &mut *(&mut ss as *mut _ as *mut libc::sockaddr_in6) };
            sin6.sin6_family = libc::AF_INET6 as libc::sa_family_t;
            sin6.sin6_port = a.port().to_be();
            sin6.sin6_flowinfo = a.flowinfo();
            sin6.sin6_addr.s6_addr = a.ip().octets();
            sin6.sin6_scope_id = a.scope_id();
            size_of::<libc::sockaddr_in6>()
        }
    };
    Ok((ss, len as libc::socklen_t))
}
