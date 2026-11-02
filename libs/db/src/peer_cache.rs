//! Per-peer mirror databases kept between sessions.
//!
//! A client that mirrors one remote peer keeps its mirror at
//! `<root>/<instance id>/db` and reopens it the next time it connects to the
//! same peer, so history it fetched or watched live, LoD levels, metadata
//! and computed outputs are all still there. The id only picks the
//! directory; whoever reopens it still reconciles sealed spans against the
//! peer's manifests, so a stale or copied cache can't serve wrong data.
//!
//! Each peer directory has one writer at a time: [`PeerCache::open`] holds an
//! exclusive advisory lock on a file inside it for the life of the
//! [`CachedDb`], and a second client connected to the same peer gets `None`
//! and falls back to a temporary database. [`PeerCache::prune`] removes only
//! directories it can lock itself, so it never deletes a cache in use.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use metor_proto_wkt::InstanceId;
use tracing::{info, warn};

use crate::{DB, Error};

/// The lock file a [`CachedDb`] holds for its lifetime.
const LOCK_FILE: &str = ".lock";

/// Rewritten on every [`PeerCache::open`]; its mtime is when the peer was
/// last used, which [`PeerCache::prune`] orders and expires by.
const LAST_USED_FILE: &str = "last_used";

/// The database inside a peer directory. A subdirectory, so the lock and
/// marker files stay out of the database's own listing.
const DB_DIR: &str = "db";

/// How many times [`PeerCache::open`] retries when a prune removes the
/// directory between opening its lock file and locking it.
const OPEN_ATTEMPTS: usize = 4;

/// The directory of cached peer databases, one subdirectory per peer
/// [`InstanceId`].
#[derive(Debug, Clone)]
pub struct PeerCache {
    root: PathBuf,
}

impl PeerCache {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// `<user cache dir>/metor/peers`, or `None` on a platform without a
    /// cache directory for this user.
    pub fn default_root() -> Option<PathBuf> {
        directories::BaseDirs::new().map(|dirs| dirs.cache_dir().join("metor").join("peers"))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Claims the cache directory for `id`, creating it if this peer is new.
    ///
    /// `Ok(None)` when another process (or another [`CachedDb`] in this
    /// one) holds it; the caller should mirror into a temporary database
    /// instead.
    pub fn open(&self, id: InstanceId) -> io::Result<Option<CachedDb>> {
        let path = self.root.join(id.to_string());
        for _ in 0..OPEN_ATTEMPTS {
            fs::create_dir_all(&path)?;
            let lock_path = path.join(LOCK_FILE);
            let lock = OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&lock_path)?;
            match lock.try_lock() {
                Ok(()) => {}
                Err(fs::TryLockError::WouldBlock) => return Ok(None),
                Err(fs::TryLockError::Error(error)) => return Err(error),
            }
            // A prune that held the lock may have removed the directory
            // after we opened the file; our lock is then on an unlinked
            // file nobody else can see, so start over.
            if !same_file(&lock, &lock_path) {
                continue;
            }
            fs::write(path.join(LAST_USED_FILE), b"")?;
            let fresh = !path.join(DB_DIR).exists();
            return Ok(Some(CachedDb {
                path,
                fresh,
                _lock: lock,
            }));
        }
        Err(io::Error::other(format!(
            "{} kept disappearing while it was being opened",
            path.display()
        )))
    }

    /// Removes peer directories nobody holds: first every one unused for
    /// `max_idle`, then the least recently used until everything under the
    /// root fits in `max_bytes`. Returns the removed directories.
    ///
    /// Directories in use, and `skip` (the peer about to be opened), are
    /// never removed but count toward `max_bytes`. Sizes are what the files
    /// occupy on disk: node files are sparse, so their length overstates
    /// them by orders of magnitude.
    pub fn prune(
        &self,
        max_bytes: u64,
        max_idle: Duration,
        skip: Option<InstanceId>,
    ) -> Vec<PathBuf> {
        let Ok(entries) = fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let now = SystemTime::now();
        let mut removed = Vec::new();
        let mut total = 0u64;
        let mut candidates = Vec::new();
        for entry in entries.filter_map(Result::ok) {
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let Some(id) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<InstanceId>().ok())
            else {
                continue;
            };
            let path = entry.path();
            let size = disk_usage(&path);
            let lock = (Some(id) != skip).then(|| try_lock(&path)).flatten();
            let Some(lock) = lock else {
                total += size;
                continue;
            };
            let last_used = last_used(&path);
            let idle = now.duration_since(last_used).unwrap_or_default();
            if idle >= max_idle && remove(&path, &lock) {
                removed.push(path);
                continue;
            }
            total += size;
            candidates.push((last_used, path, size, lock));
        }
        candidates.sort_by_key(|(last_used, ..)| *last_used);
        for (_, path, size, lock) in candidates {
            if total <= max_bytes {
                break;
            }
            if remove(&path, &lock) {
                total -= size;
                removed.push(path);
            }
        }
        removed
    }
}

