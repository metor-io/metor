//! The content-addressed store slot candidates load from: every library at
//! `<store>/<blake3 of its bytes>/<file name>`.

use std::fs::File;
use std::io;
use std::path::Path;

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
