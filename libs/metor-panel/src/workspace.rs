//! Free functions over the set of open panel windows.
//!
//! There is no window manager: the window list is derived from gpui's own
//! registry on demand ([`gpui::App::windows`]), so opening and closing
//! windows needs no bookkeeping that could go stale. [`open_panel_window`]
//! is the one constructor — boot, dock reopen, and tab tear-out all go
//! through it.

use std::sync::Arc;

use gpui::{
    App, AppContext as _, Bounds, Entity, Pixels, TitlebarOptions, WeakEntity, Window,
    WindowBounds, WindowHandle, WindowId, WindowOptions, point, px, size,
};
use metor_db::DB;

use crate::app::AppRoot;
use crate::tiles::{LoadError, TileGroup, TileLayout};

/// Version of the multi-window layout document. Independent of the
/// per-window [`TileLayout`] version, which each window's tree still
/// carries and checks.
pub(crate) const WORKSPACE_LAYOUT_VERSION: u32 = 1;

/// The document written to per-target layout files: one tile tree per open
/// window plus its screen bounds. Named presets and target-shipped presets
/// deliberately stay single-window [`TileLayout`] documents — a preset
/// describes an arrangement, not the user's monitor setup.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct WorkspaceLayout {
    pub version: u32,
    pub windows: Vec<WindowLayout>,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct WindowLayout {
    /// Screen-space `(x, y, width, height)` in gpui pixels; `None` centers.
    #[serde(default)]
    pub bounds: Option<(f32, f32, f32, f32)>,
    pub layout: TileLayout,
}

/// Every open panel window, ordered by window id so iteration (layout
/// serialization, "first window" fallbacks) is deterministic in a session.
pub(crate) fn panel_windows(cx: &App) -> Vec<WindowHandle<AppRoot>> {
    let mut windows: Vec<_> = cx
        .windows()
        .into_iter()
        .filter_map(|w| w.downcast::<AppRoot>())
        .collect();
    windows.sort_by_key(|w| w.window_id());
    windows
}

/// Window-id-keyed tile trees, maintained by [`open_panel_window`].
///
/// This map exists because a window mid-dispatch has its gpui slot taken,
/// making its root view unreachable through `WindowHandle::read` — exactly
/// when the palette, opened from inside that window's dispatch, asks for
/// its tiles. Entities read fine regardless, so the tree is reachable by
/// window id when the root view is not. Weak entries die with their window
/// and are pruned on the next insert.
#[derive(Default)]
struct WindowTiles(Vec<(WindowId, WeakEntity<TileGroup>)>);

impl gpui::Global for WindowTiles {}

fn register_window_tiles(id: WindowId, tiles: &Entity<TileGroup>, cx: &mut App) {
    let map = cx.default_global::<WindowTiles>();
    map.0.retain(|(_, weak)| weak.upgrade().is_some());
    map.0.push((id, tiles.downgrade()));
}

/// The tile tree of the active window, falling back to the lowest-id live
/// window. For a callback holding a `Window`, prefer [`tiles_for`] — it
/// names the window precisely instead of trusting focus.
pub(crate) fn active_tiles(cx: &App) -> Option<Entity<TileGroup>> {
    let map = cx.try_global::<WindowTiles>()?;
    let active = cx.active_window().map(|w| w.window_id());
    if let Some(tiles) = active
        .and_then(|id| map.0.iter().find(|(wid, _)| *wid == id))
        .and_then(|(_, weak)| weak.upgrade())
    {
        return Some(tiles);
    }
    map.0
        .iter()
        .filter_map(|(id, weak)| weak.upgrade().map(|tiles| (*id, tiles)))
        .min_by_key(|(id, _)| *id)
        .map(|(_, tiles)| tiles)
}

/// The tile tree owned by `window`'s root, when it is a panel window.
pub(crate) fn tiles_for(window: &Window, cx: &App) -> Option<Entity<TileGroup>> {
    let root = window.root::<AppRoot>().flatten()?;
    Some(root.read(cx).tiles().clone())
}

/// Whether any panel window holds an item: the multi-window blank-slate
/// test consent checks use before replacing what's on screen.
pub(crate) fn any_window_has_items(cx: &App) -> bool {
    let Some(map) = cx.try_global::<WindowTiles>() else {
        return false;
    };
    map.0
        .iter()
        .filter_map(|(_, weak)| weak.upgrade())
        .any(|tiles| tiles.read(cx).has_items(cx))
}

