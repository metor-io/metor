//! Session ownership and shutdown. Saved snapshots are independent artifacts;
//! only explicitly selected recording directories survive as live DB storage.
use gpui::App;
use metor_db::DB;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, atomic::AtomicBool};
use tempfile::TempDir;

mod archive;
mod commands;
pub(crate) mod startup;
pub(crate) use commands::{
    OpenHandler, OpenRecording, QuitPanel, SaveSnapshot, SaveSnapshotAs, finish_export, init,
    open_recording, request_quit, rows, save, save_as, save_workspace, take,
};

pub(crate) struct SessionStorage {
    // Field order ensures an abandoned import stops its executor before its
    // temporary directory is removed, even if the panel was never run.
    runtime: Option<ImportRuntime>,
    directory: StorageDirectory,
    source: Option<PathBuf>,
}
enum StorageDirectory {
    Temporary(TempDir),
    Recording { path: PathBuf, _lock: File },
}
impl StorageDirectory {
    fn path(&self) -> &Path {
        match self {
            Self::Temporary(d) => d.path(),
            Self::Recording { path, .. } => path,
        }
    }
}
struct ImportRuntime(crate::background_tasks::BackgroundTasks);
impl Drop for ImportRuntime {
    fn drop(&mut self) {
        if let Err(error) = self.0.shutdown() {
            tracing::error!(%error, "import executor shutdown failed");
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
        let directory = tempfile::Builder::new().prefix("metor-panel-").tempdir()?;
        let db = Arc::new(DB::create(directory.path().to_owned())?);
        Ok((
            Self {
                directory: StorageDirectory::Temporary(directory),
                runtime: None,
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
                    runtime: None,
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

    fn is_temporary(&self) -> bool {
        matches!(self.directory, StorageDirectory::Temporary(_))
    }

    pub(crate) fn import(
        path: PathBuf,
        cancel: &AtomicBool,
    ) -> io::Result<(Self, Arc<DB>, archive::Imported)> {
        let directory = tempfile::Builder::new().prefix("metor-open-").tempdir()?;
        let imported = archive::import(&path, directory.path(), cancel)?;
        let runtime = ImportRuntime(Default::default());
        let db_path = directory.path().join("db");
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        runtime
            .0
            .spawn(stellarator::util::CancelToken::new(), move || async move {
                let result = DB::open(db_path).map(Arc::new);
                let ok = result.is_ok();
                if tx.send(result).is_ok() && ok {
                    std::future::pending::<()>().await;
                }
            });
        let db = rx
            .recv()
            .map_err(io::Error::other)?
            .map_err(io::Error::other)?;
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
                runtime: Some(runtime),
                directory: StorageDirectory::Temporary(directory),
                source,
            },
            db,
            imported,
        ))
    }

    pub(crate) fn finish(mut self, db: &DB, shutdown: io::Result<()>) -> Result<(), FinishError> {
        let shutdown = shutdown.and(
            self.runtime
                .take()
                .map(|runtime| runtime.0.shutdown().map_err(io::Error::other))
                .unwrap_or(Ok(())),
        );
        match self.directory {
            StorageDirectory::Temporary(directory) => {
                let path = directory.keep();
                shutdown
                    .and_then(|()| fs::remove_dir_all(&path))
                    .map_err(|error| FinishError {
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
        }
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
