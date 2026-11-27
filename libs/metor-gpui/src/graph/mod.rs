//! A pan-and-zoom view of a directed graph, laid out left to right in layers.
//!
//! [`GraphView`] is generic over what it draws: the app hands it a [`GraphModel`] of labelled
//! nodes and edges, and hears back through [`GraphEvent`]. Layout runs on the UI thread, since
//! the web has no threads, and only when the set of nodes, sizes or edges changes; a change of
//! state or label only repaints.

mod layout;

use std::hash::{DefaultHasher, Hash, Hasher};

use gpui::{
    App, Bounds, Context, EventEmitter, FocusHandle, Focusable, Hsla, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, PathBuilder, PinchEvent, Pixels, Point, Render,
    ScrollWheelEvent, SharedString, Size, Window, canvas, div, point, prelude::*, px, size,
};

pub use layout::{Layout, NodeSpec, Placement, Route, Spacing};

use crate::viewport::{Viewport, Zoomed, wheel_zoom_factor};
use crate::{CONTROL_RADIUS, StateTone, Theme, theme, ui_px};

/// Smallest zoom a graph reaches, so a 500-node graph still fits a window.
const MIN_ZOOM: f32 = 0.05;
/// Largest zoom a graph reaches.
const MAX_ZOOM: f32 = 2.0;
/// Length of an edge's arrowhead at zoom 1.
const ARROW: f32 = 7.0;

/// One node: what it shows and how much room it takes at zoom 1.
#[derive(Clone, Debug, PartialEq)]
pub struct GraphNode {
    /// The app's id for the node, unique within a model.
    pub id: u64,
    pub label: SharedString,
    /// The state dot's tone, or no dot.
    pub state: Option<StateTone>,
    /// Secondary text after the label, such as a duration.
    pub detail: Option<SharedString>,
    pub size: Size<Pixels>,
}

impl GraphNode {
    /// The size nodes take unless the app picks one.
    pub const DEFAULT_SIZE: Size<Pixels> = Size {
        width: px(180.0),
        height: px(32.0),
    };

    /// A node of [`DEFAULT_SIZE`](Self::DEFAULT_SIZE) with no state or detail.
    pub fn new(id: u64, label: impl Into<SharedString>) -> Self {
        Self {
            id,
            label: label.into(),
            state: None,
            detail: None,
            size: Self::DEFAULT_SIZE,
        }
    }
}

/// How an edge is stroked, so a graph can show two kinds of link.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum EdgeStyle {
    #[default]
    Solid,
    /// For a weaker link, such as parent to child.
    Dashed,
}

/// An edge from one node id to another.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct GraphEdge {
    pub from: u64,
    pub to: u64,
    pub style: EdgeStyle,
}

/// The nodes and edges a [`GraphView`] draws.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GraphModel {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
}

impl GraphModel {
    /// A hash of everything layout depends on: ids, sizes and edges, but not labels or states.
    fn structure_hash(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        for node in &self.nodes {
            node.id.hash(&mut hasher);
            f32::from(node.size.width).to_bits().hash(&mut hasher);
            f32::from(node.size.height).to_bits().hash(&mut hasher);
        }
        self.edges.len().hash(&mut hasher);
        for edge in &self.edges {
            (edge.from, edge.to).hash(&mut hasher);
        }
        hasher.finish()
    }

    fn layout(&self) -> Layout {
        let nodes: Vec<NodeSpec> = self
            .nodes
            .iter()
            .map(|node| NodeSpec {
                id: node.id,
                width: node.size.width.into(),
                height: node.size.height.into(),
            })
            .collect();
        let edges: Vec<(u64, u64)> = self.edges.iter().map(|e| (e.from, e.to)).collect();
        Layout::compute(&nodes, &edges, Spacing::default())
    }
}

/// What a [`GraphView`] tells its owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphEvent {
    /// A node was clicked, or `enter` was pressed on the selected one.
    NodeClicked(u64),
    NodeHovered(Option<u64>),
}

