//! Opens a target's real resources at build.

use std::fs::OpenOptions;
use std::net::{SocketAddr, TcpListener, UdpSocket};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use super::Class;
use super::sys::configure;
use crate::io::{Access, Baud, CanFilter, GpioDirection, IoError, RealKind, TtyConfig};

#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("{0} is not supported on this host")]
    Unsupported(&'static str),
    #[error("configuring the port: {0}")]
    Configure(IoError),
}

/// Opens `kind`, set up and nonblocking where its ops wait for readiness. A
/// `TcpDial` opens a socket per op instead, so it has nothing to open.
pub fn open(kind: &RealKind) -> Result<(OwnedFd, Class), OpenError> {
    Ok(match kind {
        RealKind::Tty { path, baud } => (open_tty(path, *baud)?, Class::Tty),
        RealKind::Can { iface, filters } => (open_can(iface.as_str(), filters)?, Class::Datagram),
        RealKind::GpioLine {
            chip,
            line,
            direction,
        } => (open_gpio(chip, *line, *direction)?, Class::Gpio),
        RealKind::Udp { bind, connect } => (open_udp(*bind, *connect)?, Class::Datagram),
        RealKind::File { path, access } => (open_file(path, *access)?, Class::File),
        RealKind::Dir { path } => (open_dir(path)?, Class::Dir),
        RealKind::TcpListen { addr, .. } => {
            let listener = TcpListener::bind(*addr)?;
            listener.set_nonblocking(true)?;
            (listener.into(), Class::Listener)
        }
        RealKind::TcpDial { .. } => return Err(OpenError::Unsupported("opening a dialer")),
    })
}

/// A serial port in raw mode at `baud`, 8N1.
fn open_tty(path: &Path, baud: Baud) -> Result<OwnedFd, OpenError> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    configure(file.as_raw_fd(), &TtyConfig::new(baud)).map_err(OpenError::Configure)?;
    Ok(file.into())
}

fn open_udp(bind: SocketAddr, connect: Option<SocketAddr>) -> Result<OwnedFd, OpenError> {
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

/// A raw CAN socket bound to `iface`, passing only `filters` if any.
#[cfg(target_os = "linux")]
fn open_can(iface: &str, filters: &[CanFilter]) -> Result<OwnedFd, OpenError> {
    use std::ffi::CString;
    use std::os::fd::FromRawFd;

    let name = CString::new(iface).map_err(|_| OpenError::Unsupported("a CAN name with NUL"))?;
    // SAFETY: `name` is NUL-terminated.
    let index = unsafe { libc::if_nametoindex(name.as_ptr()) };
    if index == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let flags = libc::SOCK_RAW | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC;
    // SAFETY: plain socket creation.
    let raw = check(unsafe { libc::socket(libc::PF_CAN, flags, libc::CAN_RAW) })?;
    // SAFETY: `socket` returned a new descriptor, now owned here.
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
        let (level, opt, ptr) = (libc::SOL_CAN_RAW, libc::CAN_RAW_FILTER, filters.as_ptr());
        // SAFETY: `filters` is `len` bytes of `can_filter`.
        check(unsafe { libc::setsockopt(raw, level, opt, ptr.cast(), len) })?;
    }
    // SAFETY: an all-zero `sockaddr_can` is valid.
    let mut addr: libc::sockaddr_can = unsafe { core::mem::zeroed() };
    addr.can_family = libc::AF_CAN as libc::sa_family_t;
    addr.can_ifindex = index as i32;
    let len = size_of::<libc::sockaddr_can>() as libc::socklen_t;
    // SAFETY: `addr` is a `sockaddr_can` of `len` bytes.
    check(unsafe { libc::bind(raw, (&raw const addr).cast(), len) })?;
    Ok(fd)
}

/// `r`, or the last host error when it is negative.
#[cfg(target_os = "linux")]
fn check(r: i32) -> Result<i32, OpenError> {
    match r {
        0.. => Ok(r),
        _ => Err(std::io::Error::last_os_error().into()),
    }
}

#[cfg(not(target_os = "linux"))]
fn open_can(_iface: &str, _filters: &[CanFilter]) -> Result<OwnedFd, OpenError> {
    Err(OpenError::Unsupported("CAN"))
}

/// One GPIO line, requested from its chip with the GPIO v2 ioctl.
#[cfg(target_os = "linux")]
fn open_gpio(chip: &Path, line: u32, direction: GpioDirection) -> Result<OwnedFd, OpenError> {
    use std::os::fd::FromRawFd;

    /// `struct gpio_v2_line_request`, its `config` left as raw words.
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
        GpioDirection::Input => 1 << 2,
        GpioDirection::Output => 1 << 3,
    };
    req.num_lines = 1;
    // SAFETY: `req` matches the kernel's struct, and `chip` is open.
    check(unsafe { libc::ioctl(chip.as_raw_fd(), GET_LINE as _, &mut req) })?;
    // SAFETY: the kernel returned a new line descriptor, now owned here.
    Ok(unsafe { OwnedFd::from_raw_fd(req.fd) })
}

#[cfg(not(target_os = "linux"))]
fn open_gpio(_chip: &Path, _line: u32, _direction: GpioDirection) -> Result<OwnedFd, OpenError> {
    Err(OpenError::Unsupported("GPIO"))
}
