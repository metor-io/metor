//! The resources a target creates by name, and what backs each one.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// One resource a target creates.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceConfig {
    pub id: String,
    pub backing: Backing,
}

/// What a resource is backed by.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backing {
    Real(RealKind),
    Sim(SimKind),
    /// Another target's resource, by name.
    Peer(String),
}

/// A real resource. The set is closed: nothing registers more.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RealKind {
    Tty {
        path: PathBuf,
        baud: u32,
    },
    Can {
        iface: String,
        #[serde(default)]
        filters: Vec<CanFilter>,
    },
    GpioLine {
        chip: PathBuf,
        line: u32,
        direction: GpioDirection,
    },
    Udp {
        bind: String,
        #[serde(default)]
        connect: Option<String>,
    },
    File {
        path: PathBuf,
        #[serde(default)]
        access: Access,
    },
    Dir {
        path: PathBuf,
    },
    TcpListen {
        addr: String,
    },
    TcpConnect {
        addr: String,
    },
}

/// A CAN identifier filter: a frame passes when its id matches `id` in the
/// bits `mask` sets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanFilter {
    pub id: u32,
    pub mask: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GpioDirection {
    Input,
    Output,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    Read,
    Write,
    #[default]
    ReadWrite,
}

/// A simulated resource. A model system serves its device end.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SimKind {
    Uart,
    GpioLine,
}

impl SimKind {
    /// The interface both ends of the resource take.
    pub fn interface(&self) -> &'static str {
        match self {
            Self::Uart => "uart",
            Self::GpioLine => "gpio_line",
        }
    }
}

impl RealKind {
    /// The interface the resource provides, as a `Cap`'s interface names it.
    pub fn interface(&self) -> &'static str {
        match self {
            Self::Tty { .. } => "uart",
            Self::Can { .. } => "can",
            Self::GpioLine { .. } => "gpio_line",
            Self::Udp { .. } => "udp",
            Self::File { .. } => "file",
            Self::Dir { .. } => "dir",
            Self::TcpListen { .. } => "listen",
            Self::TcpConnect { .. } => "dial",
        }
    }
}

impl Backing {
    /// The interface the resource provides. A peer's is not known here.
    pub fn interface(&self) -> Option<&'static str> {
        match self {
            Self::Real(kind) => Some(kind.interface()),
            Self::Sim(kind) => Some(kind.interface()),
            Self::Peer(_) => None,
        }
    }

    /// The backing's kind, as its config names it.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Real(_) => "real",
            Self::Sim(_) => "sim",
            Self::Peer(_) => "peer",
        }
    }
}
