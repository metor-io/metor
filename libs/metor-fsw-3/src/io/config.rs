//! The resources a target creates by name, and what backs each one.

use std::net::{IpAddr, SocketAddr};
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
    /// Accepts streams on `addr` from the peers `allow` names, or from any
    /// peer when it is empty.
    TcpListen {
        addr: SocketAddr,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        allow: Vec<Cidr>,
    },
    /// Dials `addr`, a `host:port`, for each stream. A host name resolves
    /// when the resource opens.
    TcpDial {
        addr: String,
    },
}

/// A block of addresses: those whose first `prefix` bits match `addr`'s.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Cidr {
    addr: IpAddr,
    prefix: u8,
}

impl Cidr {
    /// The block, or `None` if `prefix` is wider than `addr`'s family. A
    /// block inside the IPv4-mapped range is kept as its IPv4 block.
    pub fn new(addr: IpAddr, prefix: u8) -> Option<Self> {
        if u32::from(prefix) > width(addr) {
            return None;
        }
        let mapped = match addr {
            IpAddr::V6(v6) if prefix >= 96 => v6.to_ipv4_mapped(),
            _ => None,
        };
        Some(match mapped {
            Some(v4) => Self {
                addr: IpAddr::V4(v4),
                prefix: prefix - 96,
            },
            None => Self { addr, prefix },
        })
    }

    /// Whether `ip` is in the block. An IPv4-mapped IPv6 address matches as
    /// its IPv4 address.
    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = ip.to_canonical();
        match (self.addr, ip) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                same_prefix(net.to_bits().into(), ip.to_bits().into(), 32, self.prefix)
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                same_prefix(net.to_bits(), ip.to_bits(), 128, self.prefix)
            }
            _ => false,
        }
    }
}

fn width(addr: IpAddr) -> u32 {
    match addr {
        IpAddr::V4(_) => 32,
        IpAddr::V6(_) => 128,
    }
}

/// Whether the first `prefix` of the low `width` bits of `a` and `b` match.
fn same_prefix(a: u128, b: u128, width: u32, prefix: u8) -> bool {
    let shift = width - u32::from(prefix);
    a.checked_shr(shift) == b.checked_shr(shift)
}

impl FromStr for Cidr {
    type Err = BadCidr;

    fn from_str(text: &str) -> Result<Self, BadCidr> {
        let bad = || BadCidr(text.to_string());
        let (addr, prefix) = text.split_once('/').ok_or_else(bad)?;
        if !prefix.bytes().all(|b| b.is_ascii_digit()) {
            return Err(bad());
        }
        let addr: IpAddr = addr.parse().map_err(|_| bad())?;
        let prefix = prefix.parse().map_err(|_| bad())?;
        Self::new(addr, prefix).ok_or_else(bad)
    }
}

impl TryFrom<String> for Cidr {
    type Error = BadCidr;

    fn try_from(text: String) -> Result<Self, BadCidr> {
        text.parse()
    }
}

impl From<Cidr> for String {
    fn from(cidr: Cidr) -> Self {
        format!("{}/{}", cidr.addr, cidr.prefix)
    }
}