/// A graph laid out left to right, with drag to pan, wheel or pinch to zoom around the cursor,
/// and arrow keys to move the selection along edges.
///
/// It fits the whole graph into view the first time it is drawn, and again on [`fit`].
///
/// [`fit`]: GraphView::fit
pub struct GraphView {
    model: GraphModel,
    layout: Layout,
    structure: u64,
    viewport: Viewport,
    /// The view's bounds from the last paint, in window coordinates.
    bounds: Option<Bounds<Pixels>>,
    needs_fit: bool,
    selected: Option<u64>,
    hovered: Option<u64>,
    highlighted: Option<u64>,
    /// The pointer position of an ongoing pan.
    pan: Option<Point<Pixels>>,
    focus: FocusHandle,
}

impl EventEmitter<GraphEvent> for GraphView {}

impl Focusable for GraphView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl GraphView {
    /// A view of `model`, laid out now and fitted to the view when first drawn.
    pub fn new(model: GraphModel, cx: &mut Context<Self>) -> Self {
        Self {
            layout: model.layout(),
            structure: model.structure_hash(),
            model,
            viewport: Viewport::with_zoom_range(MIN_ZOOM, MAX_ZOOM),
            bounds: None,
            needs_fit: true,
            selected: None,
            hovered: None,
            highlighted: None,
            pan: None,
            focus: cx.focus_handle(),
        }
    }

    /// The model being drawn.
    pub fn model(&self) -> &GraphModel {
        &self.model
    }

    /// The layout of the current model, in unzoomed canvas units.
    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// Replaces the model. Layout reruns only when ids, sizes or edges changed.
    pub fn set_model(&mut self, model: GraphModel, cx: &mut Context<Self>) {
        let structure = model.structure_hash();
        if structure != self.structure {
            self.layout = model.layout();
            self.structure = structure;
        }
        self.model = model;
        let nodes = &self.model.nodes;
        let known = |id: Option<u64>| id.filter(|id| nodes.iter().any(|node| node.id == *id));
        let (selected, hovered, highlighted) = (
            known(self.selected),
            known(self.hovered),
            known(self.highlighted),
        );
        self.selected = selected;
        self.hovered = hovered;
        self.highlighted = highlighted;
        cx.notify();
    }

    /// The id of the selected node.
    pub fn selected(&self) -> Option<u64> {
        self.selected
    }

    /// Selects the node with `id`, or clears the selection. Emits no event.
    pub fn select(&mut self, id: Option<u64>, cx: &mut Context<Self>) {
        self.selected = id;
        cx.notify();
    }

    /// Marks one node, such as the anchor of a neighbourhood, with an accent border.
    pub fn highlight(&mut self, id: Option<u64>, cx: &mut Context<Self>) {
        self.highlighted = id;
        cx.notify();
    }

    /// Zooms and scrolls so the whole graph shows.
    pub fn fit(&mut self, cx: &mut Context<Self>) {
        self.needs_fit = true;
        cx.notify();
    }

    fn index_of(&self, id: u64) -> Option<usize> {
        self.model.nodes.iter().position(|node| node.id == id)
    }

    fn extent(&self) -> Point<f32> {
        point(self.layout.width, self.layout.height)
    }

    fn apply_fit(&mut self, view: Size<f32>) {
        let mut fitted = self.viewport.fitted(self.extent(), view);
        // Center the graph in whatever room the fit leaves.
        fitted.scroll = point(
            ((view.width - self.layout.width * fitted.zoom) / 2.0).max(0.0),
            ((view.height - self.layout.height * fitted.zoom) / 2.0).max(0.0),
        );
        self.viewport = fitted;
        self.needs_fit = false;
    }

