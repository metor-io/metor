//! Makes a connection's descriptor from its recipe: a socket made and
//! bound, or an opened path configured.

use std::ffi::CStr;
use std::net::{SocketAddr, TcpListener, ToSocketAddrs, UdpSocket};
use std::os::fd::{AsRawFd, OwnedFd};

use super::Class;
use super::recipe::{PathKind, SocketKind};
use super::sys;
use crate::io::{GpioDirection, IoError, TtyConfig};

/// Why a resource's recipe could not be made at build.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("`{0}` resolves to no address")]
    NoAddress(String),
    #[error("a path or name holds a NUL")]
    Nul,
}

/// The first address `addr`, a `host:port`, resolves to.
pub fn resolve(addr: &str) -> Result<SocketAddr, OpenError> {
    let mut found = addr.to_socket_addrs()?;
    found
        .next()
        .ok_or_else(|| OpenError::NoAddress(addr.to_string()))
}

/// A socket, bound and nonblocking. A dial's socket is made by the backend
/// that connects it.
pub(crate) fn make_socket(kind: &SocketKind) -> Result<(OwnedFd, Class), IoError> {
    match kind {
        SocketKind::Udp { bind, connect } => {
            let socket = UdpSocket::bind(bind).map_err(sys::os)?;
            if let Some(peer) = connect {
                socket.connect(peer).map_err(sys::os)?;
            }
            socket.set_nonblocking(true).map_err(sys::os)?;
            Ok((socket.into(), Class::Datagram))
        }
        SocketKind::Can { iface, filters } => Ok((open_can(iface, filters)?, Class::Datagram)),
        SocketKind::Listen { addr, .. } => {
            let listener = TcpListener::bind(addr).map_err(sys::os)?;
            listener.set_nonblocking(true).map_err(sys::os)?;
            Ok((listener.into(), Class::Listener))
        }
        SocketKind::Dial(_) => Err(IoError::Unsupported),
    }
}

/// Sets up a path the backend opened: a tty's line settings, or a GPIO
/// chip's line, whose descriptor takes the chip's place.
pub(crate) fn configure(kind: &PathKind, fd: OwnedFd) -> Result<(OwnedFd, Class), IoError> {
    match kind {
        PathKind::Tty { baud, .. } => {
            sys::configure(fd.as_raw_fd(), &TtyConfig::new(*baud))?;
            Ok((fd, Class::Tty))
        }
        PathKind::GpioLine {
            line, direction, ..
        } => Ok((request_line(&fd, *line, *direction)?, Class::Gpio)),
        PathKind::File { .. } | PathKind::Dir { .. } => Ok((fd, kind.class())),
    }
}

/// A raw CAN socket bound to `iface`, passing only `filters` if any.
#[cfg(target_os = "linux")]
fn open_can(iface: &CStr, filters: &[[u32; 2]]) -> Result<OwnedFd, IoError> {
    use std::os::fd::FromRawFd;

    // SAFETY: `iface` is NUL-terminated.
    let index = unsafe { libc::if_nametoindex(iface.as_ptr()) };
    if index == 0 {
        return Err(sys::errno());
    }
    let flags = libc::SOCK_RAW | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC;
    // SAFETY: plain socket creation.
    let raw = sys::cvt(unsafe { libc::socket(libc::PF_CAN, flags, libc::CAN_RAW) })?;
    // SAFETY: `socket` returned a new descriptor, now owned here.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    if !filters.is_empty() {
        const _: () = assert!(size_of::<[u32; 2]>() == size_of::<libc::can_filter>());
        let len = size_of_val(filters) as libc::socklen_t;
        let (level, opt, ptr) = (libc::SOL_CAN_RAW, libc::CAN_RAW_FILTER, filters.as_ptr());
        // SAFETY: `filters` is `len` bytes laid out as `can_filter`s.
        sys::cvt(unsafe { libc::setsockopt(raw, level, opt, ptr.cast(), len) })?;
    }
    // SAFETY: an all-zero `sockaddr_can` is valid.
    let mut addr: libc::sockaddr_can = unsafe { core::mem::zeroed() };
    addr.can_family = libc::AF_CAN as libc::sa_family_t;
    addr.can_ifindex = index as i32;
    let len = size_of::<libc::sockaddr_can>() as libc::socklen_t;
    // SAFETY: `addr` is a `sockaddr_can` of `len` bytes.
    sys::cvt(unsafe { libc::bind(raw, (&raw const addr).cast(), len) })?;
    Ok(fd)
}

#[cfg(not(target_os = "linux"))]
fn open_can(_iface: &CStr, _filters: &[[u32; 2]]) -> Result<OwnedFd, IoError> {
    Err(IoError::Unsupported)
}

/// One GPIO line, requested from its open chip with the GPIO v2 ioctl.
#[cfg(target_os = "linux")]
fn request_line(chip: &OwnedFd, line: u32, direction: GpioDirection) -> Result<OwnedFd, IoError> {
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
    sys::cvt(unsafe { libc::ioctl(chip.as_raw_fd(), GET_LINE as _, &mut req) })?;
    // SAFETY: the kernel returned a new line descriptor, now owned here.
    Ok(unsafe { OwnedFd::from_raw_fd(req.fd) })
}

#[cfg(not(target_os = "linux"))]
fn request_line(
    _chip: &OwnedFd,
    _line: u32,
    _direction: GpioDirection,
) -> Result<OwnedFd, IoError> {
    Err(IoError::Unsupported)
}
