//! Slippy map with a telemetry position on it.
//!
//! Raster tiles (Mapbox by default, any `{z}/{x}/{y}` server via config)
//! are fetched through the shared [`tiles::TileStore`] and blitted under a
//! marker for the latest sample of
//! a lat/lon component plus a trail of its recent history. The camera
//! follows the position until the operator pans away; a double-click hands
//! control back. An unbound map is still a map — it pans and zooms freely,
//! which is also what a freshly-placed one shows while the operator picks a
//! component.
//!
//! The binding reads two elements of one component (latitude and longitude
//! in degrees, indices configurable), which is the shape a `[lat, lon, alt]`
//! triple like the ADCS example's `gps.lla` publishes.
//!
//! The trail is the plots' data model in miniature: the visible window is
//! the app-wide [`GlobalTimeRange`] (or this map's own override), and when
//! the raw history over that window is too dense to decode per frame the
//! trail reads the component's LoD companions instead — bucket midpoints
//! stand in for samples, which for a smooth track is exactly a decimation.

use std::sync::Arc;

use gpui::{
    Bounds, ContentMask, Context, Corners, Edges, IntoElement, MouseButton, PathBuilder, Pixels,
    Point, RenderImage, SharedString, Window, canvas, div, point, prelude::*, px,
};
use metor_db::{Component, DB};
use metor_proto::types::{ComponentId, Timestamp};
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use super::binding::{component_meta, spawn_elements_stream};
use super::time_series::Override;
use super::time_series::time_range::{GlobalTimeRange, TimeRangeBehavior};
use crate::theme::{Theme, theme};

pub mod mercator;
pub mod tiles;

use mercator::{Camera, Mercator, TileId, VisibleTile, fallback_rect, project, unproject};

/// Most positions a trail draws. Unlike the plots' GPU budget this is paid
/// in CPU decode per repaint, and a polyline stops gaining fidelity long
/// before this many segments.
const TRAIL_POINTS: usize = 2048;

/// Raw-sample count over the window above which the trail switches to LoD
/// buckets, with hysteresis so panning near the boundary doesn't flap.
const TRAIL_RAW_ENTER: u64 = 4096 + 4096 / 4;
const TRAIL_RAW_EXIT: u64 = 4096 * 4 / 5;

/// Deepest ancestor substituted for a tile still downloading; further up is
/// a blur that costs more overdraw than it informs.
const FALLBACK_DEPTH: u8 = 3;

/// Marker dot radius.
const MARKER_PX: f32 = 5.0;

/// Alpha of the marker's interior. The pill treatment's usual 0.18 wash
/// assumes a calm theme surface underneath; over a basemap's imagery it
/// disappears, so the interior carries more of the hue while the border
/// stays the full-strength edge.
const MARKER_FILL_ALPHA: f32 = 0.45;

/// Persisted shape of a [`Map`], shared by the tile and dashboard surfaces.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct MapConfig {
    /// Component or `=` expression publishing the position; empty for an
    /// unbound map.
    pub component: String,
    /// Element indices of latitude and longitude (degrees) in the sample.
    pub lat_element: usize,
    pub lon_element: usize,
    /// Slippy tile URL template with `{z}/{x}/{y}` placeholders; empty means
    /// the built-in Mapbox source.
    pub tile_url: String,
    pub center_lat: f64,
    pub center_lon: f64,
    pub zoom: f64,
    /// This map's trail window in the time-range grammar; empty follows the
    /// app-wide range.
    pub time_range: String,
    pub follow: bool,
}

impl Default for MapConfig {
    fn default() -> Self {
        Self {
            component: String::new(),
            lat_element: 0,
            lon_element: 1,
            tile_url: String::new(),
            center_lat: 0.0,
            center_lon: 0.0,
            zoom: 2.0,
            time_range: String::new(),
            follow: true,
        }
    }
}

