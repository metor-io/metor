//! Temporary database directories that outlive a crashed owner only until
//! the next sweep.
//!
//! A [`ScratchDir`] holds an exclusive advisory lock on a file inside its
//! directory for as long as it lives. The OS releases that lock when the
//! owning process exits, however it exits, so [`sweep`] can tell a live
//! owner's directory from one a crash or kill left behind by trying to take
//! the lock. The pid written into the lock file is only for humans: pids get
//! reused, locks don't outlive their process.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// The lock file a live owner holds inside its directory.
const LOCK_FILE: &str = ".scratch.lock";

/// The marker [`ScratchDir::keep`] leaves, which [`sweep`] never removes.
const KEEP_FILE: &str = ".keep";

/// How old a directory without a lock file must be before [`sweep`] removes
/// it. Such directories come from versions that predate the lock; a day
/// keeps a long-running old process's directory safe through a restart of
/// something else.
const LEGACY_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// A uniquely named directory under the system temp dir, owned by this
/// process.
///
/// It is removed when dropped, by [`ScratchDir::remove`], or by a later
/// [`sweep`] once this process is gone. [`ScratchDir::keep`] hands it over
/// for good.
#[derive(Debug)]
pub struct ScratchDir {
    path: PathBuf,
    /// Held for the owner's lifetime; `None` once kept or removed.
    lock: Option<File>,
}

impl ScratchDir {
    /// Creates a directory named `prefix` followed by the pid and a random
    /// suffix in the system temp dir, and locks it.
    pub fn create(prefix: &str) -> io::Result<Self> {
        Self::create_in(&std::env::temp_dir(), prefix)
    }

    /// Creates a locked directory named `prefix` followed by the pid and a
    /// random suffix in `parent`.
    pub fn create_in(parent: &Path, prefix: &str) -> io::Result<Self> {
        let path = loop {
            let name = format!("{prefix}{}-{:08x}", std::process::id(), fastrand::u32(..));
            let path = parent.join(name);
            match fs::create_dir(&path) {
                Ok(()) => break path,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        };
        match Self::lock(&path) {
            Ok(lock) => Ok(Self {
                path,
                lock: Some(lock),
            }),
            Err(error) => {
                let _ = fs::remove_dir_all(&path);
                Err(error)
            }
        }
    }

    /// Writes and locks the lock file under a temporary name, then renames
    /// it into place, so a sweep never finds the lock file unlocked. Until
    /// then the directory has no lock file and is too new to sweep.
    fn lock(path: &Path) -> io::Result<File> {
        let staged = path.join(format!("{LOCK_FILE}.tmp"));
        let mut file = File::create_new(&staged)?;
        file.try_lock().map_err(io::Error::other)?;
        writeln!(file, "{}", std::process::id())?;
        fs::rename(&staged, path.join(LOCK_FILE))?;
        Ok(file)
    }

    /// The directory's path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Releases the directory so it is never removed, by this owner or by a
    /// sweep, and returns its path.
    ///
    /// If the keep marker can't be written, the directory is left in place
    /// unlocked, so the next sweep removes it.
    pub fn keep(mut self) -> io::Result<PathBuf> {
        let path = std::mem::take(&mut self.path);
        let lock = self.lock.take();
        File::create(path.join(KEEP_FILE))?;
        drop(lock);
        let _ = fs::remove_file(path.join(LOCK_FILE));
        Ok(path)
    }

    /// Removes the directory now, reporting what drop would ignore.
    pub fn remove(mut self) -> io::Result<()> {
        // The lock is held until the directory is gone, so no sweep races
        // this removal. Unix lets an open file's directory be removed.
        let removed = remove_lock_last(&self.path);
        self.lock = None;
        removed
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        if self.lock.take().is_some() {
            let _ = remove_lock_last(&self.path);
        }
    }
}

/// Removes `path` with its lock file going last. A removal cut short, such
/// as a large database whose process exits mid-delete, then still leaves a
/// lock file behind, so the next sweep finishes the job instead of
/// mistaking the remnant for a lockless legacy directory and waiting out
/// its age.
fn remove_lock_last(path: &Path) -> io::Result<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_name() == LOCK_FILE {
            continue;
        }
        if entry.file_type()?.is_dir() {
            fs::remove_dir_all(entry.path())?;
        } else {
            fs::remove_file(entry.path())?;
        }
    }
    fs::remove_dir_all(path)
}

/// Removes every directory in the system temp dir whose name starts with
/// `prefix` and whose owner has exited, returning the removed paths.
pub fn sweep(prefix: &str) -> Vec<PathBuf> {
    sweep_in(&std::env::temp_dir(), prefix)
}

/// Removes every directory directly in `parent` whose name starts with
/// `prefix` and whose owner has exited, returning the removed paths.
///
/// A directory is stale when its lock file can be locked, so its owner is
/// gone. A directory without a lock file predates [`ScratchDir`] and is
/// stale once it has not been modified for [`LEGACY_MAX_AGE`]. Kept
/// directories, symlinks, and anything that fails to read are left alone.
pub fn sweep_in(parent: &Path, prefix: &str) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(parent) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(prefix))
        // `DirEntry::file_type` doesn't follow symlinks.
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path())
        .filter(|path| remove_if_stale(path))
        .collect()
}

