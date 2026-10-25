//! Ops as plain nonblocking syscalls. `PollOs` runs every op through `run`,
//! and `UringOs` the ones io_uring has no opcode for.

use core::mem::MaybeUninit;
use core::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use core::ptr::NonNull;
use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use super::{Class, Make, OsOp, Out};
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

/// `e` as an `IoError`, or `EIO` for an error with no Linux errno.
pub(crate) fn os(e: std::io::Error) -> IoError {
    IoError::Os(e.raw_os_error().and_then(linux).unwrap_or(Errno::EIO))
}

/// The permission bits of a file a connect creates, before the umask.
pub(super) const CREATE_MODE: libc::mode_t = 0o666;

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
            Op::Accept if op.class == Class::Listener => return accept(fd),
            Op::Connect => return open_path(op),
            Op::Open(flags) => return open_at(fd, buf, flags),
            Op::TtyConfigure(cfg) => return configure(fd, &cfg).map(|()| Out::len(0)),
            Op::TtyFlush(queue) => return flush(fd, queue).map(|()| Out::Len(0)),
            Op::GpioGet => return gpio(fd, None).map(Out::Len),
            Op::GpioSet(level) => return gpio(fd, Some(level)).map(Out::Len),
            Op::Accept | Op::Listen | Op::Close | Op::Cancel | Op::Timeout(_) => {
                return Err(IoError::Unsupported);
            }
        }
    };
    cvt(n).map(|n| Out::Len(n as u32))
}

/// Accepts the next stream on listener `fd`, with its peer.
fn accept(fd: RawFd) -> Result<Out, IoError> {
    // SAFETY: an all-zero `sockaddr_storage` is valid.
    let mut ss: libc::sockaddr_storage = unsafe { core::mem::zeroed() };
    let mut len = size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    let addr = (&raw mut ss).cast::<libc::sockaddr>();
    #[cfg(target_os = "linux")]
    // SAFETY: `addr` is `len` writable bytes.
    let raw = cvt(unsafe {
        libc::accept4(fd, addr, &mut len, libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC)
    })?;
    #[cfg(not(target_os = "linux"))]
    // SAFETY: `addr` is `len` writable bytes.
    let raw = cvt(unsafe { libc::accept(fd, addr, &mut len) })?;
    // SAFETY: `accept` returned a new descriptor, now owned here.
    let stream = unsafe { OwnedFd::from_raw_fd(raw) };
    #[cfg(not(target_os = "linux"))]
    set_flags(stream.as_raw_fd());
    Ok(Out::Opened {
        fd: stream,
        class: Class::Stream,
        peer: socket_addr(&ss),
    })
}

/// Marks socket `fd` close-on-exec and nonblocking, where the call that
/// made it could not, and keeps a send to a closed peer from raising
/// `SIGPIPE`, which `SEND_FLAGS` cannot here.
#[cfg(not(target_os = "linux"))]
fn set_flags(fd: RawFd) {
    let on: libc::c_int = 1;
    let len = size_of::<libc::c_int>() as libc::socklen_t;
    let opt = (libc::SOL_SOCKET, libc::SO_NOSIGPIPE, (&raw const on).cast());
    // SAFETY: plain `fcntl` and `setsockopt` on an open socket; `on` is
    // `len` bytes.
    unsafe {
        libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK);
        libc::setsockopt(fd, opt.0, opt.1, opt.2, len);
    }
}

/// A nonblocking TCP socket for `addr`'s family.
pub(super) fn stream_socket(addr: SocketAddr) -> Result<OwnedFd, IoError> {
    let family = match addr {
        SocketAddr::V4(_) => libc::AF_INET,
        SocketAddr::V6(_) => libc::AF_INET6,
    };
    #[cfg(target_os = "linux")]
    let kind = libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    let kind = libc::SOCK_STREAM;
    // SAFETY: plain socket creation.
    let raw = cvt(unsafe { libc::socket(family, kind, 0) })?;
    // SAFETY: `socket` returned a new descriptor, now owned here.
    let socket = unsafe { OwnedFd::from_raw_fd(raw) };
    #[cfg(not(target_os = "linux"))]
    set_flags(socket.as_raw_fd());
    Ok(socket)
}

