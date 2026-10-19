//! A content-addressed store for pack libraries.
//!
//! Slots load their candidates from the store.
//! Each library is stored at `<store>/<blake3 of its bytes>/<file name>`, so a
//! path in the store always refers to the same bytes.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

/// A library that [`put`] copied into the store.
pub struct StoredLibrary {
    pub path: PathBuf,
    pub hash: blake3::Hash,
}

/// Returns the store directory for the target in `target_dir`.
pub fn dir(target_dir: &Path) -> PathBuf {
    target_dir.join(".metor").join("store")
}

/// Copies `artifact` into `store` under its hash, unless the same bytes are
/// already stored.
///
/// A stored file is never rewritten. That way, a library another process has
/// mapped keeps its inode.
pub fn put(store: &Path, artifact: &Path) -> io::Result<StoredLibrary> {
    let hash = hash(artifact)?;
    let name = artifact
        .file_name()
        .ok_or_else(|| io::Error::other(format!("`{}` names no file", artifact.display())))?;
    let dir = store.join(hash.to_hex().as_str());
    let path = dir.join(name);
    if !path.exists() {
        std::fs::create_dir_all(&dir)?;
        copy_atomic(artifact, &path)?;
    }
    Ok(StoredLibrary { path, hash })
}

/// Returns the blake3 hash of the file at `path`.
pub fn hash(path: &Path) -> io::Result<blake3::Hash> {
    Hashed::of(path).map(|hashed| hashed.hash)
}

/// A file hash that can later be checked against a path.
///
/// [`Hashed::still_at`] tells you whether a path still refers to the file that
/// was hashed, so the bytes you load are the bytes you checked.
pub struct Hashed {
    pub hash: blake3::Hash,
    file: File,
}

impl Hashed {
    pub fn of(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let mut hasher = blake3::Hasher::new();
        hasher.update_reader(&file)?;
        Ok(Self {
            hash: hasher.finalize(),
            file,
        })
    }

    /// Returns whether `path` still refers to the file that was hashed.
    ///
    /// Always returns `true` on non-unix hosts.
    pub fn still_at(&self, path: &Path) -> io::Result<bool> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let (hashed, now) = (self.file.metadata()?, std::fs::metadata(path)?);
            Ok((hashed.dev(), hashed.ino()) == (now.dev(), now.ino()))
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            Ok(true)
        }
    }
}

/// Copies `src` to a temp file next to `dst`, then renames it over `dst`.
///
/// macOS caches a library's code signature by inode. If a dylib is overwritten
/// in place, the next process that `dlopen`s it is killed.
pub fn copy_atomic(src: &Path, dst: &Path) -> Result<(), std::io::Error> {
    let tmp = temp_beside(dst);
    std::fs::copy(src, &tmp)?;
    std::fs::rename(&tmp, dst)
}

/// Writes `bytes` to `dst` through a temp file, like [`copy_atomic`].
pub fn write_atomic(dst: &Path, bytes: &[u8]) -> Result<(), std::io::Error> {
    let tmp = temp_beside(dst);
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, dst)
}

/// Returns a temp path next to `dst`, unique to this process.
fn temp_beside(dst: &Path) -> PathBuf {
    let mut name = dst.as_os_str().to_owned();
    name.push(format!(".tmp{}", std::process::id()));
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hashed_file_identity() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::write(&a, b"one").expect("writes");
        std::fs::write(&b, b"one").expect("writes");
        let hashed = Hashed::of(&a).expect("reads");
        assert!(hashed.still_at(&a).expect("stats"));
        std::fs::rename(&b, &a).expect("replaces");
        assert!(!hashed.still_at(&a).expect("stats"));
        assert!(hashed.still_at(&dir.path().join("gone")).is_err());
    }

    #[test]
    fn test_put_same_bytes_once() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = dir.path().join("store");
        let artifact = dir.path().join("libup.so");
        std::fs::write(&artifact, b"one").expect("writes");
        let first = put(&store, &artifact).expect("stores");
        assert_eq!(first.hash, blake3::hash(b"one"));
        assert!(
            first
                .path
                .starts_with(store.join(first.hash.to_hex().as_str()))
        );
        assert_eq!(std::fs::read(&first.path).expect("reads"), b"one");
        let modified = std::fs::metadata(&first.path).and_then(|m| m.modified());

        let again = put(&store, &artifact).expect("stores");
        assert_eq!(again.path, first.path);
        let unchanged = std::fs::metadata(&again.path).and_then(|m| m.modified());
        assert_eq!(unchanged.ok(), modified.ok());
    }

    #[test]
    fn test_put_new_bytes_new_path() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = dir.path().join("store");
        let artifact = dir.path().join("libup.so");
        std::fs::write(&artifact, b"one").expect("writes");
        let first = put(&store, &artifact).expect("stores");
        std::fs::write(&artifact, b"two").expect("writes");
        let second = put(&store, &artifact).expect("stores");
        assert_ne!(first.path, second.path);
        assert_eq!(std::fs::read(&first.path).expect("reads"), b"one");
        assert!(second.path.starts_with(&store));
    }

    #[test]
    fn test_copy_atomic_replaces_file() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let (src, dst) = (dir.path().join("src"), dir.path().join("dst"));
        std::fs::write(&src, b"new").expect("writes");
        std::fs::write(&dst, b"old").expect("writes");
        copy_atomic(&src, &dst).expect("copies");
        assert_eq!(std::fs::read(&dst).expect("reads"), b"new");

        let names: Vec<_> = std::fs::read_dir(dir.path())
            .expect("lists")
            .map(|entry| entry.expect("an entry").file_name())
            .collect();
        assert_eq!(names.len(), 2, "no temp file is left: {names:?}");
    }
}
