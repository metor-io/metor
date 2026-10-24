//! What a real resource's `connect` or `listen` makes, resolved at build so
//! that making it allocates nothing.

use core::net::SocketAddr;
use std::ffi::{CStr, CString};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use super::Class;
use super::open::{OpenError, resolve};
use crate::io::{Access, Baud, Cidr, GpioDirection, RealKind};

pub(crate) enum Recipe {
    /// Opened through the backend, then configured.
    Path(PathKind),
    /// Made and bound by the io step; a dial then connects through the
    /// backend.
    Socket(SocketKind),
}

pub(crate) enum PathKind {
    Tty {
        path: CString,
        baud: Baud,
    },
    GpioLine {
        chip: CString,
        line: u32,
        direction: GpioDirection,
    },
    File {
        path: CString,
        access: Access,
    },
    Dir {
        path: CString,
    },
}

pub(crate) enum SocketKind {
    Udp {
        bind: SocketAddr,
        connect: Option<SocketAddr>,
    },
    /// Each filter as `can_filter`'s `[can_id, can_mask]`.
    Can {
        iface: CString,
        filters: Box<[[u32; 2]]>,
    },
    Dial(SocketAddr),
    Listen {
        addr: SocketAddr,
        allow: Box<[Cidr]>,
    },
}

impl Recipe {
    /// Resolves `kind`. A dial's host name resolves here.
    pub(crate) fn new(kind: &RealKind) -> Result<Self, OpenError> {
        Ok(match kind {
            RealKind::Tty { path, baud } => Self::Path(PathKind::Tty {
                path: c_path(path)?,
                baud: *baud,
            }),
            RealKind::GpioLine {
                chip,
                line,
                direction,
            } => Self::Path(PathKind::GpioLine {
                chip: c_path(chip)?,
                line: *line,
                direction: *direction,
            }),
            RealKind::File { path, access } => Self::Path(PathKind::File {
                path: c_path(path)?,
                access: *access,
            }),
            RealKind::Dir { path } => Self::Path(PathKind::Dir {
                path: c_path(path)?,
            }),
            RealKind::Udp { bind, connect } => Self::Socket(SocketKind::Udp {
                bind: *bind,
                connect: *connect,
            }),
            RealKind::Can { iface, filters } => Self::Socket(SocketKind::Can {
                iface: CString::new(iface.as_str()).map_err(|_| OpenError::Nul)?,
                filters: filters.iter().map(|f| [f.id, f.mask]).collect(),
            }),
            RealKind::TcpDial { addr } => Self::Socket(SocketKind::Dial(resolve(addr)?)),
            RealKind::TcpListen { addr, allow } => Self::Socket(SocketKind::Listen {
                addr: *addr,
                allow: allow.as_slice().into(),
            }),
        })
    }
}

impl PathKind {
    pub(crate) fn path(&self) -> &CStr {
        match self {
            Self::Tty { path, .. } | Self::File { path, .. } | Self::Dir { path } => path,
            Self::GpioLine { chip, .. } => chip,
        }
    }

    /// The host `open` flags.
    pub(crate) fn flags(&self) -> libc::c_int {
        let flags = match self {
            Self::Tty { .. } => libc::O_RDWR | libc::O_NOCTTY | libc::O_NONBLOCK,
            Self::GpioLine { .. } => libc::O_RDWR,
            Self::File { access, .. } => match access {
                Access::Read => libc::O_RDONLY,
                Access::Write => libc::O_WRONLY | libc::O_CREAT,
                Access::ReadWrite => libc::O_RDWR | libc::O_CREAT,
            },
            Self::Dir { .. } => libc::O_RDONLY | libc::O_DIRECTORY,
        };
        flags | libc::O_CLOEXEC
    }

    /// How the opened descriptor's ops run once configured.
    pub(crate) fn class(&self) -> Class {
        match self {
            Self::Tty { .. } => Class::Tty,
            Self::GpioLine { .. } => Class::Gpio,
            Self::File { .. } => Class::File,
            Self::Dir { .. } => Class::Dir,
        }
    }
}

fn c_path(path: &Path) -> Result<CString, OpenError> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| OpenError::Nul)
}
