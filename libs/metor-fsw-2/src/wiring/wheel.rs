//! A deterministic wheel (zip) writer, the sibling of [`bundle`](super::bundle)'s
//! reproducible tar: entries sorted by archive name, timestamps fixed at the
//! DOS epoch, unix modes in the central directory's external attributes, and
//! every entry **stored** rather than compressed — identical inputs produce
//! byte-identical wheels with no compressor in the loop, and installers
//! accept stored entries per the zip and wheel specs. Pack payloads are a
//! few MB of shared objects; a size guardrail is phase 4's concern
//! (`docs/design-packaging.md` §7.1).
//!
//! [`write_wheel`] assembles the `dist-info` (`METADATA`, `WHEEL`, a sha256
//! `RECORD`) around the caller's files and writes
//! `<dist>-<version>-<tag>.whl`.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use sha2::{Digest, Sha256};

/// The wheel's identity and dependency metadata.
#[derive(Clone, Debug)]
pub struct WheelMeta {
    /// The distribution name as authored (`adcs-pack`); file names use the
    /// PEP 427/503 normalization.
    pub dist_name: String,
    /// The distribution version.
    pub version: String,
    /// The wheel tag (`py3-none-any` for a fat pack wheel).
    pub tag: String,
    /// The `Requires-Python` specifier, if any.
    pub requires_python: Option<String>,
    /// `Requires-Dist` lines: the injected pins plus the pack's own
    /// `[project] dependencies`.
    pub requires: Vec<String>,
}

/// One payload file: archive name, contents, and unix mode (`0o644` for
/// text, `0o755` for the shared objects — installers extract the mode from
/// the central directory, which is how a packaged binary stays executable).
#[derive(Clone, Debug)]
pub struct WheelFile {
    /// The archive path, `/`-separated.
    pub arcname: String,
    /// The file contents.
    pub bytes: Vec<u8>,
    /// The unix permission bits.
    pub mode: u32,
}

/// The distribution name as it appears in file names (PEP 427/503): runs of
/// `-`, `_`, `.` collapse to one `_`, lowercased.
pub fn normalize_dist(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut in_sep = false;
    for c in name.chars() {
        if c == '-' || c == '_' || c == '.' {
            if !in_sep {
                out.push('_');
            }
            in_sep = true;
        } else {
            out.push(c.to_ascii_lowercase());
            in_sep = false;
        }
    }
    out
}

/// Write `<dist>-<version>-<tag>.whl` into `out_dir` and return its path.
pub fn write_wheel(
    out_dir: &Path,
    meta: &WheelMeta,
    files: Vec<WheelFile>,
) -> std::io::Result<PathBuf> {
    let dist = normalize_dist(&meta.dist_name);
    let dist_info = format!("{dist}-{}.dist-info", meta.version);

    let mut entries = files;
    entries.push(WheelFile {
        arcname: format!("{dist_info}/METADATA"),
        bytes: metadata_contents(meta).into_bytes(),
        mode: 0o644,
    });
    entries.push(WheelFile {
        arcname: format!("{dist_info}/WHEEL"),
        bytes: format!(
            "Wheel-Version: 1.0\nGenerator: metor-fsw {}\nRoot-Is-Purelib: {}\nTag: {}\n",
            env!("CARGO_PKG_VERSION"),
            meta.tag.ends_with("-any"),
            meta.tag,
        )
        .into_bytes(),
        mode: 0o644,
    });
    // Stable byte order before RECORD, whose own line order must match.
    entries.sort_by(|a, b| a.arcname.cmp(&b.arcname));

    let mut record = String::new();
    for entry in &entries {
        let digest = Sha256::digest(&entry.bytes);
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
        record.push_str(&format!(
            "{},sha256={},{}\n",
            entry.arcname,
            b64,
            entry.bytes.len()
        ));
    }
    record.push_str(&format!("{dist_info}/RECORD,,\n"));
    entries.push(WheelFile {
        arcname: format!("{dist_info}/RECORD"),
        bytes: record.into_bytes(),
        mode: 0o644,
    });

    let name = format!("{dist}-{}-{}.whl", meta.version, meta.tag);
    let path = out_dir.join(name);
    std::fs::write(&path, zip_stored(&entries))?;
    Ok(path)
}

