//! Session ownership and shutdown. Saved snapshots are independent artifacts;
//! only explicitly selected recording directories survive as live DB storage,
//! plus the per-peer cache a single-peer mirror reopens next time.
use gpui::App;
use metor_db::DB;
use metor_db::scratch::{self, ScratchDir};
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, atomic::AtomicBool};

mod archive;
mod commands;
mod peer;
mod usage;
pub(crate) use peer::{CachedPeer, select_for_peer};
pub(crate) mod startup;
pub(crate) use commands::{
    OpenHandler, OpenRecording, QuitPanel, SaveSnapshot, SaveSnapshotAs, cached_peer,
    finish_export, init, open_recording, request_quit, rows, save, save_as, save_workspace, take,
};

pub(crate) struct SessionStorage {
    directory: StorageDirectory,
    source: Option<PathBuf>,
}
/// The temp-dir prefix of a session's own database.
const SESSION_PREFIX: &str = "metor-panel-";
/// The temp-dir prefix of an opened recording's working copy.
const IMPORT_PREFIX: &str = "metor-open-";

/// Removes the temporary session directories that crashed or killed panels
/// left behind, on a background thread so startup doesn't wait for it.
pub(crate) fn sweep_stale() {
    std::thread::spawn(|| {
        scratch::sweep(SESSION_PREFIX);
        scratch::sweep(IMPORT_PREFIX);
    });
}

enum StorageDirectory {
    Temporary(ScratchDir),
    Recording {
        path: PathBuf,
        _lock: File,
    },
    /// One peer's mirror, kept between sessions in the peer cache and
    /// locked while this session runs. A cache, not a record: flushed and
    /// kept on quit so the next session reopens it, never swept as stale,
    /// evicted while running like a temporary session, and removed only by
    /// the cache's own pruning once unused. Snapshots save from it like
    /// from any session.
    Cached(metor_db::peer_cache::CachedDb, CachedPeer),
}
impl StorageDirectory {
    fn path(&self) -> &Path {
        match self {
            Self::Temporary(d) => d.path(),
            Self::Recording { path, .. } => path,
            Self::Cached(cached, _) => cached.path(),
        }
    }
}
#[derive(serde::Serialize, serde::Deserialize)]
struct RecordingManifest {
    format: String,
    version: u32,
    created_unix_micros: u128,
}
impl SessionStorage {
    pub(crate) fn create() -> Result<(Self, Arc<DB>), metor_db::Error> {
        let directory = ScratchDir::create(SESSION_PREFIX)?;
        let db = Arc::new(DB::create(directory.path().to_owned())?);
        // Deleted on quit, so a mirror of it when it is served must not
        // cache it or count on it for history.
        db.set_ephemeral(true);
        Ok((
            Self {
                directory: StorageDirectory::Temporary(directory),
                source: None,
            },
            db,
        ))
    }
    pub(crate) fn record_to(path: PathBuf) -> Result<(Self, Arc<DB>), metor_db::Error> {
        let path = bundle_path(path)?;
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let path = parent.canonicalize()?.join(path.file_name().unwrap());
        // Reserve the exact name atomically before DB::create, which itself
        // accepts existing directories. Never initialize over another recording.
        fs::create_dir(&path)?;
        let result = (|| {
            let lock = recording_lock(&path)?;
            let manifest = RecordingManifest {
                format: "metor-recording".into(),
                version: 1,
                created_unix_micros: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_micros(),
            };
            fs::write(
                path.join("manifest.json"),
                serde_json::to_vec(&manifest).map_err(io::Error::other)?,
            )?;
            let db = Arc::new(DB::create(path.join("db"))?);
            sync_tree(&path)?;
            sync_directory(
                path.parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or(Path::new(".")),
            )?;
            Ok((
                Self {
                    directory: StorageDirectory::Recording {
                        path: path.clone(),
                        _lock: lock,
                    },
                    source: None,
                },
                db,
            ))
        })();
        if result.is_err() {
            // Only this attempt owns this exclusively created directory; no
            // producers have been started yet.
            let _ = fs::remove_dir_all(&path);
        }
        result
    }

    #[cfg(test)]
    pub(crate) fn is_temporary(&self) -> bool {
        matches!(self.directory, StorageDirectory::Temporary(_))
    }

    pub(crate) fn is_recording(&self) -> bool {
        matches!(self.directory, StorageDirectory::Recording { .. })
    }

