//! Interface counters from `/proc/net/dev`, and driver counters from the
//! `SIOCETHTOOL` ioctl.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use zerocopy::IntoBytes;

use super::{NicSet, ethtool, net_dev};
use crate::sample::{Result, TextFile};

const ETHTOOL_GSTRINGS: u32 = 0x1b;
const ETHTOOL_GSTATS: u32 = 0x1d;
const ETHTOOL_GSSET_INFO: u32 = 0x37;
const ETH_SS_STATS: u32 = 1;

/// Room past the probed count in case the driver adds counters later; the
/// kernel writes as many as the driver reports now.
const SLACK: usize = 64;

pub struct Source {
    file: TextFile,
    ethtool: Option<Ethtool>,
}

impl Source {
    pub fn new(max: usize) -> Self {
        Self {
            file: TextFile::with_capacity(16 * 1024),
            ethtool: Ethtool::open(max),
        }
    }

    pub fn read(&mut self, set: &mut NicSet) -> Result<()> {
        let ethtool = self.ethtool.as_ref();
        net_dev::parse(self.file.read("/proc/net/dev")?, |name, counters| {
            let nic = set.update(name, counters);
            if let Some(ethtool) = ethtool {
                ethtool.refresh(&nic.name, &mut nic.extra.os, &mut nic.extra.driver);
            }
        })
    }
}

/// Whether an interface's driver counters have been probed.
#[derive(Debug, Default, PartialEq, Eq)]
enum Probe {
    #[default]
    Unknown,
    Unsupported,
    Ready(usize),
}

/// Per-interface ethtool state.
#[derive(Debug, Default)]
pub struct Extra {
    probe: Probe,
    picks: Vec<u16>,
    stats: Vec<u64>,
}

/// A socket to issue ethtool ioctls on.
struct Ethtool {
    sock: OwnedFd,
    max: usize,
}

impl Ethtool {
    fn open(max: usize) -> Option<Self> {
        // SAFETY: `socket` has no memory preconditions.
        let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
        if fd < 0 || max == 0 {
            return None;
        }
        // SAFETY: `fd` is a new socket this process owns.
        let sock = unsafe { OwnedFd::from_raw_fd(fd) };
        Some(Self { sock, max })
    }

    /// Reads `ifname`'s counters into `driver`, probing names on first use.
    fn refresh(&self, ifname: &str, extra: &mut Extra, driver: &mut super::DriverCounters) {
        if extra.probe == Probe::Unknown {
            extra.probe = self.probe(ifname, extra, driver);
        }
        let Probe::Ready(n) = extra.probe else {
            return;
        };
        match self.stats(ifname, &mut extra.stats) {
            Some(got) if got == n => {
                ethtool::gather(&extra.stats[1..], &extra.picks, &mut driver.values)
            }
            _ => {
                extra.probe = Probe::Unknown;
                driver.values.iter_mut().for_each(|v| *v = 0);
            }
        }
    }

    fn probe(&self, ifname: &str, extra: &mut Extra, driver: &mut super::DriverCounters) -> Probe {
        let Some(n) = self.count(ifname).filter(|&n| n > 0) else {
            return Probe::Unsupported;
        };
        let mut buf = vec![0u32; 3 + n * ethtool::NAME_LEN / 4];
        buf[0] = ETHTOOL_GSTRINGS;
        buf[1] = ETH_SS_STATS;
        buf[2] = n as u32;
        if !self.ioctl(ifname, buf.as_mut_bytes()) {
            return Probe::Unsupported;
        }
        let raw = &buf.as_bytes()[12..];
        ethtool::pick_names(raw, self.max, &mut driver.names, &mut extra.picks);
        driver.values.clear();
        driver.values.resize(driver.names.len(), 0);
        extra.stats = vec![0; 1 + n + SLACK];
        Probe::Ready(n)
    }

    /// The number of counters the driver reports.
    fn count(&self, ifname: &str) -> Option<usize> {
        // `{ cmd: u32, reserved: u32, sset_mask: u64, data: [u32] }`
        let mut buf = [0u64; 3];
        put_u32(buf.as_mut_bytes(), 0, ETHTOOL_GSSET_INFO);
        buf[1] = 1 << ETH_SS_STATS;
        if !self.ioctl(ifname, buf.as_mut_bytes()) || buf[1] & (1 << ETH_SS_STATS) == 0 {
            return None;
        }
        Some(get_u32(buf.as_bytes(), 16) as usize)
    }

    /// Fills `buf` with an `ETHTOOL_GSTATS` reply, returning its count.
    fn stats(&self, ifname: &str, buf: &mut [u64]) -> Option<usize> {
        // `{ cmd: u32, n_stats: u32, data: [u64] }`
        put_u32(buf.as_mut_bytes(), 0, ETHTOOL_GSTATS);
        self.ioctl(ifname, buf.as_mut_bytes())
            .then(|| get_u32(buf.as_bytes(), 4) as usize)
    }

    /// Issues `SIOCETHTOOL` for `ifname` with `data` as the request.
    fn ioctl(&self, ifname: &str, data: &mut [u8]) -> bool {
        // SAFETY: `ifreq` is plain data, valid when zeroed.
        let mut req: libc::ifreq = unsafe { core::mem::zeroed() };
        let name = ifname.as_bytes();
        if name.len() >= req.ifr_name.len() {
            return false;
        }
        for (dst, &src) in req.ifr_name.iter_mut().zip(name) {
            *dst = src as libc::c_char;
        }
        req.ifr_ifru.ifru_data = data.as_mut_ptr().cast();
        // SAFETY: `req` names the interface and points at `data`, which holds
        // the request the command expects.
        let code = unsafe { libc::ioctl(self.sock.as_raw_fd(), libc::SIOCETHTOOL as _, &mut req) };
        code == 0
    }
}

fn put_u32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_ne_bytes());
}

fn get_u32(bytes: &[u8], at: usize) -> u32 {
    let mut word = [0; 4];
    word.copy_from_slice(&bytes[at..at + 4]);
    u32::from_ne_bytes(word)
}
