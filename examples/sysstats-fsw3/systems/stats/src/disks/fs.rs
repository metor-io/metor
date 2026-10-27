//! Space used per mounted local filesystem.

use metor_component::{AsVTable, Componentize, Decomponentize, Metadatatize};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::mem::pct;
use crate::sample::Result;
use crate::set::Set;

/// A filesystem's size in bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Space {
    pub total: u64,
    pub free: u64,
    /// Free to unprivileged users.
    pub available: u64,
}

/// One filesystem's space, as `df` reports it.
#[derive(
    AsVTable,
    Metadatatize,
    Componentize,
    Decomponentize,
    IntoBytes,
    Immutable,
    KnownLayout,
    FromBytes,
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
)]
#[repr(C)]
pub struct FsStats {
    #[fsw(unit = "B")]
    pub total: u64,
    #[fsw(unit = "B")]
    pub used: u64,
    #[fsw(unit = "B")]
    pub available: u64,
    /// `used` over `used + available`.
    #[fsw(unit = "%")]
    pub used_pct: f64,
}

impl From<Space> for FsStats {
    fn from(s: Space) -> Self {
        let used = s.total.saturating_sub(s.free);
        Self {
            total: s.total,
            used,
            available: s.available,
            used_pct: pct(used, used.saturating_add(s.available)),
        }
    }
}

/// Mounted filesystems keyed by mount point.
pub type MountSet = Set<Space, ()>;

/// Writes a mount point as one path segment: `/` is `root`, and the rest
/// drops its leading `/` and joins the parts with `_`.
pub fn mount_key(path: &str, out: &mut String) {
    out.clear();
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() {
        out.push_str("root");
        return;
    }
    out.extend(trimmed.chars().map(|c| match c {
        '/' | '.' => '_',
        c => c,
    }));
}

