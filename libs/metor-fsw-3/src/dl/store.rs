//! The content-addressed store slot candidates load from: every library at
//! `<store>/<blake3 of its bytes>/<file name>`.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

/// Where a library landed in the store, and the hash that named it.
pub struct Stored {
    pub path: PathBuf,
    pub hash: blake3::Hash,
}

/// The store beside a target file's directory.
pub fn dir(target_dir: &Path) -> PathBuf {
    target_dir.join(".metor").join("store")
}

/// Copies `artifact` into `store` under its hash, unless those bytes are
/// already there. A stored file is never rewritten, so a library another
/// process has mapped keeps its inode.
pub fn put(store: &Path, artifact: &Path) -> io::Result<Stored> {
    let hash = hash(artifact)?;
    let name = artifact
        .file_name()
        .ok_or_else(|| io::Error::other(format!("`{}` names no file", artifact.display())))?;
    let dir = store.join(hash.to_hex().as_str());
    let path = dir.join(name);
    if !path.exists() {
        std::fs::create_dir_all(&dir)?;
        crate::cli::cargo::copy_atomic(artifact, &path)?;
    }
    Ok(Stored { path, hash })
}

/// The blake3 of the file at `path`.
pub fn hash(path: &Path) -> io::Result<blake3::Hash> {
    Hashed::of(path).map(|hashed| hashed.hash)
}

/// A file hashed through one open handle, so a later check names those bytes.
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

    /// Whether `path` still names the file that was hashed.
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
}