/// Removes `path` if its owner has exited, returning whether it did.
fn remove_if_stale(path: &Path) -> bool {
    if path.join(KEEP_FILE).exists() {
        return false;
    }
    let lock_path = path.join(LOCK_FILE);
    let removed = match fs::symlink_metadata(&lock_path) {
        Ok(meta) if meta.is_file() => {
            let Ok(lock) = File::open(&lock_path) else {
                return false;
            };
            if lock.try_lock().is_err() {
                return false;
            }
            // Removed while holding the dead owner's lock, so a second
            // sweeper can't take it in between.
            remove_lock_last(path)
        }
        Ok(_) => return false,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let age = fs::symlink_metadata(path)
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|modified| SystemTime::now().duration_since(modified).ok());
            if age.is_none_or(|age| age < LEGACY_MAX_AGE) {
                return false;
            }
            fs::remove_dir_all(path)
        }
        Err(_) => return false,
    };
    match removed {
        Ok(()) => {
            tracing::info!(path = %path.display(), "removed stale temporary database");
            true
        }
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "failed to remove stale temporary database");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PREFIX: &str = "scratch-test-";

    #[test]
    fn test_live_owner_survives_sweep() {
        let parent = tempfile::tempdir().expect("a temp dir");
        let live = ScratchDir::create_in(parent.path(), PREFIX).expect("creates");
        assert!(sweep_in(parent.path(), PREFIX).is_empty());
        assert!(live.path().is_dir());
        let pid = fs::read_to_string(live.path().join(LOCK_FILE)).expect("a lock file");
        assert_eq!(pid.trim(), std::process::id().to_string());
    }

    #[test]
    fn test_dead_owner_is_swept() {
        let parent = tempfile::tempdir().expect("a temp dir");
        let dir = ScratchDir::create_in(parent.path(), PREFIX).expect("creates");
        let path = dir.path().to_owned();
        // Dropping the lock without the directory is what a killed owner
        // leaves behind.
        let mut dir = std::mem::ManuallyDrop::new(dir);
        drop(dir.lock.take());
        assert_eq!(sweep_in(parent.path(), PREFIX), std::slice::from_ref(&path));
        assert!(!path.exists());
    }

    #[test]
    fn test_drop_and_remove_delete_the_dir() {
        let parent = tempfile::tempdir().expect("a temp dir");
        let dropped = ScratchDir::create_in(parent.path(), PREFIX).expect("creates");
        let dropped_path = dropped.path().to_owned();
        drop(dropped);
        assert!(!dropped_path.exists());
        let removed = ScratchDir::create_in(parent.path(), PREFIX).expect("creates");
        let removed_path = removed.path().to_owned();
        removed.remove().expect("removes");
        assert!(!removed_path.exists());
    }

    #[test]
    fn test_kept_dir_survives_owner_and_sweep() {
        let parent = tempfile::tempdir().expect("a temp dir");
        let dir = ScratchDir::create_in(parent.path(), PREFIX).expect("creates");
        let path = dir.keep().expect("keeps");
        assert!(sweep_in(parent.path(), PREFIX).is_empty());
        assert!(path.is_dir());
    }

    #[test]
    fn test_unrelated_and_fresh_legacy_dirs_are_untouched() {
        let parent = tempfile::tempdir().expect("a temp dir");
        let unrelated = parent.path().join("other-dir");
        fs::create_dir(&unrelated).expect("creates");
        File::create(unrelated.join(LOCK_FILE)).expect("creates");
        let legacy = parent.path().join(format!("{PREFIX}legacy"));
        fs::create_dir(&legacy).expect("creates");
        let file = parent.path().join(format!("{PREFIX}file"));
        File::create(&file).expect("creates");
        assert!(sweep_in(parent.path(), PREFIX).is_empty());
        assert!(unrelated.is_dir() && legacy.is_dir() && file.is_file());
    }

    #[test]
    fn test_old_legacy_dir_is_swept() {
        let parent = tempfile::tempdir().expect("a temp dir");
        let legacy = parent.path().join(format!("{PREFIX}legacy"));
        fs::create_dir(&legacy).expect("creates");
        let old = SystemTime::now() - LEGACY_MAX_AGE - Duration::from_secs(60);
        File::open(&legacy)
            .and_then(|dir| dir.set_modified(old))
            .expect("sets mtime");
        assert_eq!(
            sweep_in(parent.path(), PREFIX),
            std::slice::from_ref(&legacy)
        );
        assert!(!legacy.exists());
    }

    #[cfg(unix)]
    #[test]
    fn test_symlinked_dir_is_untouched() {
        let parent = tempfile::tempdir().expect("a temp dir");
        let elsewhere = tempfile::tempdir().expect("a temp dir");
        let link = parent.path().join(format!("{PREFIX}link"));
        std::os::unix::fs::symlink(elsewhere.path(), &link).expect("links");
        File::create(elsewhere.path().join(LOCK_FILE)).expect("creates");
        assert!(sweep_in(parent.path(), PREFIX).is_empty());
        assert!(elsewhere.path().join(LOCK_FILE).exists());
    }
}