/// Open one panel window. `bounds` places it in screen coordinates
/// (sanitized against the live displays); `tiles` seeds it with an existing
/// tree — a torn-out tab, a restored layout — instead of a fresh empty
/// pane. `show_picker_if_disconnected` greets a fresh session with the
/// connection picker, wanted at boot and dock reopen but not for a window
/// that exists only to hold content.
pub(crate) fn open_panel_window(
    db: Arc<DB>,
    bounds: Option<Bounds<Pixels>>,
    tiles: Option<Entity<TileGroup>>,
    show_picker_if_disconnected: bool,
    cx: &mut App,
) -> Option<WindowHandle<AppRoot>> {
    let bounds = bounds
        .filter(|b| bounds_visible(b, &display_bounds(cx)))
        .unwrap_or_else(|| Bounds::centered(None, size(px(1024.), px(600.)), cx));
    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            // `appears_transparent` hides the native titlebar on macOS
            // (leaving the traffic lights) and on Windows (leaving nothing —
            // the app draws its own controls).
            titlebar: Some(TitlebarOptions {
                appears_transparent: true,
                traffic_light_position: Some(point(px(12.0), px(8.0))),
                ..Default::default()
            }),
            // Linux-only: request client-side decorations; the window root
            // wraps itself in resize borders and a shadow via
            // `window_controls::client_side_decorations`.
            window_decorations: Some(gpui::WindowDecorations::Client),
            app_id: Some("metor-panel".into()),
            ..Default::default()
        },
        move |window, cx| {
            // Snapshot the layout while the closing window still counts:
            // this is the only hook that runs before gpui drops it, and it
            // is what lets closing the last window behave like quitting.
            window.on_window_should_close(cx, |window, cx| {
                crate::connections::flush_layout_including(window, cx);
                true
            });
            let root = cx.new(|cx| AppRoot::new(db, tiles, show_picker_if_disconnected, cx));
            let tiles = root.read(cx).tiles().clone();
            register_window_tiles(window.window_handle().window_id(), &tiles, cx);
            root
        },
    )
    .inspect_err(|err| tracing::error!(%err, "open panel window failed"))
    .ok()
}

/// Serialize every open panel window into a [`WorkspaceLayout`] JSON
/// document, or `None` when no panel window is open — the caller keeps its
/// previous snapshot rather than persisting emptiness.
///
/// `extra` carries a window that can't be reached through its handle: the
/// one currently dispatching its own close, whose slot gpui has taken.
/// Build it with [`window_layout`].
pub(crate) fn serialize_workspace(
    extra: Option<(WindowId, WindowLayout)>,
    cx: &mut App,
) -> Option<String> {
    let mut windows: Vec<(WindowId, WindowLayout)> = panel_windows(cx)
        .into_iter()
        .filter_map(|handle| {
            let layout = handle
                .update(cx, |root, window, cx| WindowLayout {
                    bounds: Some(bounds_tuple(window.bounds())),
                    layout: root.tiles().read(cx).serialize(cx),
                })
                .ok()?;
            Some((handle.window_id(), layout))
        })
        .collect();
    if let Some((id, layout)) = extra
        && !windows.iter().any(|(existing, _)| *existing == id)
    {
        windows.push((id, layout));
    }
    if windows.is_empty() {
        return None;
    }
    windows.sort_by_key(|(id, _)| *id);
    let workspace = WorkspaceLayout {
        version: WORKSPACE_LAYOUT_VERSION,
        windows: windows.into_iter().map(|(_, layout)| layout).collect(),
    };
    Some(serde_json::to_string(&workspace).expect("workspace layout always serializes"))
}

/// Snapshot one window from in-hand references, for close-time hooks that
/// run while the window is mid-dispatch and unreachable through its handle.
pub(crate) fn window_layout(window: &Window, cx: &App) -> Option<(WindowId, WindowLayout)> {
    let tiles = tiles_for(window, cx)?;
    Some((
        window.window_handle().window_id(),
        WindowLayout {
            bounds: Some(bounds_tuple(window.bounds())),
            layout: tiles.read(cx).serialize(cx),
        },
    ))
}

/// Parse a layout file: the multi-window document, or — for files written
/// before multi-window — a bare single-tree [`TileLayout`], wrapped as one
/// centered window. Per-window tree versions are checked on restore.
pub(crate) fn parse_workspace(json: &str) -> Result<WorkspaceLayout, LoadError> {
    if let Ok(workspace) = serde_json::from_str::<WorkspaceLayout>(json) {
        if workspace.version != WORKSPACE_LAYOUT_VERSION {
            return Err(LoadError::UnsupportedVersion(workspace.version));
        }
        return Ok(workspace);
    }
    let layout: TileLayout = serde_json::from_str(json)?;
    Ok(WorkspaceLayout {
        version: WORKSPACE_LAYOUT_VERSION,
        windows: vec![WindowLayout {
            bounds: None,
            layout,
        }],
    })
}