/// A claimed peer directory. Its lock is held until this is dropped, so
/// keep it alive as long as the database opened from it.
#[derive(Debug)]
pub struct CachedDb {
    path: PathBuf,
    /// No database existed here when the directory was claimed, or the one
    /// that did couldn't be opened and was replaced: the peer's history
    /// starts empty.
    pub fresh: bool,
    _lock: File,
}

impl CachedDb {
    /// The peer's directory.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Where the database lives, for [`DB::open`] or [`DB::create`].
    pub fn db_path(&self) -> PathBuf {
        self.path.join(DB_DIR)
    }

    /// Opens the cached database, or creates it when there is none. A
    /// database that fails to open is logged, deleted and created anew: a
    /// cache can always be fetched again, and must never stop a session
    /// from starting.
    pub fn open_db(&mut self) -> Result<DB, Error> {
        let path = self.db_path();
        if !self.fresh {
            match DB::open(path.clone()) {
                Ok(db) => return Ok(db),
                Err(error) => {
                    warn!(path = %path.display(), ?error, "replacing a cached database that failed to open");
                    self.fresh = true;
                    if let Err(error) = fs::remove_dir_all(&path)
                        && error.kind() != io::ErrorKind::NotFound
                    {
                        return Err(error.into());
                    }
                }
            }
        }
        DB::create(path)
    }
}

/// The lock on `dir`'s lock file, unless someone else holds it. A directory
/// without one (a claim cut short before its lock file existed) gets one.
fn try_lock(dir: &Path) -> Option<File> {
    let lock_path = dir.join(LOCK_FILE);
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .ok()?;
    lock.try_lock().ok()?;
    same_file(&lock, &lock_path).then_some(lock)
}