    /// The node under `position`, in window coordinates.
    fn node_at(&self, position: Point<Pixels>) -> Option<u64> {
        let origin = self.bounds?.origin;
        hit_test(&self.layout, self.viewport, position - origin)
            .map(|index| self.model.nodes[index].id)
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus, cx);
        match self.node_at(event.position) {
            Some(id) => {
                self.selected = Some(id);
                cx.emit(GraphEvent::NodeClicked(id));
                cx.notify();
            }
            None => self.pan = Some(event.position),
        }
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(last) = self.pan {
            if event.pressed_button != Some(MouseButton::Left) {
                self.pan = None;
                return;
            }
            let delta = event.position - last;
            self.viewport.scroll.x += f32::from(delta.x);
            self.viewport.scroll.y += f32::from(delta.y);
            self.pan = Some(event.position);
            cx.notify();
            return;
        }
        let hovered = self.node_at(event.position);
        if hovered != self.hovered {
            self.hovered = hovered;
            cx.emit(GraphEvent::NodeHovered(hovered));
            cx.notify();
        }
    }

    fn zoom_at(&mut self, factor: f32, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(bounds) = self.bounds else {
            return;
        };
        let anchor = position - bounds.origin;
        self.viewport.zoom_around(
            self.viewport.zoom * factor,
            point(anchor.x.into(), anchor.y.into()),
        );
        cx.notify();
    }

    fn on_scroll(&mut self, event: &ScrollWheelEvent, window: &mut Window, cx: &mut Context<Self>) {
        let delta = event.delta.pixel_delta(window.line_height());
        self.zoom_at(wheel_zoom_factor(delta.y.into()), event.position, cx);
    }

    fn on_pinch(&mut self, event: &PinchEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.zoom_at(1.0 + event.delta, event.position, cx);
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        if key == "enter" {
            if let Some(id) = self.selected {
                cx.emit(GraphEvent::NodeClicked(id));
            }
            return;
        }
        let Some(direction) = Step::from_key(key) else {
            return;
        };
        let next = match self.selected.and_then(|id| self.index_of(id)) {
            Some(from) => {
                let ids: Vec<u64> = self.model.nodes.iter().map(|node| node.id).collect();
                step(&self.layout, &ids, from, direction)
            }
            None => (!self.model.nodes.is_empty()).then_some(0),
        };
        if let Some(next) = next {
            self.selected = Some(self.model.nodes[next].id);
            cx.stop_propagation();
            cx.notify();
        }
    }

    fn render_node(&self, index: usize, theme: &Theme) -> impl IntoElement + use<> {
        let node = &self.model.nodes[index];
        let placement = self.layout.nodes[index];
        let zoom = self.viewport.zoom;
        let at = self.viewport.to_screen(point(placement.x, placement.y));
        let selected = self.selected == Some(node.id);
        let border = if self.highlighted == Some(node.id) {
            theme.line_color
        } else if selected {
            theme.text_secondary
        } else {
            theme.border_primary
        };
        let bg = if selected || self.hovered == Some(node.id) {
            theme.selection_bg
        } else {
            theme.bg_elevated
        };
        div()
            .absolute()
            .left(px(at.x))
            .top(px(at.y))
            .w(px(placement.width * zoom))
            .h(px(placement.height * zoom))
            .flex()
            .flex_row()
            .items_center()
            .gap(ui_px(6.0))
            .px(ui_px(8.0))
            .rounded(ui_px(CONTROL_RADIUS))
            .border_1()
            .border_color(border)
            .bg(bg)
            .overflow_hidden()
            .text_size(ui_px(12.0))
            .text_color(theme.text_primary)
            .when_some(node.state, |row, tone| {
                row.child(
                    div()
                        .flex_none()
                        .size(ui_px(8.0))
                        .rounded_full()
                        .bg(theme.state_color(tone)),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(node.label.clone()),
            )
            .when_some(node.detail.clone(), |row, detail| {
                row.child(
                    div()
                        .flex_none()
                        .text_size(ui_px(11.0))
                        .text_color(theme.text_tertiary)
                        .child(detail),
                )
            })
    }

    /// Indices of the nodes whose rect is on screen.
    fn visible_nodes(&self) -> Vec<usize> {
        let Some(bounds) = self.bounds else {
            return (0..self.model.nodes.len()).collect();
        };
        let view = bounds.size;
        let zoom = self.viewport.zoom;
        (0..self.model.nodes.len())
            .filter(|&index| {
                let p = self.layout.nodes[index];
                let at = self.viewport.to_screen(point(p.x, p.y));
                at.x < f32::from(view.width)
                    && at.y < f32::from(view.height)
                    && at.x + p.width * zoom > 0.0
                    && at.y + p.height * zoom > 0.0
            })
            .collect()
    }

    /// Each route as it is painted, styled by its edge and by whether it touches the focus.
    fn strokes(&self, theme: &Theme) -> Vec<Stroke> {
        let styles: std::collections::HashMap<(u64, u64), EdgeStyle> = self
            .model
            .edges
            .iter()
            .map(|edge| ((edge.from, edge.to), edge.style))
            .collect();
        let focus = self.hovered.or(self.selected);
        self.layout
            .routes
            .iter()
            .map(|route| {
                let touches_focus = focus.is_some_and(|id| route.from == id || route.to == id);
                Stroke {
                    points: route
                        .points
                        .iter()
                        .map(|&(x, y)| self.viewport.to_screen(point(x, y)))
                        .collect(),
                    dashed: styles.get(&(route.from, route.to)) == Some(&EdgeStyle::Dashed),
                    color: if touches_focus {
                        theme.text_secondary
                    } else {
                        theme.axis_color
                    },
                }
            })
            .collect()
    }
}

/// One edge as it is painted: its polyline in view-local screen pixels.
struct Stroke {
    points: Vec<Point<f32>>,
    dashed: bool,
    color: Hsla,
}

/// The index of the node under `local`, a point relative to the view's origin.
pub fn hit_test(layout: &Layout, viewport: Viewport, local: Point<Pixels>) -> Option<usize> {
    let canvas = viewport.to_canvas(point(local.x.into(), local.y.into()));
    layout.node_at((canvas.x, canvas.y))
}

/// A direction the arrow keys move the selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// Along an outgoing edge.
    Forward,
    /// Along an incoming edge.
    Back,
    /// To the neighbour above in the same layer.
    Up,
    /// To the neighbour below in the same layer.
    Down,
}

