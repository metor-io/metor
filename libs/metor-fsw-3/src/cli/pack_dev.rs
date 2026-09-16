//! `metor pack dev`: build a pack and lay out the editable module beside it.

use std::path::{Path, PathBuf};

use super::build::write_atomic as write_bytes;
use super::build::{BuildError, PackConfig, cargo_build, cdylib_name, copy_atomic, triple};
use super::config::PackRef;
use super::module::render;
use crate::dl::{Pack, PackError};
use crate::pack::ABI_VERSION;

/// A `PackDevError` is why a pack's editable module was not laid out.
#[derive(Debug, thiserror::Error)]
pub enum PackDevError {
    #[error("reading {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("writing {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path} is not valid TOML: {source}")]
    Toml {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("`{0}` is missing")]
    MissingField(&'static str),
    #[error("the params schema of `{system}` did not decode: {source}")]
    Schema {
        system: String,
        source: serde_json::Error,
    },
    #[error("system `{system}` has an input port and a param both named `{name}`")]
    NameClash { system: String, name: String },
    #[error(transparent)]
    Build(#[from] BuildError),
    #[error(transparent)]
    Pack(#[from] PackError),
}

/// Builds the pack at `root` and writes `<root>/.metor/<module>/`.
///
/// The laid-out paths are printed on stderr, cargo's own stream.
pub fn pack_dev(root: &Path) -> Result<(), PackDevError> {
    let config = PackConfig::read(root)?;
    let built = cargo_build(&config.krate, root, false)?;
    // SAFETY: cargo just built the library from this workspace against this ABI.
    let pack = unsafe { Pack::open(&built) }?;

    let module = root.join(".metor").join(&config.module);
    let reference = PackRef {
        id: config.id.clone(),
        lib: config.lib.clone(),
        libs: module.join("_libs"),
    };
    let text = render(&reference, ABI_VERSION, pack.def())?;

    let libs = reference.libs.join(triple());
    create_dir_all(&libs)?;
    let init = module.join("__init__.py");
    write_atomic(&init, text.as_bytes())?;
    let typed = module.join("py.typed");
    write_atomic(&typed, b"")?;
    let dylib = libs.join(cdylib_name(&config.lib));
    copy_atomic(&built, &dylib).map_err(|source| PackDevError::Write {
        path: dylib.clone(),
        source,
    })?;

    for path in [&init, &typed, &dylib] {
        eprintln!("{}", path.display());
    }
    Ok(())
}

fn create_dir_all(path: &Path) -> Result<(), PackDevError> {
    std::fs::create_dir_all(path).map_err(|source| PackDevError::Write {
        path: path.to_path_buf(),
        source,
    })
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), PackDevError> {
    write_bytes(path, bytes).map_err(|source| PackDevError::Write {
        path: path.to_path_buf(),
        source,
    })
}