    /// Whether what the session holds may be dropped once a peer can give
    /// it back: a temporary session or a peer cache, never a recording.
    pub(crate) fn is_cache(&self) -> bool {
        matches!(
            self.directory,
            StorageDirectory::Temporary(_) | StorageDirectory::Cached(..)
        )
    }

    pub(crate) fn import(
        path: PathBuf,
        cancel: &AtomicBool,
    ) -> io::Result<(Self, Arc<DB>, archive::Imported)> {
        let directory = ScratchDir::create(IMPORT_PREFIX)?;
        // The import fills an empty directory; the scratch root holds its lock.
        let bundle = directory.path().join("bundle");
        fs::create_dir(&bundle)?;
        let imported = archive::import(&path, &bundle, cancel)?;
        let db = Arc::new(DB::open(bundle.join("db")).map_err(io::Error::other)?);
        let mut actual_gaps = db.snapshot_gaps();
        let mut recorded_gaps = imported.gaps.clone();
        actual_gaps.sort_by_key(|g| (g.component, g.start, g.end));
        recorded_gaps.sort_by_key(|g| (g.component, g.start, g.end));
        if actual_gaps != recorded_gaps {
            return Err(io::Error::other(
                "Recording coverage does not match the snapshot manifest",
            ));
        }
        let source = Some(path.canonicalize()?);
        Ok((
            Self {
                directory: StorageDirectory::Temporary(directory),
                source,
            },
            db,
            imported,
        ))
    }

    pub(crate) fn finish(self, db: &DB, shutdown: io::Result<()>) -> Result<(), FinishError> {
        // Nothing may write the directory once it is removed, and the flush
        // below must see every sample the persistence thread would have.
        db.stop_persistence();
        match self.directory {
            StorageDirectory::Temporary(directory) => {
                let path = directory.path().to_owned();
                let removed = match shutdown {
                    Ok(()) => directory.remove(),
                    // Kept for recovery: a sweep must not take it either.
                    Err(error) => directory.keep().and(Err(error)),
                };
                removed.map_err(|error| FinishError {
                    recovery: path,
                    error,
                })
            }
            StorageDirectory::Recording { path, _lock } => shutdown
                .and_then(|()| {
                    db.flush().map_err(io::Error::other)?;
                    sync_tree(&path)
                })
                .map_err(|error| FinishError {
                    recovery: path,
                    error,
                }),
            // No fsync: a cache torn by a crash is replaced when it next
            // fails to open, and its history fetched again.
            StorageDirectory::Cached(cached, _) => shutdown
                .and_then(|()| db.flush().map_err(io::Error::other))
                .map_err(|error| FinishError {
                    recovery: cached.path().to_owned(),
                    error,
                }),
        }
    }
}

/// The OS picker for opening a recording: one bundle file or directory.
fn open_recording_prompt() -> gpui::PathPromptOptions {
    gpui::PathPromptOptions {
        files: true,
        directories: true,
        multiple: false,
        prompt: Some("Open recording".into()),
    }
}

fn recording_lock(path: &Path) -> io::Result<File> {
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.join(".recording.lock"))?;
    file.try_lock().map_err(|error| io::Error::other(format!(
        "Recording is in use; save a snapshot from the recording panel before opening it: {error}"
    )))?;
    Ok(file)
}

#[derive(Debug, thiserror::Error)]
#[error("{error}; database retained at {recovery}")]
pub(crate) struct FinishError {
    recovery: PathBuf,
    error: io::Error,
}

fn bundle_path(mut path: PathBuf) -> io::Result<PathBuf> {
    let Some(name) = path.file_name() else {
        return Err(io::Error::other("Choose a name for the database bundle"));
    };
    if path.extension().is_none_or(|ext| ext != "metor") {
        let mut name = name.to_os_string();
        name.push(".metor");
        path.set_file_name(name);
    }
    Ok(path)
}

/// Persist copied files, metadata sidecars, and directory entries before
/// publishing a bundle.
fn sync_tree(path: &Path) -> io::Result<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            sync_tree(&entry.path())?;
        } else if kind.is_file() {
            File::options()
                .read(true)
                .write(true)
                .open(entry.path())?
                .sync_all()?;
        } else {
            return Err(io::Error::other("Unexpected non-file in session database"));
        }
    }
    sync_directory(path)
}

fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests;