/// Calls `f` with the mount point and type of every block-device mount in
/// `/proc/self/mounts` text. Mount points keep their octal escapes.
#[cfg(any(target_os = "linux", test))]
pub fn parse_mounts<'a>(text: &'a str, mut f: impl FnMut(&'a str, &'a str)) {
    for line in text.lines() {
        let mut fields = line.split_ascii_whitespace();
        let (Some(dev), Some(path), Some(kind)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if dev.starts_with("/dev/") && kind != "squashfs" {
            f(path, kind);
        }
    }
}

/// Writes `path` with `/proc/mounts` octal escapes (`\040`) decoded, then a
/// NUL, into `out`.
#[cfg(any(target_os = "linux", test))]
pub fn unescape(path: &str, out: &mut Vec<u8>) {
    out.clear();
    let bytes = path.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let octal = bytes.get(i + 1..i + 4).and_then(|d| {
            let d = core::str::from_utf8(d).ok()?;
            u8::from_str_radix(d, 8).ok()
        });
        match (bytes[i], octal) {
            (b'\\', Some(b)) => {
                out.push(b);
                i += 4;
            }
            (b, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    out.push(0);
}

#[cfg(target_os = "linux")]
pub use linux::Mounts;
#[cfg(target_os = "macos")]
pub use macos::Mounts;

#[cfg(target_os = "linux")]
mod linux {
    use core::ffi::CStr;

    use super::{MountSet, Space, mount_key, parse_mounts, unescape};
    use crate::sample::{Result, SampleError, TextFile};

    pub struct Mounts {
        file: TextFile,
        path: Vec<u8>,
        key: String,
    }

    impl Default for Mounts {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Mounts {
        pub fn new() -> Self {
            Self {
                file: TextFile::with_capacity(16 * 1024),
                path: Vec::with_capacity(256),
                key: String::with_capacity(256),
            }
        }

        pub fn read(&mut self, set: &mut MountSet) -> Result<()> {
            let text = self.file.read("/proc/self/mounts")?;
            let (path, key) = (&mut self.path, &mut self.key);
            parse_mounts(text, |mount, _| {
                unescape(mount, path);
                let Ok(cpath) = CStr::from_bytes_with_nul(path) else {
                    return;
                };
                if let Ok(space) = statvfs(cpath) {
                    mount_key(&String::from_utf8_lossy(&path[..path.len() - 1]), key);
                    set.update(key, space);
                }
            });
            Ok(())
        }
    }

    fn statvfs(path: &CStr) -> Result<Space> {
        // SAFETY: `statvfs` is plain integers, valid when zeroed.
        let mut st: libc::statvfs = unsafe { core::mem::zeroed() };
        // SAFETY: `path` is NUL-terminated and `st` a valid out-pointer.
        if unsafe { libc::statvfs(path.as_ptr(), &mut st) } != 0 {
            return Err(SampleError::Os {
                call: "statvfs",
                code: std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
            });
        }
        let block = st.f_frsize as u64;
        Ok(Space {
            total: st.f_blocks as u64 * block,
            free: st.f_bfree as u64 * block,
            available: st.f_bavail as u64 * block,
        })
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use core::ffi::CStr;

    use super::{MountSet, Space, mount_key};
    use crate::sample::{Result, SampleError};

    /// Filesystem types that hold no user data.
    const SKIP: [&[u8]; 2] = [b"devfs", b"autofs"];

    pub struct Mounts {
        key: String,
    }

    impl Default for Mounts {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Mounts {
        pub fn new() -> Self {
            Self {
                key: String::with_capacity(256),
            }
        }

        pub fn read(&mut self, set: &mut MountSet) -> Result<()> {
            for fs in mounts()? {
                if fs.f_flags & libc::MNT_LOCAL as u32 == 0
                    || SKIP.contains(&c_bytes(&fs.f_fstypename))
                {
                    continue;
                }
                let Ok(path) = core::str::from_utf8(c_bytes(&fs.f_mntonname)) else {
                    continue;
                };
                let block = u64::from(fs.f_bsize);
                mount_key(path, &mut self.key);
                set.update(
                    &self.key,
                    Space {
                        total: fs.f_blocks * block,
                        free: fs.f_bfree * block,
                        available: fs.f_bavail * block,
                    },
                );
            }
            Ok(())
        }
    }

    /// The mount table `getmntinfo` keeps, without blocking on remote mounts.
    fn mounts() -> Result<&'static [libc::statfs]> {
        let mut table: *mut libc::statfs = core::ptr::null_mut();
        // SAFETY: `table` is a valid out-pointer; libc owns the buffer.
        let n = unsafe { libc::getmntinfo(&mut table, libc::MNT_NOWAIT) };
        if n <= 0 || table.is_null() {
            return Err(SampleError::Os {
                call: "getmntinfo",
                code: n,
            });
        }
        // SAFETY: `getmntinfo` returned `n` entries at `table`, valid until
        // the next call on this thread; the caller reads them before then.
        Ok(unsafe { core::slice::from_raw_parts(table, n as usize) })
    }

    /// The bytes of a NUL-terminated C string field.
    fn c_bytes(field: &[libc::c_char]) -> &[u8] {
        // SAFETY: `c_char` and `u8` have the same size and alignment.
        let bytes: &[u8] =
            unsafe { core::slice::from_raw_parts(field.as_ptr().cast(), field.len()) };
        CStr::from_bytes_until_nul(bytes).map_or(bytes, CStr::to_bytes)
    }
}

/// Reads every mount into `set`, dropping mounts that are gone.
pub fn sample(mounts: &mut Mounts, set: &mut MountSet) -> Result<()> {
    set.begin();
    let read = mounts.read(set);
    set.end();
    read
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mount_key() {
        let mut key = String::new();
        mount_key("/", &mut key);
        assert_eq!(key, "root");
        mount_key("/System/Volumes/Data", &mut key);
        assert_eq!(key, "System_Volumes_Data");
        mount_key("/mnt/v1.2/", &mut key);
        assert_eq!(key, "mnt_v1_2");
    }

    #[test]
    fn test_parse_mounts() {
        let text = "\
/dev/vda1 / ext4 rw,relatime 0 0
proc /proc proc rw 0 0
/dev/loop0 /snap/core squashfs ro 0 0
/dev/sdb1 /mnt/my\\040disk xfs rw 0 0
short line
";
        let mut seen = Vec::new();
        parse_mounts(text, |path, kind| seen.push((path, kind)));
        assert_eq!(seen, [("/", "ext4"), ("/mnt/my\\040disk", "xfs")]);
    }

    #[test]
    fn test_unescape() {
        let mut out = Vec::new();
        unescape("/mnt/my\\040disk", &mut out);
        assert_eq!(out, b"/mnt/my disk\0");
        unescape("/bad\\9x", &mut out);
        assert_eq!(out, b"/bad\\9x\0");
    }

    #[test]
    fn test_used_pct() {
        let stats = FsStats::from(Space {
            total: 1000,
            free: 300,
            available: 100,
        });
        assert_eq!(stats.used, 700);
        assert_eq!(stats.used_pct, 87.5);
        assert_eq!(FsStats::from(Space::default()).used_pct, 0.0);
    }
}
