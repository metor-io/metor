//! The resources a target creates by name, and what backs each one.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::Baud;
use super::cap::{self, Interface};

/// What backs a resource a target creates.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backing {
    Real(RealKind),
    Sim(SimResource),
    /// Another target's resource, by name.
    Peer(String),
}

/// A real resource. The set is closed: nothing registers more.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RealKind {
    Tty {
        path: PathBuf,
        baud: Baud,
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

/// A simulated resource: a wire between a driver's `Cap` and a device model's.
///
/// It names both holders, so neither end can be left unheld.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimResource {
    pub kind: SimKind,
    pub host: Holder,
    pub device: Holder,
}

/// A system's `Cap` parameter, as a sim resource names its holder.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holder {
    pub system: String,
    pub param: String,
}

impl Holder {
    pub fn new(system: impl Into<String>, param: impl Into<String>) -> Self {
        Self {
            system: system.into(),
            param: param.into(),
        }
    }
}

/// What a simulated resource carries.
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
            Self::Uart => cap::Uart::NAME,
            Self::GpioLine => cap::GpioLine::NAME,
        }
    }
}

impl RealKind {
    /// The interface the resource provides, as a `Cap`'s interface names it.
    pub fn interface(&self) -> &'static str {
        match self {
            Self::Tty { .. } => cap::Uart::NAME,
            Self::Can { .. } => cap::Can::NAME,
            Self::GpioLine { .. } => cap::GpioLine::NAME,
            Self::Udp { .. } => cap::Udp::NAME,
            Self::File { .. } => cap::RegularFile::NAME,
            Self::Dir { .. } => cap::Dir::NAME,
            Self::TcpListen { .. } => cap::Listen::NAME,
            Self::TcpConnect { .. } => "dial",
        }
    }
}

impl Backing {
    /// The interface the resource provides. A peer's is not known here.
    pub fn interface(&self) -> Option<&'static str> {
        match self {
            Self::Real(kind) => Some(kind.interface()),
            Self::Sim(sim) => Some(sim.kind.interface()),
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