/// A nonblocking TCP socket with its connect to `addr` started. Wait for it
/// to take writes, then ask [`connected`].
pub(super) fn connect(addr: SocketAddr) -> Result<OwnedFd, IoError> {
    let socket = stream_socket(addr)?;
    let (ss, len) = sockaddr(addr);
    // SAFETY: `ss` is a socket address of `len` bytes.
    let started = cvt(unsafe { libc::connect(socket.as_raw_fd(), (&raw const ss).cast(), len) });
    match started {
        Ok(_) | Err(IoError::Os(Errno::EINPROGRESS)) => Ok(socket),
        Err(e) => Err(e),
    }
}

/// How the connect started on `fd` ended, once it takes writes: `Ok` once
/// connected, or its error. A socket that takes writes unconnected with no
/// error is `ENOTCONN`.
pub(super) fn connected(fd: RawFd) -> Result<(), IoError> {
    let mut error: libc::c_int = 0;
    let mut len = size_of::<libc::c_int>() as libc::socklen_t;
    let (level, opt, out) = (libc::SOL_SOCKET, libc::SO_ERROR, (&raw mut error).cast());
    // SAFETY: `out` is `len` writable bytes.
    cvt(unsafe { libc::getsockopt(fd, level, opt, out, &mut len) })?;
    if error != 0 {
        return Err(IoError::Os(linux(error).unwrap_or(Errno::EIO)));
    }
    // SAFETY: an all-zero `sockaddr_storage` is valid.
    let mut ss: libc::sockaddr_storage = unsafe { core::mem::zeroed() };
    let mut len = size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    // SAFETY: `ss` is `len` writable bytes.
    cvt(unsafe { libc::getpeername(fd, (&raw mut ss).cast(), &mut len) }).map(drop)
}

/// The address in `ss`, if its family is IPv4 or IPv6.
pub(super) fn socket_addr(ss: &libc::sockaddr_storage) -> Option<SocketAddr> {
    let ptr = core::ptr::from_ref(ss);
    match i32::from(ss.ss_family) {
        libc::AF_INET => {
            // SAFETY: an `AF_INET` address is a `sockaddr_in`, which
            // `sockaddr_storage` is larger and more aligned than.
            let sin = unsafe { &*ptr.cast::<libc::sockaddr_in>() };
            let ip = Ipv4Addr::from(sin.sin_addr.s_addr.to_ne_bytes());
            Some(SocketAddrV4::new(ip, u16::from_be(sin.sin_port)).into())
        }
        libc::AF_INET6 => {
            // SAFETY: as above, for `sockaddr_in6`.
            let sin6 = unsafe { &*ptr.cast::<libc::sockaddr_in6>() };
            let ip = Ipv6Addr::from(sin6.sin6_addr.s6_addr);
            let port = u16::from_be(sin6.sin6_port);
            Some(SocketAddrV6::new(ip, port, sin6.sin6_flowinfo, sin6.sin6_scope_id).into())
        }
        _ => None,
    }
}

/// Opens a connect's path.
fn open_path(op: &OsOp) -> Result<Out, IoError> {
    let Some(Make::Open { path, flags }) = op.make else {
        return Err(IoError::Unsupported);
    };
    // SAFETY: `path` is a NUL-terminated path its recipe keeps alive.
    let raw = cvt(unsafe { libc::open(path.as_ptr(), flags, libc::c_uint::from(CREATE_MODE)) })?;
    Ok(Out::Opened {
        // SAFETY: `open` returned a new descriptor, now owned here.
        fd: unsafe { OwnedFd::from_raw_fd(raw) },
        class: op.class,
        peer: None,
    })
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
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    Ok(Out::Opened {
        fd,
        class,
        peer: None,
    })
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
    (bits, f.mode())
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
