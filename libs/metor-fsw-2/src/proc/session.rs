//! The per-run session directory: shared ring files, control blocks, and
//! worker manifests for one coordinator's process systems.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// The directory holding everything a run shares with its workers, owned by
/// the [`Coordinator`](crate::Coordinator) and removed best-effort on drop —
/// its contents are ephemeral IPC state, never archives. On unix, unlinking
/// still-mapped files is legal, so removal never races a straggling worker's
/// mappings.
pub(crate) struct SessionDir {
    path: PathBuf,
}

/// The default root: `/dev/shm` when it exists (a tmpfs, so ring traffic
/// never touches a disk), else the OS temp dir.
fn default_root() -> PathBuf {
    let shm = Path::new("/dev/shm");
    if shm.is_dir() {
        shm.to_path_buf()
    } else {
        std::env::temp_dir()
    }
}

impl SessionDir {
    /// Create a fresh, uniquely named session directory under `root` (or the
    /// default root). The pid plus a process-wide counter keep concurrent
    /// coordinators in one process apart.
    pub(crate) fn create(root: Option<&Path>) -> std::io::Result<Self> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let root = root.map(Path::to_path_buf).unwrap_or_else(default_root);
        let path = root.join(format!(
            "metor-fsw-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Relaxed)
        ));
        std::fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for SessionDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
