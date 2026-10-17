//! Ops as plain nonblocking syscalls. `PollOs` runs every op through `run`,
//! and `UringOs` the ones io_uring has no opcode for.

use core::mem::MaybeUninit;
use core::ptr::NonNull;
use std::ffi::CString;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};

use super::{Class, OsOp, Out};
use crate::io::{
    Baud, DataBits, Errno, Flow, IoError, Op, OpenFlags, Parity, StopBits, TtyConfig, TtyQueue,
};

/// `send` never raises `SIGPIPE`.
#[cfg(target_os = "linux")]
pub(super) const SEND_FLAGS: i32 = libc::MSG_NOSIGNAL;
#[cfg(not(target_os = "linux"))]
pub(super) const SEND_FLAGS: i32 = 0;

/// The last host error, as a Linux errno, or `EIO` for one with no name here.
pub(crate) fn errno() -> IoError {
    let raw = std::io::Error::last_os_error().raw_os_error();
    IoError::Os(raw.and_then(linux).unwrap_or(Errno::EIO))
}

#[cfg(target_os = "linux")]
pub(super) fn linux(raw: i32) -> Option<Errno> {
    u16::try_from(raw).ok().and_then(Errno::new)
}

#[cfg(not(target_os = "linux"))]
pub(super) fn linux(raw: i32) -> Option<Errno> {
    Errno::HOST.iter().find(|(h, _)| *h == raw).map(|(_, e)| *e)
}

/// `r`, or the last host error when it is negative.
pub(crate) fn cvt<T: Copy + PartialOrd + Default>(r: T) -> Result<T, IoError> {
    if r < T::default() {
        Err(errno())
    } else {
        Ok(r)
    }
}

/// Runs `op` once, without waiting. A descriptor that is not ready gives
/// `EAGAIN`.
pub(super) fn run(op: &OsOp) -> Result<Out, IoError> {
    let (fd, buf) = (op.fd, op.bytes());
    let (ptr, len) = (buf.cast::<libc::c_void>().as_ptr(), buf.len());
    let socket = matches!(op.class, Class::Stream | Class::Datagram);
    // SAFETY: `ptr` is `len` bytes of the op's span, which only this op uses
    // while it is in flight, and `fd` stays open until the op completes.
    let n = unsafe {
        match op.op {
            Op::Read { offset: Some(at) } => libc::pread(fd, ptr, len, at as libc::off_t),
            Op::Write { offset: Some(at) } => libc::pwrite(fd, ptr, len, at as libc::off_t),
            Op::Recv if socket => libc::recv(fd, ptr, len, 0),
            Op::Read { .. } | Op::Recv => libc::read(fd, ptr, len),
            Op::Send if socket => libc::send(fd, ptr, len, SEND_FLAGS),
            Op::Write { .. } | Op::Send => libc::write(fd, ptr, len),
            Op::SendTo(addr) => {
                let (ss, sl) = sockaddr(addr);
                libc::sendto(fd, ptr, len, SEND_FLAGS, (&raw const ss).cast(), sl)
            }
            Op::Fsync => libc::fsync(fd) as isize,
            Op::Accept => return accept(fd),
            Op::Open(flags) => return open_at(fd, buf, flags),
            Op::TtyConfigure(cfg) => return configure(fd, &cfg).map(|()| Out::len(0)),
            Op::TtyFlush(queue) => return flush(fd, queue).map(|()| Out::Len(0)),
            Op::GpioGet => return gpio(fd, None).map(Out::Len),
            Op::GpioSet(level) => return gpio(fd, Some(level)).map(Out::Len),
            Op::Close | Op::Cancel => return Err(IoError::Unsupported),
        }
    };
    cvt(n).map(|n| Out::Len(n as u32))
}

fn accept(fd: RawFd) -> Result<Out, IoError> {
    let (addr, len) = (core::ptr::null_mut(), core::ptr::null_mut());
    #[cfg(target_os = "linux")]
    // SAFETY: plain `accept4` with no address out.
    let raw =
        cvt(unsafe { libc::accept4(fd, addr, len, libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC) })?;
    #[cfg(not(target_os = "linux"))]
    let raw = {
        // SAFETY: plain `accept` with no address out.
        let raw = cvt(unsafe { libc::accept(fd, addr, len) })?;
        // SAFETY: `raw` is the new descriptor.
        unsafe {
            libc::fcntl(raw, libc::F_SETFD, libc::FD_CLOEXEC);
            libc::fcntl(raw, libc::F_SETFL, libc::O_NONBLOCK);
        }
        raw
    };
    // SAFETY: `accept` returned a new descriptor, now owned here.
    Ok(Out::Opened(
        unsafe { OwnedFd::from_raw_fd(raw) },
        Class::Stream,
    ))
}