/// The `METADATA` text: core identity plus the dependency lines.
fn metadata_contents(meta: &WheelMeta) -> String {
    let mut out = format!(
        "Metadata-Version: 2.1\nName: {}\nVersion: {}\n",
        meta.dist_name, meta.version
    );
    if let Some(rp) = &meta.requires_python {
        out.push_str(&format!("Requires-Python: {rp}\n"));
    }
    for req in &meta.requires {
        out.push_str(&format!("Requires-Dist: {req}\n"));
    }
    out
}

// ---------------------------------------------------------------------------
// The stored-only zip encoder
// ---------------------------------------------------------------------------

// 1980-01-01 00:00:00, the earliest DOS timestamp — the fixed value that
// makes archives reproducible.
const DOS_DATE: u16 = 0x0021;
const DOS_TIME: u16 = 0;

/// Encode `entries` (already sorted) as a stored-only zip.
fn zip_stored(entries: &[WheelFile]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    let mut count: u16 = 0;

    for entry in entries {
        let crc = crc32fast::hash(&entry.bytes);
        let name = entry.arcname.as_bytes();
        let size = entry.bytes.len() as u32;
        let offset = out.len() as u32;

        // Local file header.
        out.write_all(&0x04034b50u32.to_le_bytes()).unwrap();
        out.write_all(&20u16.to_le_bytes()).unwrap(); // version needed
        out.write_all(&0u16.to_le_bytes()).unwrap(); // flags
        out.write_all(&0u16.to_le_bytes()).unwrap(); // method: stored
        out.write_all(&DOS_TIME.to_le_bytes()).unwrap();
        out.write_all(&DOS_DATE.to_le_bytes()).unwrap();
        out.write_all(&crc.to_le_bytes()).unwrap();
        out.write_all(&size.to_le_bytes()).unwrap(); // compressed
        out.write_all(&size.to_le_bytes()).unwrap(); // uncompressed
        out.write_all(&(name.len() as u16).to_le_bytes()).unwrap();
        out.write_all(&0u16.to_le_bytes()).unwrap(); // extra len
        out.write_all(name).unwrap();
        out.write_all(&entry.bytes).unwrap();

        // Central directory entry.
        central.write_all(&0x02014b50u32.to_le_bytes()).unwrap();
        // Made by: unix (3) << 8 | zip spec 2.0 — carries the external attrs.
        central.write_all(&0x031eu16.to_le_bytes()).unwrap();
        central.write_all(&20u16.to_le_bytes()).unwrap();
        central.write_all(&0u16.to_le_bytes()).unwrap();
        central.write_all(&0u16.to_le_bytes()).unwrap();
        central.write_all(&DOS_TIME.to_le_bytes()).unwrap();
        central.write_all(&DOS_DATE.to_le_bytes()).unwrap();
        central.write_all(&crc.to_le_bytes()).unwrap();
        central.write_all(&size.to_le_bytes()).unwrap();
        central.write_all(&size.to_le_bytes()).unwrap();
        central
            .write_all(&(name.len() as u16).to_le_bytes())
            .unwrap();
        central.write_all(&0u16.to_le_bytes()).unwrap(); // extra
        central.write_all(&0u16.to_le_bytes()).unwrap(); // comment
        central.write_all(&0u16.to_le_bytes()).unwrap(); // disk
        central.write_all(&0u16.to_le_bytes()).unwrap(); // internal attrs
        let external = (0o100000 | entry.mode) << 16; // regular file + mode
        central.write_all(&external.to_le_bytes()).unwrap();
        central.write_all(&offset.to_le_bytes()).unwrap();
        central.write_all(name).unwrap();
        count += 1;
    }

    let cd_offset = out.len() as u32;
    let cd_size = central.len() as u32;
    out.extend_from_slice(&central);

    // End of central directory.
    out.write_all(&0x06054b50u32.to_le_bytes()).unwrap();
    out.write_all(&0u16.to_le_bytes()).unwrap(); // disk
    out.write_all(&0u16.to_le_bytes()).unwrap(); // cd disk
    out.write_all(&count.to_le_bytes()).unwrap();
    out.write_all(&count.to_le_bytes()).unwrap();
    out.write_all(&cd_size.to_le_bytes()).unwrap();
    out.write_all(&cd_offset.to_le_bytes()).unwrap();
    out.write_all(&0u16.to_le_bytes()).unwrap(); // comment len
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> WheelMeta {
        WheelMeta {
            dist_name: "Adcs-Pack".into(),
            version: "1.2.0".into(),
            tag: "py3-none-any".into(),
            requires_python: Some(">=3.11".into()),
            requires: vec!["metor-fsw-abi==8".into(), "metor-config>=0.3,<0.4".into()],
        }
    }

    fn files() -> Vec<WheelFile> {
        vec![
            WheelFile {
                arcname: "adcs_pack/_libs/x/libx.so".into(),
                bytes: vec![1, 2, 3],
                mode: 0o755,
            },
            WheelFile {
                arcname: "adcs_pack/__init__.py".into(),
                bytes: b"# module\n".to_vec(),
                mode: 0o644,
            },
        ]
    }

    #[test]
    fn normalization_is_pep503() {
        assert_eq!(normalize_dist("Adcs-Pack"), "adcs_pack");
        assert_eq!(normalize_dist("a.b--c_d"), "a_b_c_d");
    }

    /// Identical inputs produce byte-identical wheels, regardless of the
    /// order the payload files were supplied in.
    #[test]
    fn wheels_are_reproducible() {
        let dir = tempfile::tempdir().unwrap();
        let (a_dir, b_dir) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::create_dir_all(&a_dir).unwrap();
        std::fs::create_dir_all(&b_dir).unwrap();
        let a = write_wheel(&a_dir, &meta(), files()).unwrap();
        let mut reversed = files();
        reversed.reverse();
        let b = write_wheel(&b_dir, &meta(), reversed).unwrap();
        assert_eq!(
            a.file_name(),
            Some("adcs_pack-1.2.0-py3-none-any.whl".as_ref())
        );
        assert_eq!(std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap());
    }

    /// Python's own zip machinery accepts the archive, sees the fixed
    /// timestamps and modes, and the RECORD digests verify.
    #[test]
    #[cfg(not(miri))]
    fn python_zipfile_reads_it_back() {
        let dir = tempfile::tempdir().unwrap();
        let wheel = write_wheel(dir.path(), &meta(), files()).unwrap();
        let check = std::process::Command::new("python3")
            .arg("-c")
            .arg(
                r#"
import base64, hashlib, sys, zipfile
zf = zipfile.ZipFile(sys.argv[1])
assert zf.testzip() is None
names = zf.namelist()
assert names[:-1] == sorted(names[:-1]), names
assert names[-1].endswith("RECORD"), names
record = zf.read("adcs_pack-1.2.0.dist-info/RECORD").decode()
for line in record.strip().splitlines():
    path, digest, _size = line.rsplit(",", 2)
    if not digest:
        continue
    algo, b64 = digest.split("=", 1)
    actual = base64.urlsafe_b64encode(hashlib.new(algo, zf.read(path)).digest()).rstrip(b"=").decode()
    assert actual == b64, path
info = zf.getinfo("adcs_pack/_libs/x/libx.so")
assert (info.external_attr >> 16) & 0o777 == 0o755
assert info.date_time == (1980, 1, 1, 0, 0, 0)
meta = zf.read("adcs_pack-1.2.0.dist-info/METADATA").decode()
assert "Requires-Dist: metor-fsw-abi==8" in meta
assert "Requires-Python: >=3.11" in meta
print("ok")
"#,
            )
            .arg(&wheel)
            .output()
            .expect("python3 runs");
        assert!(
            check.status.success(),
            "{}",
            String::from_utf8_lossy(&check.stderr)
        );
    }
}
