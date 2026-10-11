//! `metor pack dev` builds a pack and writes its editable Python module.
//!
//! The module lands in `.metor/` next to the pack, so a target in the same
//! workspace can import it without installing a wheel.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::cargo::{CargoError, PackConfig, cargo_build};
use super::module::render;
use crate::dl::store::{copy_atomic, write_atomic};
use crate::dl::{DlPack, Pack, PackError, PackRef, cdylib_name, triple};
use crate::pack::ABI_VERSION;

static BUILD: Mutex<()> = Mutex::new(());

/// An error building a pack or writing its editable module.
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
    #[error("rendering the module: {0}")]
    Template(#[from] minijinja::Error),
    #[error(transparent)]
    Cargo(#[from] CargoError),
    #[error(transparent)]
    Pack(#[from] PackError),
    #[error("{0} is already loaded; restart the process before rebuilding this pack")]
    AlreadyLoaded(PathBuf),
}

impl PackDevError {
    /// Returns a closure that turns an IO error into a `Write` error for
    /// `path`.
    fn write(path: &Path) -> impl FnOnce(std::io::Error) -> Self + '_ {
        |source| Self::Write {
            path: path.to_path_buf(),
            source,
        }
    }
}

/// Builds the pack at `root` and writes its module to
/// `<root>/.metor/<module>/`.
///
/// Only one call runs at a time. A library this process has already loaded
/// cannot be replaced, so rebuilding it requires a new process. The written
/// paths are printed to stderr, next to cargo's output.
pub fn pack_dev(root: &Path) -> Result<(), PackDevError> {
    let _build = BUILD.lock().unwrap_or_else(|e| e.into_inner());
    let config = PackConfig::read(root)?;
    let module = root.join(".metor").join(&config.module);
    let reference = PackRef::dylib(config.id.clone(), config.lib.clone(), module.join("_libs"));
    let libs = reference.libs.join(triple());
    let dylib = libs.join(cdylib_name(&config.lib));
    if crate::dl::is_loaded(&dylib) {
        return Err(PackDevError::AlreadyLoaded(dylib));
    }
    let built = cargo_build(&config.krate, root)?;
    std::fs::create_dir_all(&libs).map_err(PackDevError::write(&libs))?;
    copy_atomic(&built, &dylib).map_err(PackDevError::write(&dylib))?;
    // SAFETY: this library was just built from the pack crate, against this
    // pack ABI.
    let pack = Pack::Dl(unsafe { DlPack::open(&dylib) }?);
    let text = render(&reference, ABI_VERSION, pack.descriptor())?;
    let init = module.join("__init__.py");
    write_atomic(&init, text.as_bytes()).map_err(PackDevError::write(&init))?;
    let typed = module.join("py.typed");
    write_atomic(&typed, b"").map_err(PackDevError::write(&typed))?;
    for path in [&init, &typed, &dylib] {
        eprintln!("{}", path.display());
    }
    Ok(())
}
