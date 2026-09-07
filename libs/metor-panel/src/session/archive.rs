//! Compact uncompressed tar snapshots. Only the format's regular-file entries
//! are accepted; imported logs are validated before the DB maps any bytes.
use metor_db::snapshot::{HistoryGap, Snapshot};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{self, BufReader, Read, Seek, SeekFrom, Write},
    path::Path,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::SystemTime,
};
use tar_core::{EntryBuilder, EntryType, Header};

const MAX_ENTRIES: usize = 100_000;
const MAX_METADATA: u64 = 16 * 1024 * 1024;
const MAX_ARCHIVE: u64 = 512 * 1024 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
struct Manifest {
    format: String,
    version: u32,
    byte_order: String,
    consistency: String,
    captured_at_micros: i64,
    capture_duration_micros: u64,
    gaps: Vec<HistoryGap>,
    entries: Vec<Entry>,
}
#[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
struct Entry {
    path: String,
    len: u64,
    sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Destination {
    Missing,
    File {
        len: u64,
        modified: SystemTime,
        #[cfg(unix)]
        device: u64,
        #[cfg(unix)]
        inode: u64,
    },
}
impl Destination {
    pub(super) fn inspect(path: &Path) -> io::Result<Self> {
        let meta = match fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::Missing),
            Err(error) => return Err(error),
        };
        if !meta.is_file() {
            return Err(invalid(
                "Choose a snapshot filename; directories and links cannot be replaced",
            ));
        }
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt as _;
        Ok(Self::File {
            len: meta.len(),
            modified: meta.modified()?,
            #[cfg(unix)]
            device: meta.dev(),
            #[cfg(unix)]
            inode: meta.ino(),
        })
    }
}

pub(super) fn save(
    snapshot: &Snapshot,
    workspace: Option<&str>,
    path: &Path,
    expected: &Destination,
    cancel: &AtomicBool,
    progress: &AtomicU64,
) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut staging = tempfile::NamedTempFile::new_in(parent)?;
    let mut entries = Vec::new();
    for file in snapshot.files() {
        entries.push(write_entry(
            staging.as_file_mut(),
            &file.path,
            file.parts(),
            cancel,
            progress,
        )?);
    }
    if let Some(workspace) = workspace {
        entries.push(write_entry(
            staging.as_file_mut(),
            "workspace.json",
            [workspace.as_bytes(), &[]],
            cancel,
            progress,
        )?);
    }
    let manifest = Manifest {
        format: "metor-snapshot".into(),
        version: 1,
        byte_order: "little".into(),
        consistency: "independent committed prefixes".into(),
        captured_at_micros: snapshot.captured_at.0,
        capture_duration_micros: snapshot.capture_duration.as_micros().min(u64::MAX as u128) as u64,
        gaps: snapshot.gaps.clone(),
        entries,
    };
    let json = serde_json::to_vec(&manifest).map_err(invalid)?;
    if json.len() as u64 > MAX_METADATA {
        return Err(invalid("Snapshot has too many entries"));
    }
    write_entry(
        staging.as_file_mut(),
        "manifest.json",
        [&json, &[]],
        cancel,
        progress,
    )?;
    staging.write_all(&[0; 1024])?;
    staging.as_file().sync_all()?;
    cancelled(cancel)?;
    if &Destination::inspect(path)? != expected {
        return Err(invalid(
            "Destination changed while saving; choose Save As and try again",
        ));
    }
    match expected {
        Destination::Missing => {
            staging.persist_noclobber(path).map_err(|e| e.error)?;
        }
        Destination::File { .. } => {
            staging.persist(path).map_err(|e| e.error)?;
        }
    }
    super::sync_directory(parent)
}
fn write_entry(
    out: &mut File,
    path: &str,
    parts: [&[u8]; 2],
    cancel: &AtomicBool,
    progress: &AtomicU64,
) -> io::Result<Entry> {
    let len = parts.iter().map(|p| p.len() as u64).sum();
    let mut builder = EntryBuilder::new_ustar();
    builder
        .path(path.as_bytes())
        .size(len)
        .map_err(invalid)?
        .mode(0o600)
        .map_err(invalid)?
        .entry_type(EntryType::Regular);
    out.write_all(&builder.finish_bytes())?;
    let mut hash = Sha256::new();
    for part in parts {
        for bytes in part.chunks(64 * 1024) {
            cancelled(cancel)?;
            out.write_all(bytes)?;
            hash.update(bytes);
            progress.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        }
    }
    out.write_all(&[0; 512][..padding(len)])?;
    Ok(Entry {
        path: path.into(),
        len,
        sha256: hex(&hash.finalize()),
    })
}

