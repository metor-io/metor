//! Widget-kind registry and the leaf renderers that back built-in kinds.
//!
//! [`WidgetRegistry`] is a gpui `Global` mapping [`WidgetKind`] to its
//! [`WidgetSpec`]. Downstream consumers can register or override specs at
//! startup via [`WidgetRegistry::register`].
use std::collections::HashMap;
use std::sync::Arc;

use gpui::{
    AnyView, App, Context, Corners, Entity, IntoElement, Render, RenderImage, SharedString, Window,
    canvas, div, prelude::*, px,
};
use image::{Frame, ImageBuffer, Rgba};
use metor_db::DB;
use smallvec::SmallVec;

use crate::views::viewer_3d::Viewer3d;
use crate::views::{ComponentText, Monitor, TimeSeriesPlot, new_component_table};
use crate::theme::theme;

use super::{DashboardWidget, WidgetKind};

/// Everything the dashboard needs to create and label one [`WidgetKind`].
///
/// Uses `Arc<dyn Fn>` so specs can capture owned state (shared resources,
/// remote handles) instead of being limited to plain function pointers.
pub struct WidgetSpec {
    pub default_size: (f32, f32),
    pub label: Arc<dyn Fn(&DashboardWidget) -> SharedString>,
    pub build:
        Arc<dyn Fn(&serde_json::Value, &Arc<DB>, &mut App) -> (AnyView, gpui::AnyEntity)>,
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
                    SharedString::from(format!("Text: {}", config_str(w, "component")))
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
                    SharedString::from(format!("Image: {}", config_str(w, "path")))
                }),
                build: Arc::new(build_image),
            },
        );
        self.register(
            WidgetKind::monitor(),
            WidgetSpec {
                default_size: (300.0, 160.0),
                label: Arc::new(|w| {
                    SharedString::from(format!("Monitor: {}", config_str(w, "component")))
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
    }
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

/// Build the rendered view and its inspectable entity for a stored widget.
pub(super) fn create_widget_view(
    kind: &WidgetKind,
    config: &serde_json::Value,
    db: &Arc<DB>,
    cx: &mut App,
) -> (AnyView, gpui::AnyEntity) {
    let spec = widget_spec(kind, cx);
    (spec.build)(config, db, cx)
}

fn config_str<'a>(widget: &'a DashboardWidget, key: &str) -> &'a str {
    widget
        .config
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("?")
}

fn as_view_and_entity<T: Render + 'static>(e: Entity<T>) -> (AnyView, gpui::AnyEntity) {
    let any = e.clone().into_any();
    (AnyView::from(e), any)
}

fn lookup_component(db: &Arc<DB>, name: &str) -> Option<metor_proto::types::ComponentId> {
    db.with_state(|state| {
        state
            .component_metadata_iter()
            .find(|(_, meta)| meta.name == name)
            .map(|(id, _)| *id)
    })
}

fn build_plot(
    _config: &serde_json::Value,
    db: &Arc<DB>,
    cx: &mut App,
) -> (AnyView, gpui::AnyEntity) {
    // Only LinePlot has Facet adapters, so expose it — not the outer
    // TimeSeriesPlot — as the inspectable entity.
    let plot = cx.new(|cx| TimeSeriesPlot::new(db.clone(), vec![], cx));
    let line_plot = plot.read(cx).line_plot().clone();
    (AnyView::from(plot), line_plot.into_any())
}

fn build_text(
    config: &serde_json::Value,
    db: &Arc<DB>,
    cx: &mut App,
) -> (AnyView, gpui::AnyEntity) {
    let component_name = config
        .get("component")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if let Some(id) = lookup_component(db, component_name) {
        as_view_and_entity(cx.new(|cx| ComponentText::new(db.clone(), id, cx)))
    } else {
        as_view_and_entity(cx.new(|_cx| PlaceholderWidget {
            label: SharedString::from(format!("? {}", component_name)),
        }))
    }
}

fn build_table(
    _config: &serde_json::Value,
    db: &Arc<DB>,
    cx: &mut App,
) -> (AnyView, gpui::AnyEntity) {
    as_view_and_entity(cx.new(|cx| new_component_table(db.clone(), cx)))
}

fn build_image(
    config: &serde_json::Value,
    _db: &Arc<DB>,
    cx: &mut App,
) -> (AnyView, gpui::AnyEntity) {
    let path = config
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    as_view_and_entity(cx.new(|_cx| ImageWidget::load(path)))
}

fn build_monitor(
    config: &serde_json::Value,
    db: &Arc<DB>,
    cx: &mut App,
) -> (AnyView, gpui::AnyEntity) {
    let component_name = config
        .get("component")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if let Some(id) = lookup_component(db, component_name) {
        as_view_and_entity(cx.new(|cx| Monitor::new(db.clone(), id, cx)))
    } else {
        as_view_and_entity(cx.new(|_cx| PlaceholderWidget {
            label: SharedString::from(format!("? {}", component_name)),
        }))
    }
}

fn build_viewer3d(
    _config: &serde_json::Value,
    db: &Arc<DB>,
    cx: &mut App,
) -> (AnyView, gpui::AnyEntity) {
    as_view_and_entity(cx.new(|cx| Viewer3d::with_db(db.clone(), cx)))
}

/// Widget that decodes an image from disk and paints it into its bounds.
struct ImageWidget {
    render_image: Option<Arc<RenderImage>>,
    label: SharedString,
}

impl ImageWidget {
    fn load(path: String) -> Self {
        let render_image = std::fs::read(&path)
            .ok()
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
            SharedString::from(path)
        } else {
            SharedString::from(format!("Failed to load: {}", path))
        };

        Self {
            render_image,
            label,
        }
    }
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
