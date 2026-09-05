//! Save/load tile-layout presets.
//!
//! Three sources:
//! - **File on disk** — caller provides the path (typically picked via the OS
//!   save dialog). Round-trip is plain `std::fs`.
//! - **Built-in directory** — `dirs::config_dir()/metor/panel/presets/`. Each
//!   preset is one `<name>.json` file. The directory is created on demand.
//! - **The connected target** — a target declaring `Presets(...)` broadcasts
//!   a [`PresetDefs`] snapshot the link replays on every connection;
//!   [`TargetPresetStore`] tails it and offers the layouts in the palette.
//!   The first arrival also applies a shipped preset automatically when the
//!   session is a blank slate (no local per-target layout, untouched tiles) —
//!   the same silent-when-empty rule the connection picker uses for locally
//!   saved layouts, which always take precedence over shipped ones.
//!
//! Loading goes through [`load_into_tiles`], which fetches the panel-item
//! [`tiles::ItemRegistry`] from gpui's globals and asks the
//! [`TileGroup`] entity to swap its contents in place.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::{fs, io};

use gpui::{App, Context, Entity, Global, Task, prelude::*};
use metor_db::DB;
use metor_proto::types::Msg;
use metor_proto_wkt::{Preset, PresetDefs};

use crate::msg_ingest::{IngestSource, ingest_all};
use crate::tiles::TileGroup;

/// Resolve the per-user preset directory path. Does **not** create it —
/// list/read paths shouldn't materialize the directory just by being walked.
fn presets_dir_path() -> io::Result<PathBuf> {
    let base = dirs::config_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no config dir"))?;
    Ok(base.join("metor").join("panel").join("presets"))
}

/// Resolve and create the per-user preset directory. Used by the write path.
pub fn presets_dir() -> io::Result<PathBuf> {
    let dir = presets_dir_path()?;
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Every `*.json` file in the preset directory, paired with its bare name
/// (sans extension) for display. The extension match is case-insensitive
/// so files written elsewhere as `.JSON` still appear.
pub fn list_presets() -> Vec<(String, PathBuf)> {
    let Ok(dir) = presets_dir_path() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    // `read_dir` returns NotFound for a missing directory, so users who
    // never saved a preset see an empty list without us touching disk.
    let Ok(entries) = fs::read_dir(&dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_json = path
            .extension()
            .and_then(OsStr::to_str)
            .is_some_and(|ext| ext.eq_ignore_ascii_case("json"));
        if !is_json {
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
    let already_has_ext = safe.len() >= 5 && safe[safe.len() - 5..].eq_ignore_ascii_case(".json");
    let file_name = if already_has_ext {
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
/// thread it. Failures are logged via `tracing`; a toast UX is out of
/// scope.
pub fn load_into_tiles(json: &str, tiles: &Entity<TileGroup>, cx: &mut App) {
    tiles.update(cx, |tg, cx| {
        if let Err(e) = tg.replace_from_json(json, cx) {
            tracing::error!(%e, "failed to load preset");
        }
    });
}

/// The latest [`PresetDefs`] snapshot off the link, one preset list at a
/// time — a re-publish replaces the set, mirroring the wire semantics.
pub struct TargetPresetStore {
    presets: Vec<Preset>,
    _task: Task<()>,
}

pub struct GlobalTargetPresets(pub Entity<TargetPresetStore>);

impl Global for GlobalTargetPresets {}

/// The shared store, or `None` if it was never initialized (e.g. in tests).
pub fn try_target_presets(cx: &App) -> Option<Entity<TargetPresetStore>> {
    cx.try_global::<GlobalTargetPresets>().map(|g| g.0.clone())
}

impl TargetPresetStore {
    pub fn init(db: Arc<DB>, cx: &mut App) {
        let entity = cx.new(|cx| TargetPresetStore::new(db, cx));
        cx.observe(&entity, apply_shipped_preset).detach();
        cx.set_global(GlobalTargetPresets(entity));
    }

    fn new(db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let task = cx.spawn(async move |this, cx| {
            let sources = vec![IngestSource::new(
                PresetDefs::ID,
                |store: &mut Self, _ts, defs: PresetDefs| store.presets = defs.presets,
            )];
            ingest_all(db, sources, this, cx).await
        });
        Self {
            presets: Vec::new(),
            _task: task,
        }
    }

    pub fn presets(&self) -> &[Preset] {
        &self.presets
    }
}

/// Apply a target-shipped preset to a blank slate: an active connection whose
/// target has no locally saved layout (the user's own arrangement always
/// wins) and an untouched tile tree. Runs on every store notify; once tiles
/// hold anything — including a just-applied preset — it is a no-op, and the
/// layout autosave then owns persistence like any other layout.
fn apply_shipped_preset(store: Entity<TargetPresetStore>, cx: &mut App) {
    let Some(connections) = crate::connections::try_global(cx) else {
        return;
    };
    {
        let conns = connections.read(cx);
        let active = conns.active();
        if active.is_empty() {
            return;
        }
        if active
            .iter()
            .any(|c| crate::connections::persist::load_layout(&c.target.id).is_some())
        {
            return;
        }
    }
    if crate::workspace::any_window_has_items(cx) {
        return;
    }
    let Some(tiles) = crate::workspace::active_tiles(cx) else {
        return;
    };
    let Some(layout) = store.read(cx).presets.first().map(|p| p.layout.clone()) else {
        return;
    };
    load_into_tiles(&layout, &tiles, cx);
}