/// Opens the path in `path`'s bytes under directory `dir`.
pub(super) fn open_at(dir: RawFd, path: NonNull<[u8]>, flags: OpenFlags) -> Result<Out, IoError> {
    // SAFETY: the op's span holds the path, and only this op reads it.
    let path = unsafe { path.as_ref() };
    let path = CString::new(path).map_err(|_| IoError::Os(Errno::EINVAL))?;
    let (bits, mode) = open_flags(flags);
    // SAFETY: `path` is NUL-terminated and `dir` is open.
    let raw = cvt(unsafe { libc::openat(dir, path.as_ptr(), bits, mode as libc::c_uint) })?;
    let class = match flags.contains(OpenFlags::DIRECTORY) {
        true => Class::Dir,
        false => Class::File,
    };
    // SAFETY: `openat` returned a new descriptor, now owned here.
    Ok(Out::Opened(unsafe { OwnedFd::from_raw_fd(raw) }, class))
}

fn open_flags(f: OpenFlags) -> (i32, u32) {
    let rw = (f.contains(OpenFlags::READ), f.contains(OpenFlags::WRITE));
    let mut bits = libc::O_CLOEXEC
        | match rw {
            (true, true) => libc::O_RDWR,
            (false, true) => libc::O_WRONLY,
            _ => libc::O_RDONLY,
        };
    for (flag, host) in [
        (OpenFlags::APPEND, libc::O_APPEND),
        (OpenFlags::CREATE, libc::O_CREAT),
        (OpenFlags::TRUNCATE, libc::O_TRUNC),
        (OpenFlags::EXCLUSIVE, libc::O_EXCL),
        (OpenFlags::DIRECTORY, libc::O_DIRECTORY),
    ] {
        if f.contains(flag) {
            bits |= host;
        }
    }
    let mode = match f.mode() {
        0 => 0o644,
        m => m,
    };
    (bits, mode)
}

/// Applies `cfg` to a serial port, in raw mode.
pub(crate) fn configure(fd: RawFd, cfg: &TtyConfig) -> Result<(), IoError> {
    let speed = speed(cfg.baud);
    let mut t = MaybeUninit::<libc::termios>::uninit();
    // SAFETY: `tcgetattr` fills `t` when it succeeds.
    cvt(unsafe { libc::tcgetattr(fd, t.as_mut_ptr()) })?;
    // SAFETY: `tcgetattr` succeeded.
    let mut t = unsafe { t.assume_init() };
    // SAFETY: `t` is a valid termios.
    unsafe {
        libc::cfmakeraw(&mut t);
        cvt(libc::cfsetispeed(&mut t, speed))?;
        cvt(libc::cfsetospeed(&mut t, speed))?;
    }
    t.c_cflag &= !(libc::CSIZE | libc::PARENB | libc::PARODD | libc::CSTOPB | libc::CRTSCTS);
    t.c_cflag |= libc::CLOCAL | libc::CREAD;
    t.c_cflag |= match cfg.data_bits {
        DataBits::Five => libc::CS5,
        DataBits::Six => libc::CS6,
        DataBits::Seven => libc::CS7,
        DataBits::Eight => libc::CS8,
    };
    t.c_cflag |= match cfg.parity {
        Parity::None => 0,
        Parity::Odd => libc::PARENB | libc::PARODD,
        Parity::Even => libc::PARENB,
    };
    t.c_cflag |= match cfg.stop_bits {
        StopBits::One => 0,
        StopBits::Two => libc::CSTOPB,
    };
    t.c_cflag |= match cfg.flow {
        Flow::None => 0,
        Flow::RtsCts => libc::CRTSCTS,
    };
    t.c_cc[libc::VMIN] = cfg.vmin;
    t.c_cc[libc::VTIME] = cfg.vtime;
    // SAFETY: `t` is a valid termios and `fd` is open.
    cvt(unsafe { libc::tcsetattr(fd, libc::TCSANOW, &t) }).map(drop)
}