pub(crate) struct Imported {
    pub workspace: Option<String>,
    pub gaps: Vec<HistoryGap>,
}
/// `target` must be a fresh managed temporary directory. Callers remove it on
/// every failure by retaining their TempDir guard until validation succeeds.
pub(super) fn import(source: &Path, target: &Path, cancel: &AtomicBool) -> io::Result<Imported> {
    if fs::read_dir(target)?.next().is_some() {
        return Err(invalid("Import target must be empty"));
    }
    let meta = fs::symlink_metadata(source)?;
    if meta.file_type().is_symlink() {
        return Err(invalid(
            "Choose a recording file or directory, not a symbolic link",
        ));
    }
    let gaps = if meta.is_dir() {
        import_directory(source, target, cancel)?
    } else if meta.is_file() {
        import_tar(source, target, cancel)?
    } else {
        return Err(invalid("Not a recording file"));
    };
    validate_database(&target.join("db"), cancel)?;
    let workspace = match fs::read_to_string(target.join("workspace.json")) {
        Ok(json) => Some(json),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    Ok(Imported { workspace, gaps })
}

fn import_tar(source: &Path, target: &Path, cancel: &AtomicBool) -> io::Result<Vec<HistoryGap>> {
    let mut input = File::open(source)?;
    let file_len = input.metadata()?.len();
    if file_len > MAX_ARCHIVE {
        return Err(invalid("Snapshot exceeds the 512 GiB import limit"));
    }
    let mut entries = BTreeMap::new();
    let mut manifest = None;
    loop {
        cancelled(cancel)?;
        let mut block = [0; 512];
        input.read_exact(&mut block)?;
        if block == [0; 512] {
            input.read_exact(&mut block)?;
            if block != [0; 512] {
                return Err(invalid("Incomplete tar trailer"));
            }
            let mut rest = [0; 64 * 1024];
            loop {
                let n = input.read(&mut rest)?;
                if n == 0 {
                    break;
                }
                cancelled(cancel)?;
                if rest[..n].iter().any(|b| *b != 0) {
                    return Err(invalid("Unexpected data after tar trailer"));
                }
            }
            break;
        }
        let header = Header::from_bytes(&block);
        header.verify_checksum().map_err(invalid)?;
        if !header.is_ustar()
            || header.entry_type() != EntryType::Regular
            || block[345..500].iter().any(|b| *b != 0)
            || !header.link_name_bytes().is_empty()
        {
            return Err(invalid(
                "Only plain regular-file snapshot entries are supported",
            ));
        }
        let path = std::str::from_utf8(header.path_bytes())
            .map_err(invalid)?
            .to_owned();
        let kind =
            classify(&path).ok_or_else(|| invalid(format!("Unexpected snapshot entry: {path}")))?;
        let len = header.entry_size().map_err(invalid)?;
        if len > file_len.saturating_sub(input.stream_position()?)
            || (matches!(kind, Kind::Metadata) && len > MAX_METADATA)
        {
            return Err(invalid("Invalid snapshot entry length"));
        }
        if entries.len() >= MAX_ENTRIES || entries.contains_key(&path) || manifest.is_some() {
            return Err(invalid(
                "Duplicate, excessive, or out-of-order snapshot entries",
            ));
        }
        let destination = target.join(&path);
        fs::create_dir_all(destination.parent().unwrap())?;
        let mut output = File::options()
            .write(true)
            .create_new(true)
            .open(&destination)?;
        let sha256 = copy_exact(&mut input, &mut output, len, cancel)?;
        let entry = Entry {
            path: path.clone(),
            len,
            sha256,
        };
        if path == "manifest.json" {
            let parsed: Manifest =
                serde_json::from_slice(&read_metadata(&destination)?).map_err(invalid)?;
            if parsed.format != "metor-snapshot"
                || parsed.version != 1
                || parsed.byte_order != "little"
            {
                return Err(invalid(
                    "Unsupported snapshot format, version, or byte order",
                ));
            }
            manifest = Some(parsed);
        } else {
            entries.insert(path, entry);
        }
        let mut padding_bytes = [0; 512];
        input.read_exact(&mut padding_bytes[..padding(len)])?;
        if padding_bytes.iter().any(|b| *b != 0) {
            return Err(invalid("Nonzero tar padding"));
        }
    }
    let manifest = manifest.ok_or_else(|| invalid("Missing snapshot manifest"))?;
    if manifest.entries.len() != entries.len() {
        return Err(invalid("Snapshot inventory does not match archive"));
    }
    for entry in &manifest.entries {
        if entries.remove(&entry.path).as_ref() != Some(entry) {
            return Err(invalid(format!(
                "Snapshot checksum or length mismatch: {}",
                entry.path
            )));
        }
    }
    Ok(manifest.gaps)
}

fn import_directory(
    source: &Path,
    target: &Path,
    cancel: &AtomicBool,
) -> io::Result<Vec<HistoryGap>> {
    let envelope = source.join("manifest.json").exists();
    let _lock = if envelope {
        let manifest: serde_json::Value =
            serde_json::from_slice(&read_metadata(&source.join("manifest.json"))?)
                .map_err(invalid)?;
        if manifest["format"] != "metor-recording" || manifest["version"] != 1 {
            return Err(invalid("Unsupported recording directory"));
        }
        if !fs::symlink_metadata(source.join(".recording.lock"))?.is_file() {
            return Err(invalid("Invalid recording lock file"));
        }
        let lock = File::open(source.join(".recording.lock"))?;
        lock.try_lock_shared()
            .map_err(|_| invalid("Recording is in use; save a snapshot from its panel instead"))?;
        Some(lock)
    } else {
        None
    };
    let db_source = if envelope {
        source.join("db")
    } else {
        source.to_owned()
    };
    if !fs::symlink_metadata(&db_source)?.is_dir() {
        return Err(invalid("Recording DB must be a directory, not a link"));
    }
    let mut count = 0;
    copy_directory(&db_source, "db", target, cancel, &mut count)?;
    if envelope && source.join("workspace.json").exists() {
        fs::write(
            target.join("workspace.json"),
            read_metadata(&source.join("workspace.json"))?,
        )?;
    }
    let mut gaps = Vec::new();
    for entry in fs::read_dir(target.join("db"))? {
        let entry = entry?;
        if entry.file_type()?.is_dir()
            && let Ok(component) = entry.file_name().to_string_lossy().parse::<u64>()
            && let Some(manifest) =
                metor_db::manifest::ComponentManifest::read_from(&entry.path()).map_err(invalid)?
        {
            gaps.extend(
                manifest
                    .spans
                    .iter()
                    .filter(|s| s.state != metor_db::manifest::SpanState::Resident)
                    .map(|s| HistoryGap {
                        component,
                        start: s.seal.start_ts.0,
                        end: s.cover_end.0,
                    }),
            );
        }
    }
    Ok(gaps)
}
fn copy_directory(
    source: &Path,
    prefix: &str,
    target: &Path,
    cancel: &AtomicBool,
    count: &mut usize,
) -> io::Result<()> {
    for entry in fs::read_dir(source)? {
        cancelled(cancel)?;
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| invalid("Invalid recording path"))?;
        let path = format!("{prefix}/{name}");
        let kind = entry.file_type()?;
        if kind.is_dir() {
            // Native DB staging directories and temporary sidecars are not data.
            if name.starts_with('.') {
                continue;
            }
            let depth = path.split('/').count();
            if depth > 4 || (name != "msgs" && name.parse::<i128>().is_err()) {
                return Err(invalid("Unexpected recording directory"));
            }
            copy_directory(&entry.path(), &path, target, cancel, count)?;
        } else if kind.is_file() {
            if name.ends_with(".tmp") {
                continue;
            }
            let kind = classify(&path)
                .ok_or_else(|| invalid(format!("Unexpected recording file: {path}")))?;
            *count += 1;
            if *count > MAX_ENTRIES {
                return Err(invalid("Too many recording files"));
            }
            let destination = target.join(&path);
            fs::create_dir_all(destination.parent().unwrap())?;
            let mut input = File::open(entry.path())?;
            let size = input.metadata()?.len();
            let len = match kind {
                Kind::Metadata => {
                    if size > MAX_METADATA {
                        return Err(invalid("Recording metadata is too large"));
                    }
                    size
                }
                Kind::Log(header) => {
                    let mut bytes = [0; 8];
                    input.read_exact(&mut bytes)?;
                    let len = u64::from_le_bytes(bytes);
                    if len < header || len > size || len > MAX_ARCHIVE {
                        return Err(invalid("Invalid native log length"));
                    }
                    input.rewind()?;
                    len
                }
            };
            let mut output = File::options()
                .write(true)
                .create_new(true)
                .open(destination)?;
            copy_exact(&mut input, &mut output, len, cancel)?;
        } else {
            return Err(invalid("Recording contains a link or special file"));
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum Kind {
    Metadata,
    Log(u64),
}
fn classify(path: &str) -> Option<Kind> {
    let names: Vec<_> = path.split('/').collect();
    match names.as_slice() {
        ["manifest.json" | "workspace.json"] | ["db", "db_state"] => Some(Kind::Metadata),
        ["db", component, "schema" | "metadata" | "manifest"] if number::<u64>(component) => {
            Some(Kind::Metadata)
        }
        ["db", component, start, "index" | "data"]
            if number::<u64>(component) && number::<i64>(start) =>
        {
            Some(Kind::Log(24))
        }
        ["db", component, start, "seal"] if number::<u64>(component) && number::<i64>(start) => {
            Some(Kind::Metadata)
        }
        ["db", "msgs", id, "metadata"] if number::<u16>(id) => Some(Kind::Metadata),
        [
            "db",
            "msgs",
            id,
            start,
            "timestamps" | "offsets" | "data_log",
        ] if number::<u16>(id) && number::<i64>(start) => Some(Kind::Log(16)),
        _ => None,
    }
}
fn number<T: std::str::FromStr + ToString>(text: &str) -> bool {
    text.parse::<T>()
        .is_ok_and(|value| value.to_string() == text)
}
fn padding(len: u64) -> usize {
    ((512 - len % 512) % 512) as usize
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn invalid(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}
fn cancelled(cancel: &AtomicBool) -> io::Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "Snapshot cancelled",
        ))
    } else {
        Ok(())
    }
}
fn read_metadata(path: &Path) -> io::Result<Vec<u8>> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_file() {
        return Err(invalid("Metadata must be a regular file"));
    }
    if meta.len() > MAX_METADATA {
        return Err(invalid("Metadata is too large"));
    }
    fs::read(path)
}
fn copy_exact(
    input: &mut impl Read,
    output: &mut impl Write,
    mut len: u64,
    cancel: &AtomicBool,
) -> io::Result<String> {
    let mut hash = Sha256::new();
    let mut bytes = [0; 64 * 1024];
    while len > 0 {
        cancelled(cancel)?;
        let count = bytes.len().min(len as usize);
        input.read_exact(&mut bytes[..count])?;
        output.write_all(&bytes[..count])?;
        hash.update(&bytes[..count]);
        len -= count as u64;
    }
    Ok(hex(&hash.finalize()))
}

