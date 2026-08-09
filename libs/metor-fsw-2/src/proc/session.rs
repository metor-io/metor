//! The per-run session directory: shared ring files, control blocks, and
//! worker manifests for one coordinator's process systems.

use std::path::{Path, PathBuf};

/// The directory holding everything a run shares with its workers, owned by
/// the [`Coordinator`](crate::Coordinator) and removed best-effort on drop —
/// its contents are ephemeral IPC state, never archives. On unix, unlinking
/// still-mapped files is legal, so removal never races a straggling worker's
/// mappings.
pub(crate) struct SessionDir {
    dir: tempfile::TempDir,
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
    /// default root).
    pub(crate) fn create(root: Option<&Path>) -> std::io::Result<Self> {
        let root = root.map(Path::to_path_buf).unwrap_or_else(default_root);
        let dir = tempfile::Builder::new()
            .prefix("metor-fsw-")
            .tempdir_in(root)?;
        Ok(Self { dir })
    }

    pub(crate) fn path(&self) -> &Path {
        self.dir.path()
    }
}
