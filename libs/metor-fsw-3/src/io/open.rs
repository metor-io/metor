//! Opens a target's real resources at build, ready for the reactor's `Os`.

use std::fs::OpenOptions;
use std::net::{TcpListener, ToSocketAddrs, UdpSocket};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use super::kind::{Access, CanFilter, GpioDirection, RealKind};
use super::os::{FdClass, speed, tty};
use super::{IoError, RawAddr, TtyConfig, TtyOp};

/// A real resource, opened.
#[derive(Debug)]
pub enum Opened {
    /// A descriptor for `Os::register`.
    Fd { fd: OwnedFd, class: FdClass },
    /// An address for the `Os` constructor's dial list. Nothing is opened:
    /// each `Connect` makes its own socket.
    Dial { addr: RawAddr },
}

#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("no termios speed for {0} baud")]
    Baud(u32),
    #[error("address `{0}` does not resolve")]
    Addr(String),
    #[error("{0} is not supported on this host")]
    Unsupported(&'static str),
    #[error("configuring the port: {0}")]
    Configure(IoError),
}

/// Opens `kind`.
pub fn open(kind: &RealKind) -> Result<Opened, OpenError> {
    match kind {
        RealKind::Tty { path, baud } => fd(open_tty(path, *baud)?, FdClass::Tty),
        RealKind::Can { iface, filters } => fd(open_can(iface, filters)?, FdClass::Can),
        RealKind::GpioLine {
            chip,
            line,
            direction,
        } => fd(open_gpio(chip, *line, *direction)?, FdClass::Gpio),
        RealKind::Udp { bind, connect } => {
            fd(open_udp(bind, connect.as_deref())?, FdClass::Datagram)
        }
        RealKind::File { path, access } => fd(open_file(path, *access)?, FdClass::File),
        RealKind::Dir { path } => fd(open_dir(path)?, FdClass::Dir),
        RealKind::TcpListen { addr } => fd(open_listener(addr)?, FdClass::Listener),
        RealKind::TcpConnect { addr } => Ok(Opened::Dial {
            addr: resolve(addr)?,
        }),
    }
}

fn fd(fd: OwnedFd, class: FdClass) -> Result<Opened, OpenError> {
    Ok(Opened::Fd { fd, class })
}

/// A serial port in raw mode at `baud`, 8N1.
fn open_tty(path: &Path, baud: u32) -> Result<OwnedFd, OpenError> {
    if speed(baud).is_none() {
        return Err(OpenError::Baud(baud));
    }
    let flags = libc::O_NOCTTY | libc::O_NONBLOCK | libc::O_CLOEXEC;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(flags)
        .open(path)?;
    let fd = OwnedFd::from(file);
    let configure = TtyOp::Configure(TtyConfig::new(baud));
    tty(fd.as_fd(), &configure).map_err(OpenError::Configure)?;
    Ok(fd)
}

fn open_udp(bind: &str, connect: Option<&str>) -> Result<OwnedFd, OpenError> {
    let socket = UdpSocket::bind(bind)?;
    if let Some(peer) = connect {
        socket.connect(peer)?;
    }
    socket.set_nonblocking(true)?;
    Ok(socket.into())
}

/// A file, created when it is opened for writing.
fn open_file(path: &Path, access: Access) -> Result<OwnedFd, OpenError> {
    let (read, write) = match access {
        Access::Read => (true, false),
        Access::Write => (false, true),
        Access::ReadWrite => (true, true),
    };
    let file = OpenOptions::new()
        .read(read)
        .write(write)
        .create(write)
        .custom_flags(libc::O_CLOEXEC)
        .open(path)?;
    Ok(file.into())
}

fn open_dir(path: &Path) -> Result<OwnedFd, OpenError> {
    let dir = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(path)?;
    Ok(dir.into())
}

fn open_listener(addr: &str) -> Result<OwnedFd, OpenError> {
    let listener = TcpListener::bind(addr)?;
    listener.set_nonblocking(true)?;
    Ok(listener.into())
}

/// The first address `addr` resolves to.
fn resolve(addr: &str) -> Result<RawAddr, OpenError> {
    let unresolved = || OpenError::Addr(addr.to_string());
    let mut addrs = addr.to_socket_addrs().map_err(|_| unresolved())?;
    addrs.next().map(RawAddr::from).ok_or_else(unresolved)
}

