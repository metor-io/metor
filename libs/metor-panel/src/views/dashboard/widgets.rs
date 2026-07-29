//! Widget-kind registry and the leaf renderers that back built-in kinds.
//!
//! [`WidgetRegistry`] is a gpui `Global` mapping [`WidgetKind`] to its
//! [`WidgetSpec`]. Downstream consumers can register or override specs at
//! startup via [`WidgetRegistry::register`].
use std::collections::HashMap;
use std::sync::Arc;

use gpui::{
    AnyView, App, Context, Corners, Entity, Hsla, IntoElement, Render, RenderImage, SharedString,
    Window, canvas, div, prelude::*, px,
};
use image::{Frame, ImageBuffer, Rgba};
use metor_db::DB;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use crate::theme::theme;
use crate::tiles::panels::{PlotPanelConfig, TraceConfig};
use crate::views::time_series::{LinePlot, Trace};
use crate::views::viewer_3d::Viewer3d;
use crate::views::{
    AttitudeConfig, AttitudeIndicator, ComponentText, Gauge, GaugeConfig, Meter, MeterConfig,
    Monitor, SequenceControl, SequenceControlConfig, StateChip, StateChipConfig, TimeSeriesPlot,
    TrafficLight, TrafficLightGrid, new_component_table,
};

use super::{DashboardWidget, WidgetKind};

/// Everything the dashboard needs to create and label one [`WidgetKind`].
///
/// Uses `Arc<dyn Fn>` so specs can capture owned state (shared resources,
/// remote handles) instead of being limited to plain function pointers.
pub struct WidgetSpec {
    pub default_size: (f32, f32),
    pub label: Arc<dyn Fn(&DashboardWidget) -> SharedString>,
    /// Build receives the widget's persisted config string. Each builder
    /// chooses how to parse it (typically `serde_json::from_str` into a
    /// kind-specific config struct).
    pub build: Arc<dyn Fn(&str, &Arc<DB>, &mut App) -> (AnyView, gpui::AnyEntity)>,
}

/// Global table of every widget kind the dashboard can render.
pub struct WidgetRegistry {
    specs: HashMap<WidgetKind, Arc<WidgetSpec>>,
}

impl gpui::Global for WidgetRegistry {}

impl WidgetRegistry {
    pub fn init(cx: &mut App) {
        let mut reg = Self {
            specs: HashMap::new(),
        };
        reg.register_defaults();
        cx.set_global(reg);
    }

    pub fn register(&mut self, kind: WidgetKind, spec: WidgetSpec) {
        self.specs.insert(kind, Arc::new(spec));
    }

    pub fn spec(&self, kind: &WidgetKind) -> Option<Arc<WidgetSpec>> {
        self.specs.get(kind).cloned()
    }

