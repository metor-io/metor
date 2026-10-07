use core::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

use zerocopy::little_endian::{U16, U32};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

use super::IoError;

const AF_INET: u16 = 2;
const AF_INET6: u16 = 10;

/// A fixed-size socket address with a Linux address family.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, IntoBytes, FromBytes, KnownLayout, Immutable, Unaligned,
)]
#[repr(C)]
pub struct RawAddr {
    family: U16,
    port: U16,
    flowinfo: U32,
    ip: [u8; 16],
    scope_id: U32,
}

impl From<SocketAddr> for RawAddr {
    fn from(addr: SocketAddr) -> Self {
        match addr {
            SocketAddr::V4(a) => {
                let mut ip = [0; 16];
                ip[..4].copy_from_slice(&a.ip().octets());
                Self {
                    family: U16::new(AF_INET),
                    port: U16::new(a.port()),
                    flowinfo: U32::ZERO,
                    ip,
                    scope_id: U32::ZERO,
                }
            }
            SocketAddr::V6(a) => Self {
                family: U16::new(AF_INET6),
                port: U16::new(a.port()),
                flowinfo: U32::new(a.flowinfo()),
                ip: a.ip().octets(),
                scope_id: U32::new(a.scope_id()),
            },
        }
    }
}

impl TryFrom<RawAddr> for SocketAddr {
    type Error = IoError;

    fn try_from(a: RawAddr) -> Result<Self, IoError> {
        let port = a.port.get();
        match a.family.get() {
            AF_INET => {
                let [o0, o1, o2, o3, ..] = a.ip;
                let ip = Ipv4Addr::new(o0, o1, o2, o3);
                Ok(SocketAddrV4::new(ip, port).into())
            }
            AF_INET6 => {
                let ip = Ipv6Addr::from(a.ip);
                let (flow, scope) = (a.flowinfo.get(), a.scope_id.get());
                Ok(SocketAddrV6::new(ip, port, flow, scope).into())
            }
            _ => Err(IoError::BadAddr),
        }
    }
}