fn flush(fd: RawFd, queue: TtyQueue) -> Result<(), IoError> {
    let queue = match queue {
        TtyQueue::Input => libc::TCIFLUSH,
        TtyQueue::Output => libc::TCOFLUSH,
        TtyQueue::Both => libc::TCIOFLUSH,
    };
    // SAFETY: plain `tcflush` on an open descriptor.
    cvt(unsafe { libc::tcflush(fd, queue) }).map(drop)
}

/// The termios speed for `baud`.
#[cfg(target_os = "linux")]
fn speed(baud: Baud) -> libc::speed_t {
    match baud {
        Baud::B1200 => libc::B1200,
        Baud::B2400 => libc::B2400,
        Baud::B4800 => libc::B4800,
        Baud::B9600 => libc::B9600,
        Baud::B19200 => libc::B19200,
        Baud::B38400 => libc::B38400,
        Baud::B57600 => libc::B57600,
        Baud::B115200 => libc::B115200,
        Baud::B230400 => libc::B230400,
        Baud::B460800 => libc::B460800,
        Baud::B921600 => libc::B921600,
        Baud::B1000000 => libc::B1000000,
    }
}

/// The termios speed for `baud`, which on BSD hosts is the rate itself.
#[cfg(not(target_os = "linux"))]
fn speed(baud: Baud) -> libc::speed_t {
    libc::speed_t::from(baud.bits_per_second())
}

/// Reads a GPIO line, or sets it to `level`, with the GPIO v2 line ioctls.
/// A read answers the level.
#[cfg(target_os = "linux")]
fn gpio(fd: RawFd, level: Option<bool>) -> Result<u32, IoError> {
    /// `struct gpio_v2_line_values`.
    #[repr(C)]
    struct LineValues {
        bits: u64,
        mask: u64,
    }
    const GET: u64 = 0xC010_B40E;
    const SET: u64 = 0xC010_B40F;
    let mut values = LineValues {
        bits: u64::from(level == Some(true)),
        mask: 1,
    };
    let req = if level.is_some() { SET } else { GET };
    // SAFETY: `values` matches the kernel's struct, and `fd` is open.
    cvt(unsafe { libc::ioctl(fd, req as _, &mut values) })?;
    Ok(match level {
        None => (values.bits & 1) as u32,
        Some(_) => 0,
    })
}

#[cfg(not(target_os = "linux"))]
fn gpio(_fd: RawFd, _level: Option<bool>) -> Result<u32, IoError> {
    Err(IoError::Unsupported)
}

/// `addr` as a host `sockaddr_storage` and its length.
pub(super) fn sockaddr(addr: core::net::SocketAddr) -> (libc::sockaddr_storage, libc::socklen_t) {
    use core::net::SocketAddr;
    // SAFETY: an all-zero `sockaddr_storage` is valid.
    let mut ss: libc::sockaddr_storage = unsafe { core::mem::zeroed() };
    let len = match addr {
        SocketAddr::V4(a) => {
            // SAFETY: `sockaddr_storage` is larger and more aligned than `sockaddr_in`.
            let sin = unsafe { &mut *(&raw mut ss).cast::<libc::sockaddr_in>() };
            sin.sin_family = libc::AF_INET as libc::sa_family_t;
            sin.sin_port = a.port().to_be();
            sin.sin_addr.s_addr = u32::from_ne_bytes(a.ip().octets());
            size_of::<libc::sockaddr_in>()
        }
        SocketAddr::V6(a) => {
            // SAFETY: `sockaddr_storage` is larger and more aligned than `sockaddr_in6`.
            let sin6 = unsafe { &mut *(&raw mut ss).cast::<libc::sockaddr_in6>() };
            sin6.sin6_family = libc::AF_INET6 as libc::sa_family_t;
            sin6.sin6_port = a.port().to_be();
            sin6.sin6_flowinfo = a.flowinfo();
            sin6.sin6_addr.s6_addr = a.ip().octets();
            sin6.sin6_scope_id = a.scope_id();
            size_of::<libc::sockaddr_in6>()
        }
    };
    (ss, len as libc::socklen_t)
}