/// The slippy-map view entity.
#[derive(facet::Facet)]
pub struct Map {
    /// The position component. Editable: picking another rebinds on the
    /// next frame.
    pub component_id: ComponentId,
    pub lat_element: usize,
    pub lon_element: usize,
    /// Keep the camera centered on the latest sample. Panning clears it;
    /// double-click restores it.
    pub follow: bool,
    /// Trail window: `Auto` follows the app-wide [`GlobalTimeRange`].
    pub time_range: Override<TimeRangeBehavior>,
    /// Fallback name for a component nothing has registered.
    #[facet(skip)]
    component: SharedString,
    /// What the stream tasks are bound to, compared against the editable
    /// fields each frame.
    #[facet(opaque)]
    bound: Option<(ComponentId, usize, usize)>,
    #[facet(opaque)]
    camera: Camera,
    /// Latest sample as degrees, `None` until one arrives.
    #[facet(opaque)]
    position: Option<(f64, f64)>,
    /// The decoded track over the resolved window; rebuilt when the window,
    /// the source, or the data frontier moves.
    #[facet(opaque)]
    trail: Vec<Mercator>,
    /// What `trail` was decoded from: `(range start, range end, series id,
    /// newest stamp)`. Matching key, current trail.
    #[facet(opaque)]
    trail_key: Option<(i64, i64, ComponentId, i64)>,
    /// The component's LoD companions (`metor_db::lod`), finest first, and
    /// the vtable generation they were resolved at.
    #[facet(opaque)]
    lod_levels: Vec<Component>,
    #[facet(opaque)]
    lod_resolved_gen: Option<u64>,
    /// Raw-vs-LoD hysteresis state, the plots' `over_budget` in miniature.
    #[facet(opaque)]
    over_budget: bool,
    #[facet(skip)]
    tile_url: SharedString,
    /// Pan gesture: press position and the camera center it started from.
    #[facet(opaque)]
    drag: Option<(Point<Pixels>, Mercator)>,
    /// The canvas rect from the last prepaint, for cursor-local math in
    /// event handlers.
    #[facet(opaque)]
    last_bounds: Bounds<Pixels>,
    #[facet(opaque)]
    db: Arc<DB>,
    #[facet(opaque)]
    _expression: Option<crate::dynamic::expressions::Expression>,
    #[facet(opaque)]
    _store_observation: Option<gpui::Subscription>,
    #[facet(opaque)]
    _task: gpui::Task<()>,
}

impl Map {
    pub fn from_config(cfg: &MapConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let (component_id, expression) = if cfg.component.is_empty() {
            (ComponentId(0), None)
        } else {
            match crate::dynamic::expressions::bind(&cfg.component, &db, cx) {
                Ok(bound) => (bound.id, bound.expression),
                Err(_) => (ComponentId::new(&cfg.component), None),
            }
        };
        let time_range = match cfg.time_range.parse::<TimeRangeBehavior>() {
            Ok(behavior) if !cfg.time_range.is_empty() => Override::Custom(behavior),
            _ => Override::Auto,
        };
        let mut map = Self {
            component_id,
            lat_element: cfg.lat_element,
            lon_element: cfg.lon_element,
            follow: cfg.follow,
            time_range,
            component: SharedString::from(cfg.component.clone()),
            bound: None,
            camera: Camera {
                center: project(cfg.center_lat, cfg.center_lon),
                zoom: cfg.zoom.clamp(1.0, mercator::MAX_TILE_ZOOM as f64),
            },
            position: None,
            trail: Vec::new(),
            trail_key: None,
            lod_levels: Vec::new(),
            lod_resolved_gen: None,
            over_budget: false,
            tile_url: SharedString::from(cfg.tile_url.clone()),
            drag: None,
            last_bounds: Bounds::default(),
            db,
            _expression: expression,
            _store_observation: tiles::TileStore::observe(cx),
            _task: gpui::Task::ready(()),
        };
        map.rebind(cx);
        map
    }

