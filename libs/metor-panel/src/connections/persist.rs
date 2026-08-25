//! On-disk state for the connection system, under
//! `dirs::config_dir()/metor/panel/connections/`.
//!
//! Two artifacts with the same tolerant-read conventions as
//! [`config`](crate::config):
//! - `index.json` — facet-json; favorites and the recents list. Address-carrying
//!   recents stay connectable across restarts through the installed
//!   address resolver; discovery-only recents carry display fields alone,
//!   reappearing as connectable once their source re-produces the target.
//! - `layouts/<sanitized-id>.json` — one workspace document per target: a
//!   tile layout per open window plus its screen bounds
//!   ([`crate::workspace::WorkspaceLayout`]). Files written before
//!   multi-window hold a bare single-tree layout; the loader wraps them as
//!   one window. Named presets deliberately stay single-tree.

use std::path::PathBuf;
use std::{fs, io};

use super::TargetId;

pub const RECENTS_CAP: usize = 16;

#[derive(facet::Facet, Clone, Debug, Default)]
pub struct ConnectionsIndex {
    #[facet(default)]
    pub favorites: Vec<String>,
    /// Most-recent first, capped at [`RECENTS_CAP`].
    #[facet(default)]
    pub recents: Vec<RecentEntry>,
    /// Connection knobs the operator set, keyed by target. Kept apart from
    /// [`recents`](Self::recents) so an answer outlives eviction past
    /// [`RECENTS_CAP`].
    #[facet(default)]
    pub options: Vec<TargetOptions>,
}

#[derive(facet::Facet, Clone, Debug, Default)]
pub struct TargetOptions {
    pub id: String,
    #[facet(default)]
    pub values: Vec<SavedOption>,
}

/// One saved answer. The value is a string rather than a typed union: it is
/// re-typed against the target's live declaration on the way back in, so a
/// knob that changed shape reads as its new default instead of failing the
/// whole file's parse.
#[derive(facet::Facet, Clone, Debug, Default)]
pub struct SavedOption {
    pub key: String,
    pub value: String,
}

#[derive(facet::Facet, Clone, Debug, Default)]
pub struct RecentEntry {
    pub id: String,
    #[facet(default)]
    pub name: String,
    #[facet(default)]
    pub detail: String,
    /// The string the installed address resolver can rebuild the target
    /// from, so the entry stays connectable across restarts without its
    /// discoverer. Absent for discovery-only targets.
    #[facet(default)]
    pub address: Option<String>,
    #[facet(default)]
    pub last_connected_unix: i64,
}

fn connections_dir() -> io::Result<PathBuf> {
    let base = dirs::config_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no config dir"))?;
    Ok(base.join("metor").join("panel").join("connections"))
}

fn index_path() -> io::Result<PathBuf> {
    Ok(connections_dir()?.join("index.json"))
}

fn layout_path(id: &TargetId) -> io::Result<PathBuf> {
    Ok(connections_dir()?
        .join("layouts")
        .join(format!("{}.json", sanitize_id(id.as_str()))))
}

/// Filesystem-safe name for a target id: anything outside `[A-Za-z0-9._-]`
/// becomes `_`, and an FNV-1a hash of the raw id is appended so distinct
/// ids can never sanitize to the same file.
pub fn sanitize_id(id: &str) -> String {
    let safe: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{safe}-{:08x}", fnv1a(id))
}

fn fnv1a(s: &str) -> u32 {
    let mut hash: u32 = 0x811c9dc5;
    for byte in s.bytes() {
        hash ^= byte as u32;
        hash = hash.wrapping_mul(0x01000193);
    }
    hash
}

/// Load the index, falling back to empty on a missing or malformed file.
pub fn load_index() -> ConnectionsIndex {
    let Ok(path) = index_path() else {
        return ConnectionsIndex::default();
    };
    let json = match fs::read_to_string(&path) {
        Ok(json) => json,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return ConnectionsIndex::default(),
        Err(e) => {
            tracing::error!(path = %path.display(), %e, "read connections index failed");
            return ConnectionsIndex::default();
        }
    };
    match facet_json::from_str(&json) {
        Ok(index) => index,
        Err(e) => {
            tracing::error!(path = %path.display(), %e, "parse connections index failed");
            ConnectionsIndex::default()
        }
    }
}

pub fn save_index(index: &ConnectionsIndex) -> io::Result<()> {
    let path = index_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = facet_json::to_string(index)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    fs::write(&path, json)
}

/// The saved layout for `id`, if one exists.
pub fn load_layout(id: &TargetId) -> Option<String> {
    let path = layout_path(id).ok()?;
    fs::read_to_string(path).ok()
}

pub fn save_layout(id: &TargetId, json: &str) -> io::Result<()> {
    let path = layout_path(id)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, json)
}