/// Removes `dir` while the caller holds its `_lock`, lock file last: a removal cut
/// short still leaves a lockable directory for the next prune, and an
/// [`PeerCache::open`] racing it either waits on the lock or sees the lock
/// file gone and starts over.
fn remove(dir: &Path, _lock: &File) -> bool {
    let removed = (|| {
        for entry in fs::read_dir(dir)? {
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
        fs::remove_dir_all(dir)
    })();
    match removed {
        Ok(()) => {
            info!(path = %dir.display(), "removed cached peer database");
            true
        }
        Err(error) => {
            warn!(path = %dir.display(), %error, "failed to remove cached peer database");
            false
        }
    }
}

fn last_used(dir: &Path) -> SystemTime {
    fs::metadata(dir.join(LAST_USED_FILE))
        .or_else(|_| fs::metadata(dir))
        .and_then(|meta| meta.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// Whether `file` is still the file at `path`, rather than one unlinked
/// since it was opened.
#[cfg(unix)]
fn same_file(file: &File, path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (file.metadata(), fs::metadata(path)) {
        (Ok(open), Ok(named)) => open.dev() == named.dev() && open.ino() == named.ino(),
        _ => false,
    }
}

/// Windows refuses to delete an open file, so the file open here can't have
/// been unlinked.
#[cfg(not(unix))]
fn same_file(_file: &File, path: &Path) -> bool {
    path.exists()
}

/// Bytes `path` occupies on disk, recursively, not following symlinks.
pub fn disk_usage(path: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return 0;
    };
    if !meta.is_dir() {
        return allocated(&meta);
    }
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| disk_usage(&entry.path()))
        .sum()
}

#[cfg(unix)]
fn allocated(meta: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.blocks() * 512
}

#[cfg(not(unix))]
fn allocated(meta: &fs::Metadata) -> u64 {
    meta.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ComponentSchema;
    use metor_proto::types::{ComponentId, PrimType, Timestamp};

    fn id(n: u8) -> InstanceId {
        InstanceId([n; 16])
    }

    /// Back-dates `dir`'s last use by `age`.
    fn age(dir: &Path, age: Duration) {
        let file = File::options()
            .write(true)
            .open(dir.join(LAST_USED_FILE))
            .unwrap();
        file.set_modified(SystemTime::now() - age).unwrap();
    }

    /// Fills `dir` with `bytes` of real (non-sparse) data.
    fn fill(dir: &Path, bytes: usize) {
        fs::write(dir.join("ballast"), vec![1u8; bytes]).unwrap();
    }

    fn claim(cache: &PeerCache, n: u8) -> PathBuf {
        cache.open(id(n)).unwrap().unwrap().path().to_owned()
    }

    #[test]
    fn reopen_reuses_the_db_and_its_data() {
        let root = tempfile::tempdir().unwrap();
        let cache = PeerCache::new(root.path().to_owned());
        let component = ComponentId::new("a.b");
        let db_id = {
            let mut cached = cache.open(id(1)).unwrap().unwrap();
            assert!(cached.fresh);
            assert_eq!(cached.path(), root.path().join(id(1).to_string()));
            let db = cached.open_db().unwrap();
            db.with_state_mut(|state| {
                state.insert_component(
                    component,
                    ComponentSchema::new(PrimType::F64, smallvec::SmallVec::<[usize; 4]>::new()),
                    &db.path,
                )
            })
            .unwrap();
            let series =
                db.with_state(|state| state.get_component(component).unwrap().time_series.clone());
            let mut writer = series.writer().unwrap();
            writer.push_buf(Timestamp(7), &7f64.to_le_bytes()).unwrap();
            drop(writer);
            db.flush().unwrap();
            db.instance_id()
        };
        let mut cached = cache.open(id(1)).unwrap().unwrap();
        assert!(!cached.fresh);
        let db = cached.open_db().unwrap();
        assert_eq!(db.instance_id(), db_id);
        let latest = db.with_state(|state| {
            state
                .get_component(component)
                .unwrap()
                .time_series
                .latest()
                .map(|s| s.timestamp())
        });
        assert_eq!(latest, Some(Timestamp(7)));
    }

    #[test]
    fn a_held_dir_cannot_be_opened_twice() {
        let root = tempfile::tempdir().unwrap();
        let cache = PeerCache::new(root.path().to_owned());
        let held = cache.open(id(1)).unwrap().unwrap();
        assert!(cache.open(id(1)).unwrap().is_none());
        assert!(cache.open(id(2)).unwrap().is_some(), "other peers are free");
        drop(held);
        assert!(cache.open(id(1)).unwrap().is_some());
    }

    #[test]
    fn prune_removes_idle_peers() {
        let root = tempfile::tempdir().unwrap();
        let cache = PeerCache::new(root.path().to_owned());
        let old = claim(&cache, 1);
        let recent = claim(&cache, 2);
        age(&old, Duration::from_secs(3600));
        age(&recent, Duration::from_secs(60));
        let removed = cache.prune(u64::MAX, Duration::from_secs(600), None);
        assert_eq!(removed, vec![old.clone()]);
        assert!(!old.exists() && recent.is_dir());
    }

    #[test]
    fn prune_removes_least_recently_used_until_under_budget() {
        let root = tempfile::tempdir().unwrap();
        let cache = PeerCache::new(root.path().to_owned());
        let dirs: Vec<_> = (1..=3).map(|n| claim(&cache, n)).collect();
        for (dir, minutes) in dirs.iter().zip([30, 10, 20]) {
            fill(dir, 64 * 1024);
            age(dir, Duration::from_secs(minutes * 60));
        }
        let one = disk_usage(&dirs[0]);
        // Room for two of the three: the oldest goes.
        let removed = cache.prune(2 * one + one / 2, Duration::MAX, None);
        assert_eq!(removed, vec![dirs[0].clone()]);
        // Room for one: the next oldest goes too.
        let removed = cache.prune(one + one / 2, Duration::MAX, None);
        assert_eq!(removed, vec![dirs[2].clone()]);
        assert!(dirs[1].is_dir());
    }

    #[test]
    fn held_and_skipped_dirs_survive_prune() {
        let root = tempfile::tempdir().unwrap();
        let cache = PeerCache::new(root.path().to_owned());
        let held = cache.open(id(1)).unwrap().unwrap();
        let skipped = claim(&cache, 2);
        let free = claim(&cache, 3);
        for dir in [held.path(), &skipped, &free] {
            fill(dir, 64 * 1024);
            age(dir, Duration::from_secs(3600));
        }
        let removed = cache.prune(0, Duration::from_secs(60), Some(id(2)));
        assert_eq!(removed, vec![free]);
        assert!(held.path().is_dir() && skipped.is_dir());
        assert!(cache.open(id(1)).unwrap().is_none(), "still held");
    }

    #[test]
    fn prune_ignores_foreign_entries() {
        let root = tempfile::tempdir().unwrap();
        let cache = PeerCache::new(root.path().to_owned());
        let stranger = root.path().join("not-a-peer");
        fs::create_dir(&stranger).unwrap();
        fs::write(root.path().join("notes"), b"x").unwrap();
        assert!(cache.prune(0, Duration::ZERO, None).is_empty());
        assert!(stranger.is_dir());
    }

    #[test]
    fn a_corrupt_db_is_recreated() {
        let root = tempfile::tempdir().unwrap();
        let cache = PeerCache::new(root.path().to_owned());
        {
            let mut cached = cache.open(id(1)).unwrap().unwrap();
            cached.open_db().unwrap();
        }
        let mut cached = cache.open(id(1)).unwrap().unwrap();
        fs::write(cached.db_path().join("db_state"), b"garbage").unwrap();
        assert!(!cached.fresh);
        let db = cached.open_db().unwrap();
        assert!(cached.fresh, "the replacement starts empty");
        assert!(db.with_state(|state| state.components.is_empty()));
    }
}