    fn register_defaults(&mut self) {
        self.register(
            WidgetKind::plot(),
            WidgetSpec {
                default_size: (400.0, 250.0),
                label: Arc::new(|w| SharedString::from(format!("Plot #{}", w.id.0))),
                build: Arc::new(build_plot),
            },
        );
        self.register(
            WidgetKind::text(),
            WidgetSpec {
                default_size: (160.0, 60.0),
                label: Arc::new(|w| {
                    let cfg = parse_or_default::<TextWidgetConfig>(&w.config);
                    SharedString::from(format!("Text: {}", display_or_unknown(&cfg.component)))
                }),
                build: Arc::new(build_text),
            },
        );
        self.register(
            WidgetKind::table(),
            WidgetSpec {
                default_size: (400.0, 300.0),
                label: Arc::new(|w| SharedString::from(format!("Table #{}", w.id.0))),
                build: Arc::new(build_table),
            },
        );
        self.register(
            WidgetKind::image(),
            WidgetSpec {
                default_size: (300.0, 200.0),
                label: Arc::new(|w| {
                    let cfg = parse_or_default::<ImageWidgetConfig>(&w.config);
                    SharedString::from(format!("Image: {}", display_or_unknown(&cfg.path)))
                }),
                build: Arc::new(build_image),
            },
        );
        self.register(
            WidgetKind::monitor(),
            WidgetSpec {
                default_size: (300.0, 160.0),
                label: Arc::new(|w| {
                    let cfg = parse_or_default::<MonitorWidgetConfig>(&w.config);
                    SharedString::from(format!("Monitor: {}", display_or_unknown(&cfg.component)))
                }),
                build: Arc::new(build_monitor),
            },
        );
        self.register(
            WidgetKind::viewer3d(),
            WidgetSpec {
                default_size: (480.0, 320.0),
                label: Arc::new(|w| SharedString::from(format!("3D Viewer #{}", w.id.0))),
                build: Arc::new(build_viewer3d),
            },
        );
        self.register(
            WidgetKind::traffic_light(),
            WidgetSpec {
                default_size: (120.0, 120.0),
                label: Arc::new(|w| {
                    let cfg = parse_or_default::<TrafficLightWidgetConfig>(&w.config);
                    SharedString::from(format!(
                        "Traffic Light: {}",
                        display_or_unknown(&cfg.component)
                    ))
                }),
                build: Arc::new(build_traffic_light),
            },
        );
        self.register(
            WidgetKind::traffic_light_grid(),
            WidgetSpec {
                default_size: (360.0, 200.0),
                label: Arc::new(|w| {
                    let cfg = parse_or_default::<TrafficLightGridWidgetConfig>(&w.config);
                    let label = if cfg.pattern.is_empty() {
                        "?".to_string()
                    } else {
                        cfg.pattern
                    };
                    SharedString::from(format!("Traffic Lights: {}", label))
                }),
                build: Arc::new(build_traffic_light_grid),
            },
        );
        self.register(
            WidgetKind::meter(),
            WidgetSpec {
                default_size: (90.0, 200.0),
                label: Arc::new(|w| {
                    let cfg = parse_or_default::<MeterConfig>(&w.config);
                    let name = cfg.label.unwrap_or(cfg.component);
                    SharedString::from(format!("Meter: {}", display_or_unknown(&name)))
                }),
                build: Arc::new(build_meter),
            },
        );
        self.register(
            WidgetKind::gauge(),
            WidgetSpec {
                default_size: (160.0, 140.0),
                label: Arc::new(|w| {
                    let cfg = parse_or_default::<GaugeConfig>(&w.config);
                    let name = cfg.label.unwrap_or(cfg.component);
                    SharedString::from(format!("Gauge: {}", display_or_unknown(&name)))
                }),
                build: Arc::new(build_gauge),
            },
        );
        self.register(
            WidgetKind::state_chip(),
            WidgetSpec {
                default_size: (150.0, 60.0),
                label: Arc::new(|w| {
                    let cfg = parse_or_default::<StateChipConfig>(&w.config);
                    let name = cfg.label.unwrap_or(cfg.component);
                    SharedString::from(format!("State: {}", display_or_unknown(&name)))
                }),
                build: Arc::new(build_state_chip),
            },
        );
        self.register(
            WidgetKind::sequence_control(),
            WidgetSpec {
                default_size: (260.0, 110.0),
                label: Arc::new(|w| {
                    let cfg = parse_or_default::<SequenceControlConfig>(&w.config);
                    SharedString::from(format!("Sequence: {}", display_or_unknown(&cfg.channel)))
                }),
                build: Arc::new(build_sequence_control),
            },
        );
        self.register(
            WidgetKind::attitude(),
            WidgetSpec {
                default_size: (220.0, 260.0),
                label: Arc::new(|w| {
                    let cfg = parse_or_default::<AttitudeConfig>(&w.config);
                    let name = cfg.label.unwrap_or(cfg.component);
                    SharedString::from(format!("Attitude: {}", display_or_unknown(&name)))
                }),
                build: Arc::new(build_attitude),
            },
        );
    }
}

