//! Resolves `use frames from "pack"` against built packs on disk.
//!
//! A pack's generated module lives at `<dir>/<pack>/`, with its library at
//! `_libs/<triple>/`. Its frames are read from the ports its descriptor
//! reports, and each must rebuild to the layout the pack announces.

use std::path::{Path, PathBuf};

use mc_rt::desc::FrameDesc;
use mc_rt::import::frame_of_port;
use metor_fsw_3::dl::{DlPack, triple};

use crate::check::Imports;

/// Packs found under a list of module directories, such as `.metor`.
pub struct PackDirs(pub Vec<PathBuf>);

impl PackDirs {
    /// The directories a file at `source` looks in: its own `.metor`, then
    /// each dev pack's `.metor` its `pyproject.toml` lists.
    pub fn around(source: &Path, extra: Vec<PathBuf>) -> PackDirs {
        let dir = source.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
        let mut dirs = extra;
        dirs.push(dir.join(".metor"));
        let packs = metor_fsw_3::cli::run::dev_packs(dir);
        dirs.extend(packs.iter().map(|root| root.join(".metor")));
        PackDirs(dirs)
    }

    /// The dev pack roots near `source`, which `metor pack dev` builds.
    pub fn dev_packs(source: &Path) -> Vec<PathBuf> {
        let dir = source.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
        metor_fsw_3::cli::run::dev_packs(dir)
    }

    /// Finds the library of `pack`.
    fn library(&self, pack: &str) -> Option<PathBuf> {
        self.0.iter().find_map(|dir| {
            let libs = dir.join(pack).join("_libs").join(triple());
            std::fs::read_dir(libs)
                .ok()?
                .filter_map(|entry| entry.ok().map(|e| e.path()))
                .find(|path| is_library(path))
        })
    }
}

fn is_library(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext == "dylib" || ext == "so" || ext == "dll")
}

impl Imports for PackDirs {
    fn frames(&self, pack: &str) -> Result<Vec<FrameDesc>, String> {
        let path = self.library(pack).ok_or_else(|| {
            let dirs: Vec<String> = self.0.iter().map(|d| d.display().to_string()).collect();
            format!("no built pack `{pack}` under {}; build it with `metor pack dev`", dirs.join(", "))
        })?;
        // SAFETY: the library is a pack `metor pack dev` built; `open` checks
        // its ABI version before calling anything else.
        let pack = unsafe { DlPack::open(&path) }.map_err(|e| e.to_string())?;
        Ok(frames_of(pack.descriptor()))
    }
}

/// Every frame a pack's ports carry, once each, that metor-code can read.
pub fn frames_of(def: &metor_fsw_3::PackDef) -> Vec<FrameDesc> {
    let mut frames: Vec<FrameDesc> = Vec::new();
    let ports = def
        .systems
        .iter()
        .flat_map(|s| s.def.inputs.iter().chain(&s.def.outputs));
    for port in ports {
        if frames.iter().any(|f| *f.name == *port.record) {
            continue;
        }
        if let Ok(frame) = frame_of_port(port, &port.name) {
            frames.push(frame);
        }
    }
    frames
}
