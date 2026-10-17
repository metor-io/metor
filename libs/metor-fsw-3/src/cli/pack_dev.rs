//! `metor pack dev` builds a pack and writes its editable Python module.
//!
//! The module lands in `.metor/` next to the pack, so a target in the same
//! workspace can import it without installing a wheel. A pack with
//! `wasm = true` is also built as a wasm module, which is checked against the
//! native library.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::cargo::{CargoError, PackConfig, cargo_build, wasm_build};
use super::module::render;
use crate::coordinator::SystemKind;
use crate::dl::store::{copy_atomic, write_atomic};
use crate::dl::{DlPack, Pack, PackError, PackKind, PackRef, cdylib_name, triple};
use crate::pack::ABI_VERSION;
use crate::pack::def::PackDef;

/// Serializes `pack_dev` calls, which would race on the same `.metor` output
/// and on the libraries this process has loaded.
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
    #[error("reading {path}: {source}")]
    Toml {
        path: PathBuf,
        source: toml::de::Error,
    },
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
    #[error("system `{system}` is async, which a module cannot run; drop `wasm = true`")]
    AsyncInWasm { system: String },
    #[error("the module describes other systems than the library")]
    DescriptorMismatch,
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
    let mut reference = PackRef::dylib(config.id.clone(), config.lib.clone(), module.join("_libs"));
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
    let mut staged = vec![dylib];
    if config.wasm {
        refuse_async(pack.descriptor())?;
        reference.kind = PackKind::Wasm(Default::default());
        staged.push(stage_module(&config, root, &reference, pack.descriptor())?);
    }
    let text = render(&reference, ABI_VERSION, pack.descriptor())?;
    let init = module.join("__init__.py");
    write_atomic(&init, text.as_bytes()).map_err(PackDevError::write(&init))?;
    let typed = module.join("py.typed");
    write_atomic(&typed, b"").map_err(PackDevError::write(&typed))?;
    for path in [&init, &typed].into_iter().chain(&staged) {
        eprintln!("{}", path.display());
    }
    Ok(())
}

/// Returns an error if the pack has an async system, since a wasm module
/// cannot run one.
fn refuse_async(def: &PackDef) -> Result<(), PackDevError> {
    let found = def.systems.iter().find(|s| s.kind == SystemKind::Async);
    match found {
        Some(system) => Err(PackDevError::AsyncInWasm {
            system: system.ty.clone(),
        }),
        None => Ok(()),
    }
}

/// Builds the pack as a wasm module and copies it to where `reference`
/// expects it.
///
/// Returns an error if the module's descriptor differs from the native
/// library's.
fn stage_module(
    config: &PackConfig,
    root: &Path,
    reference: &PackRef,
    library: &PackDef,
) -> Result<PathBuf, PackDevError> {
    let built = wasm_build(&config.krate, root)?;
    let staged = reference.artifact();
    if let Some(dir) = staged.parent() {
        std::fs::create_dir_all(dir).map_err(PackDevError::write(dir))?;
    }
    copy_atomic(&built, &staged).map_err(PackDevError::write(&staged))?;
    let pack = reference.open()?;
    if pack.descriptor() != library {
        return Err(PackDevError::DescriptorMismatch);
    }
    Ok(staged)
}