/// A raw CAN socket bound to `iface`, passing only `filters` if any.
#[cfg(target_os = "linux")]
fn open_can(iface: &str, filters: &[CanFilter]) -> Result<OwnedFd, OpenError> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};

    let last = || OpenError::Io(std::io::Error::last_os_error());
    let name = CString::new(iface).map_err(|_| OpenError::Addr(iface.to_string()))?;
    // SAFETY: `name` is NUL-terminated.
    let index = unsafe { libc::if_nametoindex(name.as_ptr()) };
    if index == 0 {
        return Err(last());
    }
    let flags = libc::SOCK_RAW | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC;
    // SAFETY: plain socket creation.
    let raw = unsafe { libc::socket(libc::PF_CAN, flags, libc::CAN_RAW) };
    if raw < 0 {
        return Err(last());
    }
    // SAFETY: `socket` returned a new descriptor.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    if !filters.is_empty() {
        let filters: Vec<libc::can_filter> = filters
            .iter()
            .map(|f| libc::can_filter {
                can_id: f.id,
                can_mask: f.mask,
            })
            .collect();
        let len = size_of_val(filters.as_slice()) as libc::socklen_t;
        let (level, name) = (libc::SOL_CAN_RAW, libc::CAN_RAW_FILTER);
        // SAFETY: `filters` is `len` bytes of `can_filter`.
        let r =
            unsafe { libc::setsockopt(fd.as_raw_fd(), level, name, filters.as_ptr().cast(), len) };
        if r < 0 {
            return Err(last());
        }
    }
    // SAFETY: an all-zero `sockaddr_can` is valid.
    let mut addr: libc::sockaddr_can = unsafe { core::mem::zeroed() };
    addr.can_family = libc::AF_CAN as libc::sa_family_t;
    addr.can_ifindex = index as i32;
    let len = size_of::<libc::sockaddr_can>() as libc::socklen_t;
    // SAFETY: `addr` is a `sockaddr_can` of `len` bytes.
    let r = unsafe { libc::bind(fd.as_raw_fd(), (&raw const addr).cast(), len) };
    if r < 0 {
        return Err(last());
    }
    Ok(fd)
}

#[cfg(not(target_os = "linux"))]
fn open_can(_iface: &str, _filters: &[CanFilter]) -> Result<OwnedFd, OpenError> {
    Err(OpenError::Unsupported("CAN"))
}

/// One GPIO line, requested from its chip with the GPIO v2 ioctl.
#[cfg(target_os = "linux")]
fn open_gpio(chip: &Path, line: u32, direction: GpioDirection) -> Result<OwnedFd, OpenError> {
    use std::os::fd::{AsRawFd, FromRawFd};

    /// `struct gpio_v2_line_request`, with its `config` left as raw words.
    #[repr(C)]
    struct LineRequest {
        offsets: [u32; 64],
        consumer: [u8; 32],
        flags: u64,
        num_attrs: u32,
        config_padding: [u32; 5],
        attrs: [[u64; 3]; 10],
        num_lines: u32,
        event_buffer_size: u32,
        padding: [u32; 5],
        fd: i32,
    }
    const GET_LINE: u64 = 0xC250_B407;
    const INPUT: u64 = 1 << 2;
    const OUTPUT: u64 = 1 << 3;
    const _: () = assert!(size_of::<LineRequest>() == 592);

    let chip = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC)
        .open(chip)?;
    // SAFETY: all-zero is a valid `LineRequest`.
    let mut req: LineRequest = unsafe { core::mem::zeroed() };
    req.offsets[0] = line;
    req.consumer[..4].copy_from_slice(b"fsw\0");
    req.flags = match direction {
        GpioDirection::Input => INPUT,
        GpioDirection::Output => OUTPUT,
    };
    req.num_lines = 1;
    // SAFETY: `req` matches the kernel's `gpio_v2_line_request`.
    let r = unsafe { libc::ioctl(chip.as_raw_fd(), GET_LINE as _, &mut req) };
    if r < 0 {
        return Err(OpenError::Io(std::io::Error::last_os_error()));
    }
    // SAFETY: the kernel returned a new line descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(req.fd) })
}

#[cfg(not(target_os = "linux"))]
fn open_gpio(_chip: &Path, _line: u32, _direction: GpioDirection) -> Result<OwnedFd, OpenError> {
    Err(OpenError::Unsupported("GPIO"))
}