    pub fn to_config(&self) -> MapConfig {
        let (center_lat, center_lon) = unproject(self.camera.center);
        MapConfig {
            component: crate::dynamic::expressions::binding_text(&self.db, self.component_id)
                .or_else(|| {
                    super::binding::component_name(&self.db, self.component_id)
                        .map(|name| name.to_string())
                })
                .unwrap_or_else(|| self.component.to_string()),
            lat_element: self.lat_element,
            lon_element: self.lon_element,
            tile_url: self.tile_url.to_string(),
            center_lat,
            center_lon,
            zoom: self.camera.zoom,
            time_range: self
                .time_range
                .as_custom()
                .map(ToString::to_string)
                .unwrap_or_default(),
            follow: self.follow,
        }
    }

    /// Restart the streams when the inspector has re-pointed the binding.
    pub(crate) fn rebind(&mut self, cx: &mut Context<Self>) {
        let want = (self.component_id, self.lat_element, self.lon_element);
        if self.bound == Some(want) {
            return;
        }
        self.bound = Some(want);
        self.position = None;
        self.trail.clear();
        self.trail_key = None;
        self.lod_levels.clear();
        self.lod_resolved_gen = None;
        self.over_budget = false;
        if self.component_id == ComponentId(0) {
            self._expression = None;
            self._task = gpui::Task::ready(());
            return;
        }
        self._expression = crate::dynamic::expressions::running(self.component_id, cx);
        self.component = component_meta(&self.db, self.component_id).name;
        let (lat_el, lon_el) = (self.lat_element, self.lon_element);
        let count = lat_el.max(lon_el) + 1;
        self._task = spawn_elements_stream(
            self.db.clone(),
            self.component_id,
            count,
            cx,
            move |map, values, cx| {
                map.push_sample(values[lat_el], values[lon_el]);
                cx.notify();
            },
        );
    }

    fn push_sample(&mut self, lat: f64, lon: f64) {
        if !lat.is_finite() || !lon.is_finite() {
            return;
        }
        self.position = Some((lat, lon));
        if self.follow {
            self.camera.center = project(lat, lon);
            self.camera.clamp();
        }
    }

    fn local(&self, position: Point<Pixels>) -> (f64, f64) {
        (
            f64::from(position.x - self.last_bounds.origin.x),
            f64::from(position.y - self.last_bounds.origin.y),
        )
    }

    fn size(&self) -> (f64, f64) {
        (
            f64::from(self.last_bounds.size.width),
            f64::from(self.last_bounds.size.height),
        )
    }
}

impl Map {
    /// The trail window this map actually uses: its own `Custom` range, or
    /// the app-wide [`GlobalTimeRange`] when set to `Auto`.
    fn resolved_time_range(&self, cx: &gpui::App) -> TimeRangeBehavior {
        match self.time_range.as_custom() {
            Some(behavior) => *behavior,
            None => GlobalTimeRange::get(cx),
        }
    }

    /// Bring the trail up to date with the resolved window and the data.
    ///
    /// Runs every render but decodes only when its cache key — the window
    /// endpoints, the chosen series, and its newest stamp — moves. Under
    /// live data that is once per arriving sample, bounded by
    /// [`TRAIL_POINTS`] via the LoD switch and stride decimation.
    fn rebuild_trail(&mut self, cx: &gpui::App) {
        if self.component_id == ComponentId(0) {
            return;
        }
        let Some(component) = self
            .db
            .with_state(|state| state.get_component(self.component_id).cloned())
        else {
            return;
        };
        let Some(extent) = series_extent(&component) else {
            return;
        };
        let range = self
            .resolved_time_range(cx)
            .calculate_range(extent.start, extent.end);

        // Raw-vs-LoD choice, the plots' `update_lod_state` in miniature.
        let vtable_gen = self.db.vtable_gen.latest();
        if self.lod_resolved_gen != Some(vtable_gen) {
            self.lod_levels =
                crate::views::time_series::resolve_lod_levels(&self.db, self.component_id);
            self.lod_resolved_gen = Some(vtable_gen);
        }
        let estimate = component.time_series.estimate_samples(range.clone());
        if self.over_budget {
            self.over_budget = estimate >= TRAIL_RAW_EXIT;
        } else {
            self.over_budget = estimate > TRAIL_RAW_ENTER;
        }
        let series = if self.over_budget {
            // Finest level fitting the budget wins; coarser fallback. Over
            // budget with no level published yet, draw nothing rather than
            // decode an unbounded raw window per frame.
            let mut selected = None;
            for level in &self.lod_levels {
                if level.time_series.latest().is_none() {
                    continue;
                }
                selected = Some(level);
                if level.time_series.estimate_samples(range.clone()) <= TRAIL_RAW_ENTER {
                    break;
                }
            }
            match selected {
                Some(level) => level,
                None => {
                    self.trail.clear();
                    self.trail_key = None;
                    return;
                }
            }
        } else {
            &component
        };

        let newest = series
            .time_series
            .latest()
            .map(|l| l.timestamp().0)
            .unwrap_or_default();
        let key = (range.start.0, range.end.0, series.component_id, newest);
        if self.trail_key == Some(key) {
            return;
        }
        self.trail = decode_trail(series, range, self.lat_element, self.lon_element);
        self.trail_key = Some(key);
    }
}