fn validate_database(path: &Path, cancel: &AtomicBool) -> io::Result<()> {
    // Implemented below: validation precedes opening any native mmap.
    validation::database(path, cancel)
}
mod validation;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{
        SessionStorage,
        tests::{assert_bundle, populate},
    };
    use metor_db::DB;
    use std::sync::Arc;

    fn fixture() -> (tempfile::TempDir, Arc<DB>) {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(DB::create(dir.path().join("db")).unwrap());
        populate(&db);
        db.flush().unwrap();
        (dir, db)
    }
    fn write_snapshot(db: &DB, path: &Path) {
        save(
            &db.snapshot().unwrap(),
            Some("{\"layout\":\"test\"}"),
            path,
            &Destination::Missing,
            &AtomicBool::new(false),
            &AtomicU64::new(0),
        )
        .unwrap();
    }
    #[test]
    fn compact_tar_round_trips_samples_messages_metadata_and_workspace() {
        let (dir, db) = fixture();
        let path = dir.path().join("snapshot.metor");
        write_snapshot(&db, &path);
        assert!(fs::metadata(&path).unwrap().len() < 64 * 1024);
        let imported = tempfile::tempdir().unwrap();
        let result = import(&path, imported.path(), &AtomicBool::new(false)).unwrap();
        assert_eq!(result.workspace.as_deref(), Some("{\"layout\":\"test\"}"));
        assert!(result.gaps.is_empty());
        assert_bundle(&imported.path().join("db"));
        let original = fs::read(&path).unwrap();
        let (copy, copy_db, _) =
            SessionStorage::import(path.clone(), &AtomicBool::new(false)).unwrap();
        let copy_path = copy.directory.path().to_owned();
        copy.finish(&copy_db, Ok(())).unwrap();
        assert!(!copy_path.exists());
        assert_eq!(original, fs::read(path).unwrap());
    }
    #[test]
    fn legacy_directories_are_copied_compactly() {
        let (_dir, db) = fixture();
        let (copy, copy_db, _) =
            SessionStorage::import(db.path.clone(), &AtomicBool::new(false)).unwrap();
        assert!(
            fs::metadata(copy_db.path.join("42/123/data"))
                .unwrap()
                .len()
                < 1024
        );
        copy.finish(&copy_db, Ok(())).unwrap();
        assert_bundle(&db.path);
    }
    #[test]
    fn cancelled_failed_and_changed_destination_saves_preserve_existing_data() {
        let (dir, db) = fixture();
        let snapshot = db.snapshot().unwrap();
        let path = dir.path().join("snapshot.metor");
        fs::write(&path, b"previous").unwrap();
        let expected = Destination::inspect(&path).unwrap();
        assert!(
            save(
                &snapshot,
                None,
                &path,
                &expected,
                &AtomicBool::new(true),
                &AtomicU64::new(0)
            )
            .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), b"previous");
        fs::write(&path, b"changed by another writer").unwrap();
        assert!(
            save(
                &snapshot,
                None,
                &path,
                &expected,
                &AtomicBool::new(false),
                &AtomicU64::new(0)
            )
            .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), b"changed by another writer");
        assert!(
            save(
                &snapshot,
                None,
                &path,
                &Destination::Missing,
                &AtomicBool::new(false),
                &AtomicU64::new(0)
            )
            .is_err()
        );
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
        let expected = Destination::inspect(&path).unwrap();
        save(
            &snapshot,
            None,
            &path,
            &expected,
            &AtomicBool::new(false),
            &AtomicU64::new(0),
        )
        .unwrap();
        let imported = tempfile::tempdir().unwrap();
        import(&path, imported.path(), &AtomicBool::new(false)).unwrap();
    }
    #[test]
    fn malformed_paths_links_duplicates_truncation_and_corruption_are_rejected() {
        let (dir, db) = fixture();
        let valid = dir.path().join("valid.metor");
        write_snapshot(&db, &valid);
        let original = fs::read(&valid).unwrap();
        let reject = |bytes: &[u8]| {
            let bad = dir.path().join("bad.metor");
            fs::write(&bad, bytes).unwrap();
            let output = tempfile::tempdir().unwrap();
            assert!(import(&bad, output.path(), &AtomicBool::new(false)).is_err());
        };
        reject(&original[..original.len() - 1024]);
        let mut corrupt = original.clone();
        corrupt[512] ^= 1;
        reject(&corrupt);
        for (path, ty) in [
            (b"../escape".as_slice(), EntryType::Regular),
            (b"db/db_state".as_slice(), EntryType::Symlink),
        ] {
            let mut builder = EntryBuilder::new_ustar();
            builder.path(path).size(0).unwrap().entry_type(ty);
            let mut bytes = builder.finish_bytes();
            bytes.extend_from_slice(&[0; 1024]);
            reject(&bytes);
        }
        let mut duplicate = original[..512 + padding(0)].to_vec();
        // Two empty db_state entries are already invalid before a manifest.
        let mut builder = EntryBuilder::new_ustar();
        builder.path(b"db/db_state").size(0).unwrap();
        duplicate.clear();
        duplicate.extend(builder.finish_bytes());
        duplicate.extend(builder.finish_bytes());
        duplicate.extend([0; 1024]);
        reject(&duplicate);
        let mut unsupported = original.clone();
        let needle = b"\"version\":1";
        let pos = unsupported
            .windows(needle.len())
            .rposition(|bytes| bytes == needle)
            .unwrap();
        unsupported[pos + needle.len() - 1] = b'2';
        reject(&unsupported);
    }
}
