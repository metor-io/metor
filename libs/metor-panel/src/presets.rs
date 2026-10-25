//! Save/load tile-layout presets.
//!
//! Three sources:
//! - **File on disk** — caller provides the path (typically picked via the OS
//!   save dialog). Round-trip is plain `std::fs`.
//! - **Built-in directory** — `presets/` under the panel config directory. Each
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

use gpui::{App, Entity, Task};
use metor_db::DB;
use metor_proto::types::Msg;
use metor_proto_wkt::{Preset, PresetDefs};

use crate::msg_ingest::{self, IngestSource};
use crate::tiles::TileGroup;

/// Resolve the per-user preset directory path. Does **not** create it —
/// list/read paths shouldn't materialize the directory just by being walked.
fn presets_dir_path() -> io::Result<PathBuf> {
    Ok(crate::config::config_dir()?.join("presets"))
}

/// Resolve and create the per-user preset directory. Used by the write path.
fn presets_dir() -> io::Result<PathBuf> {
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
    let already_has_ext = Path::new(&safe)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"));
    let file_name = if already_has_ext {
        safe
    } else {
        format!("{safe}.json")
    };
    let path = presets_dir()?.join(file_name);
    fs::write(&path, json)?;
    Ok(path)
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

/// The shared store, or `None` if it was never initialized (e.g. in tests).
pub fn try_target_presets(cx: &App) -> Option<Entity<TargetPresetStore>> {
    msg_ingest::global(cx)
}

impl TargetPresetStore {
    pub fn init(db: Arc<DB>, cx: &mut App) {
        let entity = msg_ingest::install(cx, |cx| {
            let sources = vec![IngestSource::new(
                PresetDefs::ID,
                |store: &mut Self, _ts, defs: PresetDefs| store.presets = defs.presets,
            )];
            Self {
                presets: Vec::new(),
                _task: msg_ingest::spawn_ingest(db, sources, cx),
            }
        });
        cx.observe(&entity, apply_shipped_preset).detach();
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

/// The layout `examples/adcs-fsw3/presets/star-tracker.json` holds, built
/// from the views' own config types so its component ids stay right. Run
/// with `UPDATE_PRESETS=1` to rewrite the file.
#[cfg(test)]
mod star_tracker_demo {
    use std::sync::Arc;

    use gpui::prelude::*;
    use metor_proto::types::ComponentId;
    use metor_proto_wkt::{SplitAxis, TabOrientation, TileItem, TileLayout, TileNode, TilePane};

    use crate::tiles::{ItemRegistry, SUPPORTED_LAYOUT_VERSION, TileGroup};
    use crate::views::dashboard::WidgetRegistry;
    use crate::views::image_view::{ImageViewConfig, OverlayColor, OverlayConfig};
    use crate::views::time_series::{PlotPanelConfig, TraceConfig, YAxisConfig};

    const PATH: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/adcs-fsw3/presets/star-tracker.json"
    );

    fn pane(kind: &str, state: String) -> TileNode {
        TileNode::Pane(TilePane {
            active_index: 0,
            tab_orientation: TabOrientation::Horizontal,
            hide_tab_bar: false,
            locked_size: None,
            tab_rail_width: None,
            items: vec![TileItem {
                kind: kind.into(),
                state,
            }],
        })
    }

    /// A plot of `(component, element, label, stroke)` traces.
    fn plot(title: &str, traces: &[(&str, usize, &str, f32)]) -> TileNode {
        let colors = &crate::theme::DARK.line_colors;
        let config = PlotPanelConfig {
            label: title.into(),
            custom_title: crate::views::time_series::Override::Custom(title.into()),
            axes: vec![YAxisConfig::default()],
            traces: traces
                .iter()
                .enumerate()
                .map(
                    |(i, &(component, element_index, label, stroke_width))| TraceConfig {
                        component_id: ComponentId::new(component),
                        element_index,
                        color: colors[i % colors.len()],
                        label: label.into(),
                        stroke_width,
                        ..Default::default()
                    },
                )
                .collect(),
            ..Default::default()
        };
        pane("time_series_plot", serde_json::to_string(&config).unwrap())
    }

    fn layout() -> TileLayout {
        const EST: &str = "sim.nav.attitude_estimate.q_hat_b_eci";
        const TRUTH: &str = "sim.plant.body.q_b_eci";
        let image = ImageViewConfig {
            component: "sim.star_sim.star_image.pixels".into(),
            overlays: vec![
                OverlayConfig::from_root("sim.star_sim.star_truth", OverlayColor::Truth),
                OverlayConfig::from_root("sim.star_tracker.star_centroids", OverlayColor::Detected),
            ],
            ..Default::default()
        };
        let quat: Vec<_> = (0..4)
            .flat_map(|i| {
                [
                    (
                        TRUTH,
                        i,
                        ["truth q0", "truth q1", "truth q2", "truth q3"][i],
                        3.0,
                    ),
                    (EST, i, ["est q0", "est q1", "est q2", "est q3"][i], 1.5),
                ]
            })
            .collect();
        TileLayout {
            version: SUPPORTED_LAYOUT_VERSION,
            global_time_range: String::new(),
            temporal: None,
            root: TileNode::Split(metor_proto_wkt::TileSplit {
                axis: SplitAxis::Horizontal,
                flexes: vec![1.2, 1.0],
                children: vec![
                    pane("image_view", serde_json::to_string(&image).unwrap()),
                    TileNode::Split(metor_proto_wkt::TileSplit {
                        axis: SplitAxis::Vertical,
                        flexes: vec![1.0, 1.0, 1.0],
                        children: vec![
                            plot("Attitude: nav estimate vs truth (body←ECI)", &quat),
                            plot(
                                "Star tracker",
                                &[
                                    (
                                        "sim.star_tracker.star_solution.n_matched",
                                        0,
                                        "n_matched",
                                        1.5,
                                    ),
                                    (
                                        "sim.star_tracker.star_solution.n_centroids",
                                        0,
                                        "n_centroids",
                                        1.5,
                                    ),
                                    (
                                        "sim.star_sim.star_truth.n_detectable",
                                        0,
                                        "rendered detectable",
                                        1.5,
                                    ),
                                ],
                            ),
                            plot(
                                "Nav stats",
                                &[
                                    ("sim.nav.nav_stats.stars_used", 0, "stars_used", 1.5),
                                    ("sim.nav.nav_stats.stars_gated", 0, "stars_gated", 1.5),
                                    ("sim.nav.nav_stats.prior_resets", 0, "prior_resets", 1.5),
                                    ("sim.nav.nav_stats.contradictions", 0, "contradictions", 1.5),
                                ],
                            ),
                        ],
                    }),
                ],
            }),
        }
    }

    #[gpui::test]
    fn star_tracker_preset_is_current_and_loads(cx: &mut gpui::TestAppContext) {
        let json = serde_json::to_string_pretty(&layout()).unwrap() + "\n";
        if std::env::var_os("UPDATE_PRESETS").is_some() {
            std::fs::create_dir_all(std::path::Path::new(PATH).parent().unwrap()).unwrap();
            std::fs::write(PATH, &json).unwrap();
        }
        let on_disk = std::fs::read_to_string(PATH).expect("preset exists");
        assert_eq!(on_disk, json, "stale preset; rerun with UPDATE_PRESETS=1");

        // Every item rehydrates as its real view, not a placeholder.
        let temp = tempfile::tempdir().unwrap();
        let db = Arc::new(metor_db::DB::create(temp.path().join("db")).unwrap());
        let registry = cx.update(|cx| {
            WidgetRegistry::init(cx);
            let mut panes = ItemRegistry::default();
            for spec in cx.global::<WidgetRegistry>().tile_specs() {
                panes.register_view(spec, db.clone());
            }
            panes
        });
        let tiles = cx.update(|cx| cx.new(|cx| TileGroup::new(Vec::new(), cx)));
        cx.update(|cx| {
            tiles.update(cx, |this, cx| {
                *this = TileGroup::from_json(&on_disk, &registry, cx).unwrap();
            })
        });
        cx.read(|cx| {
            let kinds: Vec<_> = tiles
                .read(cx)
                .panes()
                .iter()
                .flat_map(|p| p.read(cx).items().iter().map(|i| i.serialization_key()))
                .collect();
            assert_eq!(kinds.len(), 4);
            assert_eq!(
                kinds.iter().filter(|k| **k == "time_series_plot").count(),
                3
            );
            assert!(kinds.contains(&"image_view"));
        });
    }
}