/// The stamps a series spans, resident nodes and manifest both — the
/// manifest is what lets a full-range window cover remote-only history.
fn series_extent(component: &Component) -> Option<std::ops::Range<Timestamp>> {
    let series = &component.time_series;
    let mut start = i64::MAX;
    let mut end = i64::MIN;
    if let Some(s) = series.start_timestamp() {
        start = start.min(s.0);
    }
    if let Some(l) = series.latest() {
        end = end.max(l.timestamp().0);
    }
    let manifest = series.manifest();
    if let Some(span) = manifest.spans.first() {
        start = start.min(span.seal.start_ts.0);
    }
    if let Some(span) = manifest.spans.last() {
        end = end.max(span.cover_end.0);
    }
    (start < end).then_some(Timestamp(start)..Timestamp(end))
}

/// Decode a series' positions over `range` into at most [`TRAIL_POINTS`]
/// mercator points, oldest first.
///
/// Works on the raw component and its LoD companions alike: a LoD sample
/// is `[min_e0..min_eN, max_e0..max_eN]` (`metor_db::lod::lod_schema`),
/// and a bucket's per-element midpoint stands in for the samples it
/// folded — for a smooth track that is exactly a decimation.
fn decode_trail(
    series: &Component,
    range: std::ops::Range<Timestamp>,
    lat_el: usize,
    lon_el: usize,
) -> Vec<Mercator> {
    let is_lod = series.schema.dim.len() > 1 && series.schema.dim.first() == Some(&2);
    let elements: usize = series.schema.dim.iter().product::<usize>().max(1);
    let n = if is_lod { elements / 2 } else { elements };
    if lat_el.max(lon_el) >= n {
        return Vec::new();
    }
    let count = if is_lod {
        n + lat_el.max(lon_el) + 1
    } else {
        lat_el.max(lon_el) + 1
    };

    let end = Timestamp(range.end.0.saturating_add(1));
    let Some(slice) = series.time_series.get_range(range.start..end) else {
        return Vec::new();
    };
    // Node slices arrive newest first; samples within a node are oldest
    // first. Collecting nodes and reversing restores one oldest-first walk.
    let nodes: Vec<_> = slice.as_iter().collect();
    let total: usize = nodes.iter().map(|node| node.timestamps().len()).sum();
    let stride = total.div_ceil(TRAIL_POINTS).max(1);

    let mut points = Vec::with_capacity(total.min(TRAIL_POINTS) + 1);
    let mut index = 0usize;
    for node in nodes.iter().rev() {
        for (_ts, view) in node.iter_values(&series.schema) {
            let take = index.is_multiple_of(stride);
            index += 1;
            if !take {
                continue;
            }
            let values: SmallVec<[f64; 8]> = view
                .iter()
                .take(count)
                .map(|value| value.as_f64())
                .collect();
            if values.len() < count {
                continue;
            }
            let (lat, lon) = if is_lod {
                (
                    (values[lat_el] + values[n + lat_el]) / 2.0,
                    (values[lon_el] + values[n + lon_el]) / 2.0,
                )
            } else {
                (values[lat_el], values[lon_el])
            };
            if lat.is_finite() && lon.is_finite() {
                points.push(project(lat, lon));
            }
        }
    }
    points
}

