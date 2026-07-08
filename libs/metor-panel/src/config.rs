//! User-editable panel configuration, persisted as one JSON document.
//!
//! Lives at `dirs::config_dir()/metor/panel/config.json` — the same base the
//! [`presets`](crate::presets) module uses, one level up from the preset
//! directory. The struct is intentionally small but grows over time (theme,
//! window prefs, …); each field carries `#[facet(default)]` so an older file
//! still parses after new fields land.
//!
//! Reads are tolerant: a missing or malformed file yields [`PanelConfig::default`]
//! rather than an error, so a hand-edited typo can never take the app down.

use std::path::PathBuf;
use std::{fs, io};

/// How the UI font family is chosen.
///
/// [`Auto`](FontConfig::Auto) prefers a system **Berkeley Mono** and falls back
/// to the bundled font when it is absent — see
/// [`theme::resolve_font_family`](crate::theme::resolve_font_family).
#[derive(facet::Facet, Clone, Debug, PartialEq, Default)]
#[repr(u8)]
pub enum FontConfig {
    #[default]
    Auto,
    /// Pin a specific installed family by name; falls back to the bundled font
    /// if the system can't resolve it.
    Family(String),
}

/// Root of the on-disk panel config document.
///
/// `Default` is hand-written rather than derived: the derived impl would seed
/// `leader` with `String::default()` (empty), but the missing-file fallback in
/// [`load`] must produce a usable leader. The `#[facet(default = …)]` attribute
/// covers the parallel case where the field is simply absent from an older file.
#[derive(facet::Facet, Clone, Debug)]
pub struct PanelConfig {
    #[facet(default)]
    pub font: FontConfig,
    /// gpui keystroke that opens the transient chord menu (e.g. `"space"`,
    /// `"cmd-k"`).
    #[facet(default = default_leader())]
    pub leader: String,
}

impl Default for PanelConfig {
    fn default() -> Self {
        Self {
            font: FontConfig::default(),
            leader: default_leader(),
        }
    }
}

fn default_leader() -> String {
    "space".to_string()
}

/// Resolve the config file path. Does **not** create the parent directory —
/// the read path shouldn't materialize anything just by being walked.
fn config_path() -> io::Result<PathBuf> {
    let base = dirs::config_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no config dir"))?;
    Ok(base.join("metor").join("panel").join("config.json"))
}

/// Load the panel config, falling back to defaults on a missing or malformed
/// file. Parse failures are logged but never propagated.
pub fn load() -> PanelConfig {
    let Ok(path) = config_path() else {
        return PanelConfig::default();
    };
    let json = match fs::read_to_string(&path) {
        Ok(json) => json,
        // A first-run user has no file yet; that's the common case, not an error.
        Err(e) if e.kind() == io::ErrorKind::NotFound => return PanelConfig::default(),
        Err(e) => {
            eprintln!("read config {}: {e}", path.display());
            return PanelConfig::default();
        }
    };
    match facet_json::from_str(&json) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("parse config {}: {e}", path.display());
            PanelConfig::default()
        }
    }
}

/// Write the panel config, creating the parent directory on demand.
pub fn save(cfg: &PanelConfig) -> io::Result<()> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = facet_json::to_string(cfg)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    fs::write(&path, json)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(cfg: &PanelConfig) -> PanelConfig {
        let json = facet_json::to_string(cfg).expect("serialize");
        facet_json::from_str(&json).expect("deserialize")
    }

    #[test]
    fn font_config_round_trips() {
        assert_eq!(round_trip(&PanelConfig::default()).font, FontConfig::Auto);

        let named = PanelConfig {
            font: FontConfig::Family("Berkeley Mono".into()),
            ..Default::default()
        };
        assert_eq!(round_trip(&named).font, named.font);
    }

    #[test]
    fn empty_object_uses_field_defaults() {
        // An older file written before `font`/`leader` existed must still parse.
        let cfg: PanelConfig = facet_json::from_str("{}").expect("deserialize");
        assert_eq!(cfg.font, FontConfig::Auto);
        assert_eq!(cfg.leader, "space");
    }

    #[test]
    fn leader_round_trips() {
        assert_eq!(round_trip(&PanelConfig::default()).leader, "space");

        let custom = PanelConfig {
            leader: "cmd-k".into(),
            ..Default::default()
        };
        assert_eq!(round_trip(&custom).leader, "cmd-k");
    }
}