/// Persisted shape of a text widget — the source component to display.
#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TextWidgetConfig {
    pub component: String,
}

/// Persisted shape of an image widget.
///
/// `data` carries the image itself, base64 (optionally as a `data:` URI), so
/// a target-shipped preset renders on a panel that cannot see the target's
/// filesystem. `path` is the local-authoring form.
#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ImageWidgetConfig {
    pub path: String,
    pub data: String,
}

/// Persisted shape of a monitor widget — the source component to monitor
/// plus the inspector-editable display fields.
///
/// `unit` and `show_sparkline` are `Option`: `None` means "never edited", so restore keeps the
/// constructor's metadata-derived defaults instead of clobbering them.
#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub struct MonitorWidgetConfig {
    pub component: String,
    pub unit: Option<String>,
    pub show_sparkline: Option<bool>,
}

/// Persisted shape of a traffic-light widget.
#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct TrafficLightWidgetConfig {
    pub component: String,
    pub color: Option<Hsla>,
}

/// Persisted shape of a traffic-light grid widget.
#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct TrafficLightGridWidgetConfig {
    pub pattern: String,
    pub color: Option<Hsla>,
}

/// Parse a widget's JSON blob into its expected config type, falling
/// back to `Default` on any parse error so labels and builders degrade
/// gracefully on a stale or hand-edited file.
fn parse_or_default<T: serde::de::DeserializeOwned + Default>(blob: &str) -> T {
    serde_json::from_str::<T>(blob).unwrap_or_default()
}

/// Render an empty string as `"?"` so labels stay legible when a widget
/// hasn't been configured yet.
fn display_or_unknown(s: &str) -> &str {
    if s.is_empty() { "?" } else { s }
}

/// Resolve `kind` to a spec. Unregistered kinds fall back to a placeholder
/// so a stale saved layout cannot crash the app.
pub(super) fn widget_spec(kind: &WidgetKind, cx: &App) -> Arc<WidgetSpec> {
    cx.global::<WidgetRegistry>()
        .spec(kind)
        .unwrap_or_else(|| placeholder_spec(kind))
}

/// Build a spec that draws a "? unknown kind" placeholder so removed or
/// renamed kinds degrade gracefully in older saved layouts.
fn placeholder_spec(kind: &WidgetKind) -> Arc<WidgetSpec> {
    let kind_name = kind.0.clone();
    Arc::new(WidgetSpec {
        default_size: (200.0, 80.0),
        label: {
            let kind_name = kind_name.clone();
            Arc::new(move |_w| SharedString::from(format!("? unknown kind: {}", kind_name)))
        },
        build: Arc::new(move |_config, _db, cx| {
            let label = SharedString::from(format!("? unknown kind: {}", kind_name));
            as_view_and_entity(cx.new(|_cx| PlaceholderWidget { label }))
        }),
    })
}

