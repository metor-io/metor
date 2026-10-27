//! Interface counters from the `NET_RT_IFLIST2` sysctl, whose `if_data64`
//! counters do not wrap at 4 GiB as `getifaddrs`'s do. macOS has no ethtool,
//! so driver counters stay empty.

use super::{Counters, NicSet};
use crate::sample::{Result, SampleError};

pub struct Source {
    buf: Vec<u8>,
}

/// macOS keeps no per-interface state.
#[derive(Debug, Default)]
pub struct Extra;

impl Source {
    pub fn new(_max: usize) -> Self {
        Self {
            buf: Vec::with_capacity(64 * 1024),
        }
    }

    pub fn read(&mut self, set: &mut NicSet) -> Result<()> {
        let len = self.fetch()?;
        for msg in messages(&self.buf[..len]) {
            if let Some((name, counters)) = interface(msg) {
                set.update(name, counters);
            }
        }
        Ok(())
    }

    /// Fills the buffer with the routing table's interface list.
    fn fetch(&mut self) -> Result<usize> {
        let mut mib = [libc::CTL_NET, libc::PF_ROUTE, 0, 0, libc::NET_RT_IFLIST2, 0];
        let mut len = 0;
        sysctl(&mut mib, core::ptr::null_mut(), &mut len)?;
        if len > self.buf.len() {
            self.buf.resize(len + len / 2, 0);
        }
        len = self.buf.len();
        sysctl(&mut mib, self.buf.as_mut_ptr(), &mut len)?;
        Ok(len)
    }
}

fn sysctl(mib: &mut [libc::c_int], buf: *mut u8, len: &mut usize) -> Result<()> {
    // SAFETY: `buf` is null or `*len` writable bytes.
    let code = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            buf.cast(),
            len,
            core::ptr::null_mut(),
            0,
        )
    };
    if code != 0 {
        return Err(SampleError::Os {
            call: "sysctl(NET_RT_IFLIST2)",
            code: std::io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(code),
        });
    }
    Ok(())
}

/// Splits a routing reply into messages by their length prefix.
fn messages(mut buf: &[u8]) -> impl Iterator<Item = &[u8]> {
    core::iter::from_fn(move || {
        let len = usize::from(u16::from_ne_bytes([*buf.first()?, *buf.get(1)?]));
        if len < 4 || len > buf.len() {
            return None;
        }
        let (msg, rest) = buf.split_at(len);
        buf = rest;
        Some(msg)
    })
}

/// Reads an `RTM_IFINFO2` message's name and counters.
fn interface(msg: &[u8]) -> Option<(&str, Counters)> {
    if i32::from(msg[3]) != libc::RTM_IFINFO2 {
        return None;
    }
    let header_len = size_of::<libc::if_msghdr2>();
    let header = msg.get(..header_len)?;
    // SAFETY: `header` holds a whole `if_msghdr2`, which is plain integers.
    let hdr: libc::if_msghdr2 =
        unsafe { header.as_ptr().cast::<libc::if_msghdr2>().read_unaligned() };
    let name = link_name(msg.get(header_len..)?)?;
    let d = hdr.ifm_data;
    let counters = Counters {
        rx_bytes: d.ifi_ibytes,
        rx_packets: d.ifi_ipackets,
        rx_errors: d.ifi_ierrors,
        rx_dropped: d.ifi_iqdrops,
        tx_bytes: d.ifi_obytes,
        tx_packets: d.ifi_opackets,
        tx_errors: d.ifi_oerrors,
        tx_dropped: u64::try_from(hdr.ifm_snd_drops).unwrap_or(0),
    };
    Some((name, counters))
}

/// The name in the `sockaddr_dl` that follows the header: `sdl_nlen` at
/// byte 5, and the name from byte 8.
fn link_name(sdl: &[u8]) -> Option<&str> {
    let len = usize::from(*sdl.get(5)?);
    core::str::from_utf8(sdl.get(8..8 + len)?)
        .ok()
        .filter(|n| !n.is_empty())
}