/// One tile blit resolved for this frame.
struct TileDraw {
    origin: (f64, f64),
    size: f64,
    image: Arc<RenderImage>,
}

/// Resolve every visible tile against the store: exact hits, ancestor
/// stand-ins for the rest, and any evicted images to release.
fn resolve_tiles(
    store: &mut tiles::TileStore,
    camera: &Camera,
    size: (f64, f64),
    bias: f64,
    source: &tiles::TileSource,
) -> (Vec<Arc<RenderImage>>, Vec<TileDraw>, Vec<TileDraw>) {
    let orphans = store.take_orphans();
    let mut exact = Vec::new();
    let mut fallback: Vec<TileDraw> = Vec::new();
    // Two wrapped columns of one antimeridian-straddling ancestor need one
    // blit each, but a 2×2 block of missing children needs only one; the
    // rect identifies the blit either way.
    let mut fallback_rects: Vec<(TileId, i64)> = Vec::new();
    for VisibleTile { id, origin, size } in mercator::visible_tiles(camera, size, bias) {
        if let Some(image) = store.request(id, source) {
            exact.push(TileDraw {
                origin,
                size,
                image,
            });
            continue;
        }
        let mut ancestor = id;
        for _ in 0..FALLBACK_DEPTH {
            let Some(up) = ancestor.parent() else { break };
            ancestor = up;
            let Some(image) = store.ready(ancestor, source) else {
                continue;
            };
            let (fb_origin, fb_size) = fallback_rect(id, ancestor, origin, size);
            let key = (ancestor, fb_origin.0.round() as i64);
            if !fallback_rects.contains(&key) {
                fallback_rects.push(key);
                fallback.push(TileDraw {
                    origin: fb_origin,
                    size: fb_size,
                    image,
                });
            }
            break;
        }
    }
    (orphans, exact, fallback)
}

/// The trail point closest to `center`'s replica of the world, so a track
/// crossing the antimeridian draws beside the camera rather than a world
/// away.
fn wrap_toward(m: Mercator, center: Mercator) -> Mercator {
    Mercator {
        x: m.x + (center.x - m.x).round(),
        y: m.y,
    }
}

fn paint_tile(draw: &TileDraw, origin: Point<Pixels>, window: &mut Window) {
    let bounds = Bounds::new(
        point(
            origin.x + px(draw.origin.0 as f32),
            origin.y + px(draw.origin.1 as f32),
        ),
        gpui::size(px(draw.size as f32), px(draw.size as f32)),
    );
    let _ = window.paint_image(bounds, Corners::default(), draw.image.clone(), 0, false);
}

impl Render for Map {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.rebind(cx);
        self.rebuild_trail(cx);
        let theme = theme(cx);

        let camera = self.camera;
        let position = self.position;
        let trail = self.trail.clone();
        let source = tiles::source_for(&self.tile_url);
        let attribution = source.attribution;
        let trail_color = theme.map_trail();
        let marker_color = theme.map_marker();
        let backdrop = theme.bg_secondary;