/// Snapshot a live widget's editable state into a fresh JSON blob.
///
/// Returns `None` for widget kinds whose persisted config never changes
/// after construction (text, image, table, viewer3d) — the cached blob on
/// `DashboardWidget.config` already reflects everything they own, so the
/// dashboard's `to_config` keeps it as-is.
///
/// Kinds with inspector-editable state (plot traces and overrides,
/// traffic-light colors, monitor unit/sparkline) re-serialize from the live
/// entity at save time. `config` is the widget's cached blob, used to carry
/// forward fields the entity doesn't own (e.g. the monitor's component
/// name, which the entity only holds as a resolved id).
pub fn serialize_widget_state(
    kind: &WidgetKind,
    entity: &gpui::AnyEntity,
    config: &str,
    cx: &App,
) -> Option<String> {
    if *kind == WidgetKind::plot() {
        let plot = entity.clone().downcast::<LinePlot>().ok()?;
        let lp = plot.read(cx);
        let (y_min_override, y_max_override) = lp
            .axes
            .first()
            .map(|a| {
                let a = a.read(cx);
                (a.y_min_override.clone(), a.y_max_override.clone())
            })
            .unwrap_or_default();
        let cfg = PlotPanelConfig {
            label: String::new(),
            traces: lp
                .traces()
                .iter()
                .map(|e| TraceConfig::from(e.read(cx)))
                .collect(),
            custom_title: lp.custom_title.as_ref().map(|s| s.to_string()),
            x_range: lp
                .x_range
                .as_custom()
                .map(|b| b.to_string())
                .unwrap_or_default(),
            y_min_override,
            y_max_override,
            axes: lp
                .axes
                .iter()
                .map(|a| crate::tiles::panels::YAxisConfig::from(a.read(cx)))
                .collect(),
            x_time_format: lp.x_time_format,
            cursors: Vec::new(),
            measurement_panel: Default::default(),
            hide_alarm_limits: !lp.show_alarm_limits,
            hide_alarm_color: !lp.show_alarm_color,
            event_overlays: lp
                .event_overlays
                .iter()
                .map(|o| {
                    let o = o.read(cx);
                    crate::tiles::panels::EventOverlayConfig {
                        kind: crate::plot_events::kind_key_to_string(o.key),
                        label: o.label.to_string(),
                        visible: o.visible,
                    }
                })
                .collect(),
        };
        return serde_json::to_string(&cfg).ok();
    }
    if *kind == WidgetKind::traffic_light() {
        let tl = entity.clone().downcast::<TrafficLight>().ok()?;
        let v = tl.read(cx);
        let cfg = TrafficLightWidgetConfig {
            component: v.name().to_string(),
            color: Some(v.color()),
        };
        return serde_json::to_string(&cfg).ok();
    }
    if *kind == WidgetKind::traffic_light_grid() {
        let g = entity.clone().downcast::<TrafficLightGrid>().ok()?;
        let v = g.read(cx);
        let cfg = TrafficLightGridWidgetConfig {
            pattern: v.pattern().to_string(),
            color: Some(v.color()),
        };
        return serde_json::to_string(&cfg).ok();
    }
    if *kind == WidgetKind::meter() {
        let m = entity.clone().downcast::<Meter>().ok()?;
        return serde_json::to_string(&m.read(cx).to_config()).ok();
    }
    if *kind == WidgetKind::gauge() {
        let g = entity.clone().downcast::<Gauge>().ok()?;
        return serde_json::to_string(&g.read(cx).to_config()).ok();
    }
    if *kind == WidgetKind::state_chip() {
        let c = entity.clone().downcast::<StateChip>().ok()?;
        return serde_json::to_string(&c.read(cx).to_config(cx)).ok();
    }
    if *kind == WidgetKind::sequence_control() {
        let s = entity.clone().downcast::<SequenceControl>().ok()?;
        return serde_json::to_string(&s.read(cx).to_config()).ok();
    }
    if *kind == WidgetKind::attitude() {
        let a = entity.clone().downcast::<AttitudeIndicator>().ok()?;
        return serde_json::to_string(&a.read(cx).to_config(cx)).ok();
    }
    if *kind == WidgetKind::monitor() {
        let m = entity.clone().downcast::<Monitor>().ok()?;
        let v = m.read(cx);
        let mut cfg = parse_or_default::<MonitorWidgetConfig>(config);
        cfg.unit = Some(v.unit.to_string());
        cfg.show_sparkline = Some(v.show_sparkline);
        return serde_json::to_string(&cfg).ok();
    }
    None
}

/// Build the rendered view and its inspectable entity for a stored widget.
pub(super) fn create_widget_view(
    kind: &WidgetKind,
    config: &str,
    db: &Arc<DB>,
    cx: &mut App,
) -> (AnyView, gpui::AnyEntity) {
    let spec = widget_spec(kind, cx);
    (spec.build)(config, db, cx)
}

fn as_view_and_entity<T: Render + 'static>(e: Entity<T>) -> (AnyView, gpui::AnyEntity) {
    let any = e.clone().into_any();
    (AnyView::from(e), any)
}

