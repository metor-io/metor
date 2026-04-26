//! Save/load tile-layout presets.
//!
//! Two destinations:
//! - **File on disk** — caller provides the path (typically picked via the OS
//!   save dialog). Round-trip is plain `std::fs`.
//! - **Built-in directory** — `dirs::config_dir()/metor/panel/presets/`. Each
//!   preset is one `<name>.json` file. The directory is created on demand.
//!
//! Loading goes through [`load_into_tiles`], which fetches the panel-item
//! [`tiles::ItemRegistry`] from gpui's globals and asks the
//! [`TileGroup`] entity to swap its contents in place.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::{fs, io};

use gpui::{App, Entity};

use crate::tiles::TileGroup;

/// Resolve (and create if missing) the per-user preset directory.
pub fn presets_dir() -> io::Result<PathBuf> {
    let base = dirs::config_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no config dir"))?;
    let dir = base.join("metor").join("panel").join("presets");
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Every `*.json` file in the preset directory, paired with its bare name
/// (sans extension) for display.
pub fn list_presets() -> Vec<(String, PathBuf)> {
    let Ok(dir) = presets_dir() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(&dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension() != Some(OsStr::new("json")) {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        out.push((stem.to_string(), path));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Save `json` as `<name>.json` under the preset directory. Silent overwrite.
pub fn save_preset(name: &str, json: &str) -> io::Result<PathBuf> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "preset name is empty",
        ));
    }
    let safe: String = trimmed
        .chars()
        .map(|c| if matches!(c, '/' | '\\') { '_' } else { c })
        .collect();
    let file_name = if safe.ends_with(".json") {
        safe
    } else {
        format!("{safe}.json")
    };
    let path = presets_dir()?.join(file_name);
    fs::write(&path, json)?;
    Ok(path)
}

/// Read a preset file from disk.
pub fn load_preset(path: &Path) -> io::Result<String> {
    fs::read_to_string(path)
}

/// Replace the current tile layout with the one encoded in `json`.
///
/// Pulls the panel-item registry from globals so callers don't have to
/// thread it. Failures are logged via `eprintln!`; a toast UX is out of
/// scope.
pub fn load_into_tiles(json: &str, tiles: &Entity<TileGroup>, cx: &mut App) {
    tiles.update(cx, |tg, cx| {
        if let Err(e) = tg.replace_from_json(json, cx) {
            eprintln!("failed to load preset: {e}");
        }
    });
}
