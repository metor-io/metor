//! The resources a target creates by name, and what backs each one.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use super::Baud;
use super::cap::{self, Interface};

/// What backs a resource a target creates.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backing {
    Real(RealKind),
    Sim(SimResource),
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
        iface: CanIface,
        #[serde(default)]
        filters: Vec<CanFilter>,
    },
    GpioLine {
        chip: PathBuf,
        line: u32,
        direction: GpioDirection,
    },
    Udp {
        bind: SocketAddr,
        #[serde(default)]
        connect: Option<SocketAddr>,
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
        addr: SocketAddr,
    },
}

/// A CAN interface name, such as `can0`: 1 to 15 bytes with no NUL, as Linux
/// takes it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CanIface(String);

impl CanIface {
    /// The longest name Linux takes, less its NUL.
    pub const MAX_LEN: usize = 15;

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for CanIface {
    type Error = BadCanIface;

    fn try_from(name: String) -> Result<Self, BadCanIface> {
        let fits = (1..=Self::MAX_LEN).contains(&name.len());
        match fits && !name.contains('\0') {
            true => Ok(Self(name)),
            false => Err(BadCanIface(name)),
        }
    }
}

impl FromStr for CanIface {
    type Err = BadCanIface;

    fn from_str(name: &str) -> Result<Self, BadCanIface> {
        Self::try_from(name.to_string())
    }
}

impl From<CanIface> for String {
    fn from(iface: CanIface) -> Self {
        iface.0
    }
}

/// A CAN interface name that is empty, too long, or holds a NUL.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("CAN interface `{0}` is not 1 to 15 bytes without a NUL")]
pub struct BadCanIface(pub String);

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
        }
    }
}

impl Backing {
    /// The interface the resource provides.
    pub fn interface(&self) -> &'static str {
        match self {
            Self::Real(kind) => kind.interface(),
            Self::Sim(sim) => sim.kind.interface(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_can_iface_bounds() {
        assert_eq!(
            "can0".parse::<CanIface>().map(String::from),
            Ok("can0".into())
        );
        assert!("a".repeat(CanIface::MAX_LEN).parse::<CanIface>().is_ok());
        for bad in [
            String::new(),
            "a".repeat(CanIface::MAX_LEN + 1),
            "can\u{0}0".into(),
        ] {
            assert_eq!(bad.parse::<CanIface>(), Err(BadCanIface(bad.clone())));
        }
    }

    #[test]
    fn test_addresses_parse_with_the_config() {
        let udp: RealKind =
            serde_json::from_str(r#"{"udp": {"bind": "127.0.0.1:9000", "connect": "[::1]:9001"}}"#)
                .expect("IP literals parse");
        assert_eq!(
            udp,
            RealKind::Udp {
                bind: "127.0.0.1:9000".parse().unwrap(),
                connect: Some("[::1]:9001".parse().unwrap()),
            }
        );
        let host =
            serde_json::from_str::<RealKind>(r#"{"tcp_listen": {"addr": "localhost:9000"}}"#);
        assert!(host.is_err(), "a host name is not an address");
        let can = serde_json::from_str::<RealKind>(r#"{"can": {"iface": ""}}"#);
        assert!(can.is_err(), "an empty CAN name fails at parse");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn real(text: &str) -> Result<RealKind, serde_json::Error> {
        serde_json::from_str(text)
    }

    #[test]
    fn test_real_kinds_take_known_modes() {
        let line = real(r#"{"gpio_line":{"chip":"/dev/gpiochip0","line":3,"direction":"output"}}"#);
        assert!(matches!(
            line,
            Ok(RealKind::GpioLine {
                direction: GpioDirection::Output,
                ..
            })
        ));
        let file = real(r#"{"file":{"path":"/tmp/log"}}"#);
        assert!(matches!(
            file,
            Ok(RealKind::File {
                access: Access::ReadWrite,
                ..
            })
        ));
        assert!(real(r#"{"gpio_line":{"chip":"c","line":3,"direction":"both"}}"#).is_err());
        assert!(real(r#"{"file":{"path":"/tmp/log","access":"append"}}"#).is_err());
    }
}