fn build_plot(config: &str, db: &Arc<DB>, cx: &mut App) -> (AnyView, gpui::AnyEntity) {
    // Only LinePlot has Facet adapters, so expose it — not the outer
    // TimeSeriesPlot — as the inspectable entity.
    let cfg = parse_or_default::<PlotPanelConfig>(config);
    let traces: Vec<Trace> = cfg.traces.into_iter().map(Trace::from).collect();
    let plot = cx.new(|cx| TimeSeriesPlot::new(db.clone(), traces, cx));
    let line_plot = plot.read(cx).line_plot().clone();
    line_plot.update(cx, |lp, cx| {
        lp.custom_title = cfg.custom_title.map(SharedString::from);
        lp.x_time_format = cfg.x_time_format;
        if cfg.axes.is_empty() {
            if let Some(axis) = lp.axes.first().cloned() {
                axis.update(cx, |a, _| {
                    a.y_min_override = cfg.y_min_override.clone();
                    a.y_max_override = cfg.y_max_override.clone();
                });
            }
        } else {
            lp.axes = cfg
                .axes
                .iter()
                .take(LinePlot::MAX_AXES)
                .map(|a| cx.new(|_| crate::views::time_series::YAxis::from(a.clone())))
                .collect();
        }
        cx.notify();
    });
    (AnyView::from(plot), line_plot.into_any())
}

fn build_text(config: &str, db: &Arc<DB>, cx: &mut App) -> (AnyView, gpui::AnyEntity) {
    let cfg = parse_or_default::<TextWidgetConfig>(config);
    if cfg.component.is_empty() {
        return as_view_and_entity(cx.new(|_cx| PlaceholderWidget {
            label: SharedString::new_static("?"),
        }));
    }
    let id = metor_proto::types::ComponentId::new(&cfg.component);
    as_view_and_entity(cx.new(|cx| ComponentText::new(db.clone(), id, cx)))
}

fn build_table(_config: &str, db: &Arc<DB>, cx: &mut App) -> (AnyView, gpui::AnyEntity) {
    as_view_and_entity(cx.new(|cx| new_component_table(db.clone(), cx)))
}

fn build_image(config: &str, _db: &Arc<DB>, cx: &mut App) -> (AnyView, gpui::AnyEntity) {
    let cfg = parse_or_default::<ImageWidgetConfig>(config);
    as_view_and_entity(cx.new(|_cx| ImageWidget::load(&cfg)))
}

fn build_monitor(config: &str, db: &Arc<DB>, cx: &mut App) -> (AnyView, gpui::AnyEntity) {
    let cfg = parse_or_default::<MonitorWidgetConfig>(config);
    if cfg.component.is_empty() {
        return as_view_and_entity(cx.new(|_cx| PlaceholderWidget {
            label: SharedString::new_static("?"),
        }));
    }
    let id = metor_proto::types::ComponentId::new(&cfg.component);
    let entity = cx.new(|cx| Monitor::new(db.clone(), id, cx));
    if cfg.unit.is_some() || cfg.show_sparkline.is_some() {
        entity.update(cx, |m, cx| {
            if let Some(unit) = cfg.unit {
                m.unit = SharedString::from(unit);
            }
            if let Some(show) = cfg.show_sparkline {
                m.show_sparkline = show;
            }
            cx.notify();
        });
    }
    as_view_and_entity(entity)
}

fn build_viewer3d(_config: &str, db: &Arc<DB>, cx: &mut App) -> (AnyView, gpui::AnyEntity) {
    as_view_and_entity(cx.new(|cx| Viewer3d::with_db(db.clone(), cx)))
}

