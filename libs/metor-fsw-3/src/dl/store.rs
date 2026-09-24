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
    let mut hasher = blake3::Hasher::new();
    hasher.update_reader(File::open(path)?)?;
    Ok(hasher.finalize())
}

/// Whether `path` names a file inside `store`, after resolving links.
pub fn contains(store: &Path, path: &Path) -> bool {
    match (store.canonicalize(), path.canonicalize()) {
        (Ok(store), Ok(path)) => path.starts_with(store),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hash_names_the_bytes() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::write(&a, b"one").expect("writes");
        std::fs::write(&b, b"one").expect("writes");
        assert_eq!(hash(&a).expect("reads"), hash(&b).expect("reads"));
        std::fs::write(&b, b"two").expect("writes");
        assert_ne!(hash(&a).expect("reads"), hash(&b).expect("reads"));
        assert_eq!(hash(&a).expect("reads"), blake3::hash(b"one"));
    }

    #[test]
    fn test_hash_missing_file_errors() {
        let dir = tempfile::tempdir().expect("a temp dir");
        assert!(hash(&dir.path().join("missing")).is_err());
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
        assert!(contains(&store, &second.path));
    }

    #[test]
    fn test_put_missing_artifact_errors() {
        let dir = tempfile::tempdir().expect("a temp dir");
        assert!(put(&dir.path().join("store"), &dir.path().join("missing")).is_err());
    }

    #[test]
    fn test_contains_only_inside() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = dir.path().join("store");
        std::fs::create_dir_all(store.join("abc")).expect("creates");
        let inside = store.join("abc").join("lib.so");
        let outside = dir.path().join("lib.so");
        std::fs::write(&inside, b"x").expect("writes");
        std::fs::write(&outside, b"x").expect("writes");
        assert!(contains(&store, &inside));
        assert!(!contains(&store, &outside));
        assert!(!contains(
            &store,
            &store.join("abc").join("..").join("..").join("lib.so")
        ));
        assert!(!contains(&store, &store.join("missing")));
    }
}