impl Step {
    fn from_key(key: &str) -> Option<Self> {
        match key {
            "right" => Some(Self::Forward),
            "left" => Some(Self::Back),
            "up" => Some(Self::Up),
            "down" => Some(Self::Down),
            _ => None,
        }
    }
}

/// The index of the node an arrow key moves the selection to from node `from`. `ids` are the
/// node ids in layout order. Along edges it picks the neighbour nearest in height, so the
/// selection follows what the eye follows.
fn step(layout: &Layout, ids: &[u64], from: usize, direction: Step) -> Option<usize> {
    let here = layout.nodes[from];
    let center = |p: &Placement| p.y + p.height / 2.0;
    match direction {
        Step::Up | Step::Down => {
            let order = match direction {
                Step::Up => here.order.checked_sub(1)?,
                _ => here.order + 1,
            };
            layout
                .nodes
                .iter()
                .position(|p| p.layer == here.layer && p.order == order)
        }
        Step::Forward | Step::Back => {
            let neighbours = layout.routes.iter().filter_map(|route| {
                let (near, far) = match direction {
                    Step::Forward => (route.from, route.to),
                    _ => (route.to, route.from),
                };
                (near == ids[from]).then_some(far)
            });
            neighbours
                .filter_map(|id| ids.iter().position(|&candidate| candidate == id))
                .min_by(|&a, &b| {
                    let da = (center(&layout.nodes[a]) - center(&here)).abs();
                    let db = (center(&layout.nodes[b]) - center(&here)).abs();
                    da.total_cmp(&db)
                })
        }
    }
}