fn build_traffic_light(config: &str, db: &Arc<DB>, cx: &mut App) -> (AnyView, gpui::AnyEntity) {
    let cfg = parse_or_default::<TrafficLightWidgetConfig>(config);
    if cfg.component.is_empty() {
        return as_view_and_entity(cx.new(|_cx| PlaceholderWidget {
            label: SharedString::new_static("?"),
        }));
    }
    let id = metor_proto::types::ComponentId::new(&cfg.component);
    let entity = cx.new(|cx| TrafficLight::new(db.clone(), id, cx));
    if let Some(color) = cfg.color {
        entity.update(cx, |t, cx| t.set_color(color, cx));
    }
    as_view_and_entity(entity)
}

fn build_meter(config: &str, db: &Arc<DB>, cx: &mut App) -> (AnyView, gpui::AnyEntity) {
    let cfg = parse_or_default::<MeterConfig>(config);
    if cfg.component.is_empty() {
        return as_view_and_entity(cx.new(|_cx| PlaceholderWidget {
            label: SharedString::new_static("?"),
        }));
    }
    as_view_and_entity(cx.new(|cx| Meter::from_config(&cfg, db.clone(), cx)))
}

fn build_gauge(config: &str, db: &Arc<DB>, cx: &mut App) -> (AnyView, gpui::AnyEntity) {
    let cfg = parse_or_default::<GaugeConfig>(config);
    if cfg.component.is_empty() {
        return as_view_and_entity(cx.new(|_cx| PlaceholderWidget {
            label: SharedString::new_static("?"),
        }));
    }
    as_view_and_entity(cx.new(|cx| Gauge::from_config(&cfg, db.clone(), cx)))
}

fn build_state_chip(config: &str, db: &Arc<DB>, cx: &mut App) -> (AnyView, gpui::AnyEntity) {
    let cfg = parse_or_default::<StateChipConfig>(config);
    if cfg.component.is_empty() {
        return as_view_and_entity(cx.new(|_cx| PlaceholderWidget {
            label: SharedString::new_static("?"),
        }));
    }
    as_view_and_entity(cx.new(|cx| StateChip::from_config(&cfg, db.clone(), cx)))
}

fn build_attitude(config: &str, db: &Arc<DB>, cx: &mut App) -> (AnyView, gpui::AnyEntity) {
    let cfg = parse_or_default::<AttitudeConfig>(config);
    if cfg.component.is_empty() {
        return as_view_and_entity(cx.new(|_cx| PlaceholderWidget {
            label: SharedString::new_static("?"),
        }));
    }
    as_view_and_entity(cx.new(|cx| AttitudeIndicator::from_config(&cfg, db.clone(), cx)))
}

fn build_sequence_control(config: &str, _db: &Arc<DB>, cx: &mut App) -> (AnyView, gpui::AnyEntity) {
    let cfg = parse_or_default::<SequenceControlConfig>(config);
    as_view_and_entity(cx.new(|cx| SequenceControl::from_config(&cfg, cx)))
}

fn build_traffic_light_grid(
    config: &str,
    db: &Arc<DB>,
    cx: &mut App,
) -> (AnyView, gpui::AnyEntity) {
    let cfg = parse_or_default::<TrafficLightGridWidgetConfig>(config);
    let pattern = SharedString::from(cfg.pattern);
    let entity = cx.new(|cx| TrafficLightGrid::new(db.clone(), pattern, cx));
    if let Some(color) = cfg.color {
        entity.update(cx, |g, cx| g.set_color(color, cx));
    }
    as_view_and_entity(entity)
}

/// Widget that decodes an image and paints it into its bounds.
struct ImageWidget {
    render_image: Option<Arc<RenderImage>>,
    label: SharedString,
}