        let map_canvas = canvas(
            {
                let this = cx.entity().downgrade();
                move |bounds, _window, cx| {
                    let _ = this.update(cx, |this, _| this.last_bounds = bounds);
                    bounds
                }
            },
            move |bounds: Bounds<Pixels>, _, window, cx| {
                window.with_content_mask(Some(ContentMask { bounds }), |window| {
                    window.paint_quad(gpui::fill(bounds, backdrop));
                    let size = (f64::from(bounds.size.width), f64::from(bounds.size.height));
                    let origin = bounds.origin;

                    if let Some(store) = tiles::try_global(cx) {
                        let bias = source.zoom_bias(f64::from(window.scale_factor()));
                        let (orphans, exact, fallback) = store.update(cx, |store, _| {
                            resolve_tiles(store, &camera, size, bias, &source)
                        });
                        for image in orphans {
                            let _ = window.drop_image(image);
                        }
                        // Stand-ins first so every exact tile wins where
                        // both cover a cell.
                        for draw in fallback.iter().chain(exact.iter()) {
                            paint_tile(draw, origin, window);
                        }
                    }

                    if trail.len() >= 2 {
                        let mut path = PathBuilder::stroke(px(1.5));
                        let mut last: Option<Mercator> = None;
                        for m in trail.iter().map(|m| wrap_toward(*m, camera.center)) {
                            let (x, y) = camera.to_screen(m, size);
                            let p = point(origin.x + px(x as f32), origin.y + px(y as f32));
                            // A jump of half the world is the trail crossing
                            // the antimeridian, not the vehicle teleporting.
                            let jumped = last.map(|l| (l.x - m.x).abs() > 0.5).unwrap_or(true);
                            if jumped {
                                path.move_to(p);
                            } else {
                                path.line_to(p);
                            }
                            last = Some(m);
                        }
                        if let Ok(path) = path.build() {
                            window.paint_path(path, trail_color);
                        }
                    }

                    if let Some((lat, lon)) = position {
                        let m = wrap_toward(project(lat, lon), camera.center);
                        let (x, y) = camera.to_screen(m, size);
                        let center = point(origin.x + px(x as f32), origin.y + px(y as f32));
                        let dot = Bounds::new(
                            point(center.x - px(MARKER_PX), center.y - px(MARKER_PX)),
                            gpui::size(px(MARKER_PX * 2.0), px(MARKER_PX * 2.0)),
                        );
                        // The house pill treatment: the hue washed out
                        // inside, full strength around the edge.
                        let mut quad = gpui::fill(dot, Theme::dim(marker_color, MARKER_FILL_ALPHA));
                        quad.corner_radii = Corners::all(px(MARKER_PX));
                        quad.border_widths = Edges::all(px(1.5));
                        quad.border_color = marker_color;
                        window.paint_quad(quad);
                    }
                });
            },
        );

        let mut root = div()
            .size_full()
            .relative()
            .overflow_hidden()
            .bg(theme.bg_secondary)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, _window, cx| {
                    if event.click_count == 2 {
                        // Hand the camera back to the data.
                        this.follow = true;
                        if let Some((lat, lon)) = this.position {
                            this.camera.center = project(lat, lon);
                        }
                        cx.notify();
                    } else {
                        this.drag = Some((event.position, this.camera.center));
                    }
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _event: &gpui::MouseUpEvent, _window, _cx| {
                    this.drag = None;
                }),
            )
            .on_mouse_move(
                cx.listener(|this, event: &gpui::MouseMoveEvent, _window, cx| {
                    if !event.dragging() {
                        return;
                    }
                    let Some((start, start_center)) = this.drag else {
                        return;
                    };
                    // The first real pan is what takes the camera off the data.
                    this.follow = false;
                    let world = this.camera.zoom.exp2() * mercator::TILE_PX;
                    this.camera.center = Mercator {
                        x: start_center.x - f64::from(event.position.x - start.x) / world,
                        y: start_center.y - f64::from(event.position.y - start.y) / world,
                    };
                    this.camera.clamp();
                    cx.notify();
                }),
            )
            .on_scroll_wheel(
                cx.listener(|this, event: &gpui::ScrollWheelEvent, _window, cx| {
                    let delta = event.delta.pixel_delta(px(20.0));
                    let zoom_amount = f64::from(f32::from(-delta.y)) / 200.0;
                    let factor = (1.0 + zoom_amount).clamp(0.5, 2.0);
                    let cursor = this.local(event.position);
                    this.camera.zoom_at(cursor, this.size(), factor);
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .child(map_canvas.size_full());

        // Every tile provider's terms require visible attribution wherever
        // its imagery shows.
        root = root.child(
            div()
                .absolute()
                .bottom_0()
                .right_0()
                .px(px(4.0))
                .py(px(1.0))
                .bg(theme.plot_chrome_bg())
                .text_size(px(9.0))
                .text_color(theme.text_secondary)
                .child(attribution),
        );

        if self.component_id == ComponentId(0) {
            root = root.child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(11.0))
                    .text_color(theme.text_tertiary)
                    .child("no position bound — pick a component in the inspector"),
            );
        }

        root
    }
}