/// Paints `stroke` offset by `origin`: a smooth curve through its points, leaving and entering
/// each point horizontally, and an arrowhead at the end.
fn paint_stroke(stroke: &Stroke, origin: Point<Pixels>, zoom: f32, window: &mut Window) {
    let [first, .., last] = stroke.points.as_slice() else {
        return;
    };
    let at = |p: Point<f32>| origin + point(px(p.x), px(p.y));
    let width = px((1.5 * zoom).clamp(1.0, 2.0));
    let mut path = PathBuilder::stroke(width);
    if stroke.dashed {
        path = path.dash_array(&[px(4.0), px(3.0)]);
    }
    path.move_to(at(*first));
    for pair in stroke.points.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let reach = (b.x - a.x) / 2.0;
        path.cubic_bezier_to(
            at(b),
            at(point(a.x + reach, a.y)),
            at(point(b.x - reach, b.y)),
        );
    }
    if let Ok(path) = path.build() {
        window.paint_path(path, stroke.color);
    }
    let previous = stroke.points[stroke.points.len() - 2];
    let pointing = if last.x >= previous.x { 1.0 } else { -1.0 };
    let length = ARROW * zoom.clamp(0.5, 1.0);
    let mut head = PathBuilder::fill();
    head.add_polygon(
        &[
            at(*last),
            at(point(last.x - pointing * length, last.y - length / 2.0)),
            at(point(last.x - pointing * length, last.y + length / 2.0)),
        ],
        true,
    );
    if let Ok(head) = head.build() {
        window.paint_path(head, stroke.color);
    }
}

impl Render for GraphView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let strokes = self.strokes(&theme);
        let zoom = self.viewport.zoom;
        let nodes: Vec<_> = self
            .visible_nodes()
            .into_iter()
            .map(|index| self.render_node(index, &theme))
            .collect();
        let view = cx.entity().downgrade();
        div()
            .id("graph-view")
            .track_focus(&self.focus)
            .relative()
            .size_full()
            .overflow_hidden()
            .bg(theme.bg_secondary)
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.pan = None),
            )
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .on_pinch(cx.listener(Self::on_pinch))
            .on_key_down(cx.listener(Self::on_key_down))
            .child(
                canvas(
                    move |bounds, _, cx| {
                        view.update(cx, |view, cx| {
                            let size = size(bounds.size.width.into(), bounds.size.height.into());
                            let resized = view.bounds.map(|b| b.size) != Some(bounds.size);
                            view.bounds = Some(bounds);
                            if view.needs_fit && size.width > 0.0 {
                                view.apply_fit(size);
                                cx.notify();
                            } else if resized {
                                cx.notify();
                            }
                        })
                        .ok();
                        bounds.origin
                    },
                    move |_, origin, window, _| {
                        for stroke in &strokes {
                            paint_stroke(stroke, origin, zoom, window);
                        }
                    },
                )
                .absolute()
                .size_full(),
            )
            .child(Zoomed::new(
                zoom,
                div().absolute().size_full().children(nodes),
            ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chain() -> Layout {
        let nodes: Vec<NodeSpec> = [10, 20, 30, 40]
            .map(|id| NodeSpec {
                id,
                width: 100.0,
                height: 30.0,
            })
            .to_vec();
        // 10 -> 20 -> 40 and 10 -> 30 -> 40: two nodes share the middle layer.
        let edges = [(10, 20), (10, 30), (20, 40), (30, 40)];
        Layout::compute(&nodes, &edges, Spacing::default())
    }

    #[test]
    fn hit_testing_goes_through_the_viewport() {
        let layout = chain();
        let first = layout.nodes[0];
        let viewport = Viewport {
            zoom: 0.5,
            scroll: point(20.0, 10.0),
            ..Viewport::with_zoom_range(MIN_ZOOM, MAX_ZOOM)
        };
        let center = viewport.to_screen(point(
            first.x + first.width / 2.0,
            first.y + first.height / 2.0,
        ));
        assert_eq!(
            hit_test(&layout, viewport, point(px(center.x), px(center.y))),
            Some(0)
        );
        assert_eq!(hit_test(&layout, viewport, point(px(5.0), px(1.0))), None);
    }
}
