//! Device counters from `/proc/diskstats`, and SMART health from the NVMe
//! admin ioctl.

use std::fs::File;
use std::os::fd::AsRawFd;

use super::{DiskSet, diskstats, smart};
use crate::sample::{Result, TextFile};

/// `_IOWR('N', 0x41, struct nvme_admin_cmd)`.
const NVME_IOCTL_ADMIN_CMD: u32 = 0xC048_4E41;
const GET_LOG_PAGE: u8 = 0x02;
const SMART_LOG: u32 = 0x02;

pub struct Source {
    file: TextFile,
    smart: bool,
}

impl Source {
    pub fn new(smart: bool) -> Self {
        Self {
            file: TextFile::with_capacity(16 * 1024),
            smart,
        }
    }

    pub fn read(&mut self, set: &mut DiskSet) -> Result<()> {
        let smart = self.smart;
        diskstats::parse(self.file.read("/proc/diskstats")?, |name, counters| {
            let disk = set.update(name, counters);
            if smart {
                disk.extra.os.refresh(&disk.name, &mut disk.extra.smart);
            }
        })
    }
}

/// Whether a device answers SMART queries.
#[derive(Debug, Default)]
pub enum Extra {
    #[default]
    Unknown,
    Unsupported,
    Ready(Nvme),
}

impl Extra {
    fn refresh(&mut self, name: &str, out: &mut Vec<(&'static str, u64)>) {
        if matches!(self, Self::Unknown) {
            *self = Nvme::open(name).map_or(Self::Unsupported, Self::Ready);
        }
        let Self::Ready(nvme) = self else {
            return;
        };
        if !nvme.read_log() {
            *self = Self::Unsupported;
            out.clear();
            return;
        }
        smart::parse(&nvme.log, out);
    }
}

/// An open NVMe namespace and a buffer for its log page.
#[derive(Debug)]
pub struct Nvme {
    dev: File,
    log: Box<[u8; smart::LOG_LEN]>,
}

/// `struct nvme_admin_cmd` from `linux/nvme_ioctl.h`.
#[repr(C)]
#[derive(Default)]
struct AdminCmd {
    opcode: u8,
    flags: u8,
    rsvd1: u16,
    nsid: u32,
    cdw2: u32,
    cdw3: u32,
    metadata: u64,
    addr: u64,
    metadata_len: u32,
    data_len: u32,
    cdw10: u32,
    cdw11: u32,
    cdw12: u32,
    cdw13: u32,
    cdw14: u32,
    cdw15: u32,
    timeout_ms: u32,
    result: u32,
}

impl Nvme {
    /// Opens `/dev/<name>` if `name` is an NVMe namespace such as `nvme0n1`.
    fn open(name: &str) -> Option<Self> {
        if !is_namespace(name) {
            return None;
        }
        let dev = File::open(format!("/dev/{name}")).ok()?;
        let mut nvme = Self {
            dev,
            log: Box::new([0; smart::LOG_LEN]),
        };
        nvme.read_log().then_some(nvme)
    }

    fn read_log(&mut self) -> bool {
        let dwords = (smart::LOG_LEN / 4) as u32;
        let mut cmd = AdminCmd {
            opcode: GET_LOG_PAGE,
            nsid: u32::MAX,
            addr: self.log.as_mut_ptr() as u64,
            data_len: smart::LOG_LEN as u32,
            cdw10: SMART_LOG | ((dwords - 1) << 16),
            ..AdminCmd::default()
        };
        // SAFETY: `cmd` is a valid admin command whose data buffer is `log`,
        // `data_len` bytes that outlive the call.
        let code =
            unsafe { libc::ioctl(self.dev.as_raw_fd(), NVME_IOCTL_ADMIN_CMD as _, &mut cmd) };
        code == 0
    }
}

/// Whether `name` is `nvme<N>n<M>`, a namespace rather than a partition.
fn is_namespace(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("nvme") else {
        return false;
    };
    let Some((ctrl, ns)) = rest.split_once('n') else {
        return false;
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    digits(ctrl) && digits(ns)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_admin_cmd_layout() {
        assert_eq!(size_of::<AdminCmd>(), 72);
    }

    #[test]
    fn test_is_namespace() {
        assert!(is_namespace("nvme0n1"));
        assert!(is_namespace("nvme10n12"));
        assert!(!is_namespace("nvme0n1p1"));
        assert!(!is_namespace("nvme0"));
        assert!(!is_namespace("sda"));
    }
}