/// Restore a workspace document: the first window's tree replaces
/// `window`'s tiles in place (keeping the live bounds the user is looking
/// at), and every further window opens at its saved bounds. Returns whether
/// the first tree loaded.
pub(crate) fn restore_workspace(
    json: &str,
    window: &mut Window,
    cx: &mut App,
    db: Arc<DB>,
) -> bool {
    let workspace = match parse_workspace(json) {
        Ok(workspace) => workspace,
        Err(err) => {
            tracing::error!(%err, "failed to load workspace layout");
            return false;
        }
    };
    let mut windows = workspace.windows.into_iter();
    let Some(first) = windows.next() else {
        return false;
    };
    let Some(tiles) = tiles_for(window, cx) else {
        return false;
    };
    let loaded = tiles.update(cx, |tiles, cx| {
        tiles
            .replace_from_layout(first.layout, cx)
            .inspect_err(|err| tracing::error!(%err, "failed to load layout"))
            .is_ok()
    });
    for saved in windows {
        let db = db.clone();
        // Deferred: the caller is mid-dispatch inside `window`.
        cx.defer(move |cx| {
            if saved.layout.version != crate::tiles::SUPPORTED_LAYOUT_VERSION {
                tracing::error!(
                    version = saved.layout.version,
                    "skipping saved window with unsupported layout version"
                );
                return;
            }
            let registry = cx.global::<crate::tiles::ItemRegistry>().clone();
            let tiles = cx.new(|cx| TileGroup::deserialize(saved.layout, &registry, cx));
            let bounds = saved.bounds.map(|(x, y, w, h)| Bounds {
                origin: point(px(x), px(y)),
                size: size(px(w), px(h)),
            });
            open_panel_window(db, bounds, Some(tiles), false, cx);
        });
    }
    loaded
}

fn bounds_tuple(bounds: Bounds<Pixels>) -> (f32, f32, f32, f32) {
    (
        bounds.origin.x.into(),
        bounds.origin.y.into(),
        bounds.size.width.into(),
        bounds.size.height.into(),
    )
}

fn display_bounds(cx: &App) -> Vec<Bounds<Pixels>> {
    cx.displays().iter().map(|d| d.bounds()).collect()
}

/// Whether any display shows at least part of `bounds` — the test that
/// keeps a window saved on a since-unplugged monitor from restoring
/// off-screen.
fn bounds_visible(bounds: &Bounds<Pixels>, displays: &[Bounds<Pixels>]) -> bool {
    displays.iter().any(|d| d.intersects(bounds))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tiles::serial::{TileNode, TilePane};
    use gpui::{point, px, size};

    fn bounds(x: f32, y: f32, w: f32, h: f32) -> Bounds<Pixels> {
        Bounds {
            origin: point(px(x), px(y)),
            size: size(px(w), px(h)),
        }
    }

    fn minimal_layout(version: u32) -> TileLayout {
        TileLayout {
            version,
            global_time_range: String::new(),
            root: TileNode::Pane(TilePane {
                active_index: 0,
                tab_orientation: Default::default(),
                hide_tab_bar: false,
                locked_size: None,
                items: Vec::new(),
            }),
        }
    }

    #[test]
    fn workspace_documents_round_trip() {
        let json = serde_json::to_string(&WorkspaceLayout {
            version: WORKSPACE_LAYOUT_VERSION,
            windows: vec![WindowLayout {
                bounds: Some((10.0, 20.0, 800.0, 600.0)),
                layout: minimal_layout(crate::tiles::SUPPORTED_LAYOUT_VERSION),
            }],
        })
        .unwrap();
        let parsed = parse_workspace(&json).unwrap();
        assert_eq!(parsed.windows.len(), 1);
        assert_eq!(parsed.windows[0].bounds, Some((10.0, 20.0, 800.0, 600.0)));
    }

    /// A layout file written before multi-window is a bare tile tree; it
    /// loads as one centered window with its own version intact.
    #[test]
    fn a_bare_single_tree_layout_wraps_as_one_window() {
        let json =
            serde_json::to_string(&minimal_layout(crate::tiles::SUPPORTED_LAYOUT_VERSION)).unwrap();
        let parsed = parse_workspace(&json).unwrap();
        assert_eq!(parsed.windows.len(), 1);
        assert!(parsed.windows[0].bounds.is_none());
        assert_eq!(
            parsed.windows[0].layout.version,
            crate::tiles::SUPPORTED_LAYOUT_VERSION
        );
    }

    #[test]
    fn an_unsupported_workspace_version_is_refused() {
        let json = serde_json::to_string(&WorkspaceLayout {
            version: 999,
            windows: vec![],
        })
        .unwrap();
        assert!(matches!(
            parse_workspace(&json),
            Err(LoadError::UnsupportedVersion(999))
        ));
    }

    #[test]
    fn bounds_on_a_live_display_are_visible() {
        let displays = [bounds(0.0, 0.0, 1920.0, 1080.0)];
        assert!(bounds_visible(&bounds(100.0, 100.0, 800.0, 600.0), &displays));
        // Partially off-screen still counts — the user can drag it back.
        assert!(bounds_visible(&bounds(1800.0, 900.0, 800.0, 600.0), &displays));
    }

    #[test]
    fn bounds_on_a_vanished_display_are_not() {
        let displays = [bounds(0.0, 0.0, 1920.0, 1080.0)];
        // A window saved on a second monitor that's gone.
        assert!(!bounds_visible(
            &bounds(2000.0, 0.0, 800.0, 600.0),
            &displays
        ));
        assert!(!bounds_visible(&bounds(0.0, 0.0, 800.0, 600.0), &[]));
    }
}