impl ImageWidget {
    /// Inline bytes win over `path`.
    ///
    /// A target ships its presets over a link and the panel may be running on
    /// a machine that has never seen the target's filesystem, so a diagram
    /// referenced only by path would be a broken tile there. `path` remains
    /// for layouts a user authored locally against their own files.
    fn load(cfg: &ImageWidgetConfig) -> Self {
        let (bytes, source) = match decode_inline(&cfg.data) {
            Some(bytes) => (Some(bytes), SharedString::new_static("inline image")),
            None => (
                std::fs::read(&cfg.path).ok(),
                SharedString::from(cfg.path.clone()),
            ),
        };

        let render_image = bytes
            .and_then(|bytes| image::load_from_memory(&bytes).ok())
            .map(|img| {
                let rgba = img.to_rgba8();
                let (w, h) = rgba.dimensions();
                let buffer = ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(w, h, rgba.into_raw())
                    .expect("buffer size mismatch");
                let frames = SmallVec::from_elem(Frame::new(buffer), 1);
                Arc::new(RenderImage::new(frames))
            });

        let label = if render_image.is_some() {
            source
        } else {
            SharedString::from(format!("Failed to load: {}", source))
        };

        Self {
            render_image,
            label,
        }
    }
}

/// Decode an image widget's inline payload, accepting either raw base64 or a
/// `data:` URI so a preset author can paste either.
fn decode_inline(data: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    if data.is_empty() {
        return None;
    }
    let payload = match data.split_once("base64,") {
        Some((prefix, rest)) if prefix.starts_with("data:") => rest,
        _ => data,
    };
    base64::engine::general_purpose::STANDARD
        .decode(payload.trim())
        .ok()
}

impl Render for ImageWidget {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(img) = &self.render_image {
            let img = img.clone();
            div().size_full().child(
                canvas(
                    move |_, _, _| {},
                    move |bounds, _, window, _| {
                        let _ =
                            window.paint_image(bounds, Corners::default(), img.clone(), 0, false);
                    },
                )
                .size_full(),
            )
        } else {
            let theme = theme(cx);
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme.text_tertiary)
                .text_size(px(14.0))
                .child(self.label.clone())
        }
    }
}

/// Grey "missing content" tile used whenever a widget can't instantiate.
struct PlaceholderWidget {
    label: SharedString,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_image_data_accepts_raw_base64_and_data_uris() {
        // "hi" in base64.
        assert_eq!(decode_inline("aGk=").as_deref(), Some(&b"hi"[..]));
        assert_eq!(
            decode_inline("data:image/png;base64,aGk=").as_deref(),
            Some(&b"hi"[..])
        );
        // Whitespace from a wrapped literal in a preset file.
        assert_eq!(decode_inline(" aGk= \n").as_deref(), Some(&b"hi"[..]));
    }

    #[test]
    fn undecodable_inline_data_falls_through_to_the_path() {
        assert!(decode_inline("").is_none());
        assert!(decode_inline("not base64!!").is_none());
    }

    #[test]
    fn image_config_tolerates_pre_inline_blobs() {
        let cfg: ImageWidgetConfig = serde_json::from_str(r#"{"path":"/tmp/a.png"}"#).unwrap();
        assert_eq!(cfg.path, "/tmp/a.png");
        assert!(cfg.data.is_empty());
    }

    #[test]
    fn monitor_config_tolerates_pre_display_field_blobs() {
        let cfg: MonitorWidgetConfig = serde_json::from_str(r#"{"component":"a.b"}"#).unwrap();
        assert_eq!(cfg.component, "a.b");
        assert_eq!(cfg.unit, None);
        assert_eq!(cfg.show_sparkline, None);
    }

    #[test]
    fn monitor_config_round_trips_display_fields() {
        let cfg = MonitorWidgetConfig {
            component: "a.b".to_string(),
            unit: Some("V".to_string()),
            show_sparkline: Some(false),
        };
        let blob = serde_json::to_string(&cfg).unwrap();
        let back: MonitorWidgetConfig = serde_json::from_str(&blob).unwrap();
        assert_eq!(back.component, "a.b");
        assert_eq!(back.unit.as_deref(), Some("V"));
        assert_eq!(back.show_sparkline, Some(false));
    }
}

impl Render for PlaceholderWidget {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .text_color(theme.text_tertiary)
            .text_size(px(14.0))
            .child(self.label.clone())
    }
}