/// A CIDR block that is not `addr/prefix`, or whose prefix is wider than
/// its address.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("`{0}` is not a CIDR block such as `10.0.0.0/24`")]
pub struct BadCidr(pub String);

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
            Self::TcpListen { .. } | Self::TcpDial { .. } => cap::Endpoint::NAME,
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

    fn cidr(text: &str) -> Cidr {
        text.parse().unwrap()
    }

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn test_cidr_contains_its_block() {
        let block = cidr("10.0.0.0/24");
        assert!(block.contains(ip("10.0.0.0")) && block.contains(ip("10.0.0.255")));
        assert!(!block.contains(ip("10.0.1.0")) && !block.contains(ip("9.255.255.255")));
        let host = cidr("192.168.1.7/32");
        assert!(host.contains(ip("192.168.1.7")) && !host.contains(ip("192.168.1.8")));
        assert!(cidr("0.0.0.0/0").contains(ip("203.0.113.9")));
        let v6 = cidr("fd00::/8");
        assert!(v6.contains(ip("fd12::1")) && !v6.contains(ip("fe80::1")));
        assert!(cidr("::/0").contains(ip("2001:db8::1")));
    }

    #[test]
    fn test_cidr_families() {
        assert!(
            cidr("10.0.0.0/8").contains(ip("::ffff:10.1.2.3")),
            "a mapped peer"
        );
        assert!(!cidr("10.0.0.0/8").contains(ip("fd00::1")));
        assert!(
            !cidr("::/0").contains(ip("10.0.0.1")),
            "a v6 block holds no v4 peer"
        );
        let mapped = cidr("::ffff:10.0.0.0/104");
        assert_eq!(mapped, cidr("10.0.0.0/8"), "a mapped block is its v4 block");
        assert!(mapped.contains(ip("10.9.9.9")) && mapped.contains(ip("::ffff:10.1.1.1")));
        assert!(cidr("::ffff:0:0/96").contains(ip("192.168.1.1")));
    }

    #[test]
    fn test_bad_cidrs_fail_at_parse() {
        for bad in [
            "10.0.0.0",
            "10.0.0.0/33",
            "::/129",
            "10.0.0/8",
            "10.0.0.0/-1",
            "10.0.0.0/+8",
            "10.0.0.0/",
            "10.0.0.0/255.255.255.0",
            "fe80::1%eth0/64",
            "/8",
            "",
        ] {
            assert_eq!(bad.parse::<Cidr>(), Err(BadCidr(bad.into())), "{bad}");
        }
        assert_eq!(String::from(cidr("10.0.0.0/8")), "10.0.0.0/8");
    }

    #[test]
    fn test_endpoints_parse_with_the_config() {
        let listen = real(r#"{"tcp_listen":{"addr":"0.0.0.0:2240","allow":["10.0.0.0/24"]}}"#);
        assert_eq!(
            listen.unwrap(),
            RealKind::TcpListen {
                addr: "0.0.0.0:2240".parse().unwrap(),
                allow: vec![cidr("10.0.0.0/24")],
            }
        );
        let open = real(r#"{"tcp_listen":{"addr":"0.0.0.0:2240"}}"#).unwrap();
        assert!(matches!(&open, RealKind::TcpListen { allow, .. } if allow.is_empty()));
        assert_eq!(
            serde_json::to_string(&open).unwrap(),
            r#"{"tcp_listen":{"addr":"0.0.0.0:2240"}}"#
        );
        assert!(real(r#"{"tcp_listen":{"addr":"0.0.0.0:2240","allow":["10.0.0.0/40"]}}"#).is_err());
        let dial = real(r#"{"tcp_dial":{"addr":"10.0.0.9:2240"}}"#).unwrap();
        assert_eq!(dial.interface(), cap::Endpoint::NAME);
        assert_eq!(open.interface(), cap::Endpoint::NAME);
        let named = real(r#"{"tcp_dial":{"addr":"gs.local:2240"}}"#).unwrap();
        assert!(matches!(named, RealKind::TcpDial { addr } if addr == "gs.local:2240"));
    }

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

#[cfg(kani)]
mod proofs {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    /// The first `prefix` bits of `a` and `b`, compared one at a time.
    fn reference(a: &[u8], b: &[u8], prefix: u8) -> bool {
        (0..usize::from(prefix)).all(|i| {
            let bit = |x: &[u8]| x[i / 8] >> (7 - i % 8) & 1;
            bit(a) == bit(b)
        })
    }

    #[kani::proof]
    #[kani::unwind(33)]
    fn v4_contains_agrees_with_the_bits() {
        let (net, ip, prefix): ([u8; 4], [u8; 4], u8) = kani::any();
        let Some(block) = Cidr::new(IpAddr::V4(Ipv4Addr::from(net)), prefix) else {
            assert!(prefix > 32);
            return;
        };
        let got = block.contains(IpAddr::V4(Ipv4Addr::from(ip)));
        assert_eq!(got, reference(&net, &ip, prefix));
    }

    #[kani::proof]
    #[kani::unwind(129)]
    fn v6_contains_agrees_with_the_bits() {
        let (net, ip, prefix): ([u8; 16], [u8; 16], u8) = kani::any();
        let ip = Ipv6Addr::from(ip);
        kani::assume(ip.to_ipv4_mapped().is_none());
        let Some(block) = Cidr::new(IpAddr::V6(Ipv6Addr::from(net)), prefix) else {
            assert!(prefix > 128);
            return;
        };
        assert_eq!(
            block.contains(IpAddr::V6(ip)),
            reference(&net, &ip.octets(), prefix)
        );
    }
}
