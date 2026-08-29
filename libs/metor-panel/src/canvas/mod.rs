//! One canvas for systems over frames, whatever wrote them.
//!
//! The panel used to keep three pictures of the same thing — the node editor's
//! dataflow, the system graph's wiring, and the program projection — and they
//! were the same picture all along: systems over frames, edges recovered from
//! names. This tile draws one graph from two sources. Native systems come from
//! the live target's wiring and are viewers, because their source of truth is
//! Rust and `target.py`. Python systems come from the source in this tile's
//! own text pane, and every one of them is editable.
//!
//! ## The text is the truth, and a gesture is an edit to it
//!
//! The two halves are two views of one artifact, never two states that have to
//! be kept in agreement — which is the failure mode every projectional editor
//! is remembered for. Dragging a card does not move a card: it rewrites that
//! declaration's `@node` annotation and reparses, and what comes back is where
//! the card now is. The canvas holds nothing the source cannot express, so
//! there is nothing to reconcile and nothing to fall out of sync.
//!
//! A drag is one edit, not one per frame: the pointer moves a *preview*
//! position and the source is rewritten once, on release. Native cards have no
//! source to rewrite, so their positions stay where they have always been — in
//! this tile's own view state.

pub mod edit;
pub mod legacy;
pub mod migrate;
pub mod model;
pub mod palette;
pub mod run;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    App, Axis, Bounds, Context, FocusHandle, Focusable, IntoElement, KeyDownEvent, MouseButton,
    Pixels, Point, Render, SharedString, Task, Window, anchored, canvas, deferred, div, point,
    prelude::*, px,
};
use metor_db::DB;
use metor_expr::Manifest;
use metor_fsw_2::ir::{EdgeKind, Wiring};

use crate::dynamic::ops::program::Compiled;
use crate::dynamic::resolver::DbResolver;
use crate::dynamic::worker::DynamicWorker;
use crate::graph_canvas::{RoutePoints, hit_test_edges, paint_grid, paint_route};
use crate::graph_layout::Direction;
use crate::inspector::rows::TextField;
use crate::theme::theme;
use crate::tiles::PaneItem;
use crate::views::system_graph::config::{SystemGraphConfig, Viewport};
use crate::views::system_graph::layout::GraphNodeKind;

use model::{CARD_WIDTH, Card, Edge, HEADER_HEIGHT, Model, Origin, Overrides, SOCKET_ROW_HEIGHT};
use run::Systems;

/// How long after a keystroke the source is compiled. Far more than a compile
/// takes; it is there so a half-typed line is not compiled thirty times.
const DEBOUNCE: Duration = Duration::from_millis(200);

struct NodeDrag {
    id: SharedString,
    pointer_origin: Point<Pixels>,
    node_origin: (f32, f32),
    moved: bool,
}

struct PanDrag {
    pointer_origin: Point<Pixels>,
    viewport_origin: Viewport,
}

pub struct GraphCanvas {
    db: Arc<DB>,
    focus_handle: FocusHandle,
    /// The program's source, which is the artifact both halves show.
    editor: TextField,
    /// What that source most recently compiled to. Kept across a failed
    /// compile so a half-typed line does not blank the canvas.
    manifest: Option<Manifest>,
    systems: Systems,
    diagnostics: Vec<(std::ops::Range<usize>, String)>,
    rebuild: Option<Task<()>>,

    viewport: Viewport,
    /// Manual positions for the half whose positions are not in the source.
    overrides: Overrides,
    collapsed: BTreeSet<String>,
    direction: Direction,
    selection: Option<SharedString>,
    selected_edge: Option<Edge>,
    node_drag: Option<NodeDrag>,
    /// Where a card is being dragged to, before the release that commits it.
    preview: Option<(SharedString, (f32, f32))>,
    /// The producer a wire is being dragged from, until it lands on a port.
    link: Option<SharedString>,
    /// Where the pointer is, for painting that wire.
    pointer: Point<Pixels>,
    /// Whether the add-a-declaration list is showing.
    palette: bool,
    pan_drag: Option<PanDrag>,
    canvas_origin: Option<Point<Pixels>>,
    /// Whether the text half is showing instead of the canvas.
    text: bool,
    /// The completion menu, while one is up over the editor.
    completion: Option<CompletionMenu>,
}

/// What the editor's completion popup shows: the ranked candidates for the
/// caret, and which one Enter would take.
struct CompletionMenu {
    completions: metor_expr::complete::Completions,
    selected: usize,
}

impl GraphCanvas {
    pub fn new(db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        Self::from_config(GraphCanvasConfig::default(), db, cx)
    }

    pub fn from_config(cfg: GraphCanvasConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        // Repaint whenever the manifest store folds a new topology.
        if let Some(store) = crate::wiring::try_global(cx) {
            cx.observe(&store, |_, _, cx| cx.notify()).detach();
        }
        let mut editor = TextField::new("adcs.omega_b * 100.0", cx).multiline();
        editor.set_text(cfg.program.source);
        let mut this = Self {
            db,
            focus_handle: cx.focus_handle(),
            editor,
            manifest: None,
            systems: Systems::default(),
            diagnostics: Vec::new(),
            rebuild: None,
            viewport: cfg.system.viewport.clone(),
            overrides: cfg.system.overrides_map(),
            collapsed: cfg.system.collapsed_set(),
            direction: cfg.system.direction,
            selection: None,
            selected_edge: None,
            node_drag: None,
            preview: None,
            link: None,
            pointer: point(px(0.0), px(0.0)),
            palette: false,
            pan_drag: None,
            canvas_origin: None,
            text: cfg.program.text,
            completion: None,
        };
        this.schedule_rebuild(cx);
        this
    }

    pub fn direction(&self) -> Direction {
        self.direction
    }

    pub fn set_direction(&mut self, direction: Direction, cx: &mut Context<Self>) {
        if self.direction != direction {
            self.direction = direction;
            cx.notify();
        }
    }

    fn flow_axis(&self) -> Axis {
        match self.direction {
            Direction::LeftRight => Axis::Horizontal,
            Direction::TopBottom => Axis::Vertical,
        }
    }

    fn wiring(&self, cx: &App) -> Option<Wiring> {
        crate::wiring::try_global(cx)?
            .read(cx)
            .state()
            .wiring()
            .cloned()
    }

    /// The graph as it stands, with any in-flight drag previewed.
    fn model(&self, cx: &App) -> Model {
        let mut model = model::build(
            self.manifest.as_ref(),
            self.wiring(cx).as_ref(),
            &self.collapsed,
            self.direction,
            &self.overrides,
        );
        if let Some((id, at)) = &self.preview
            && let Some(card) = model.cards.iter_mut().find(|c| c.id == *id)
        {
            card.pos = *at;
        }
        model
    }

    /// Cancel any pending compile and queue a fresh one after the debounce.
    fn schedule_rebuild(&mut self, cx: &mut Context<Self>) {
        self.rebuild.take();
        self.rebuild = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DEBOUNCE).await;
            let _ = this.update(cx, |this, cx| this.rebuild(cx));
        }));
    }

    /// Compile the source and reconcile what is running against it.
    ///
    /// A compile that fails leaves the previous declarations running and the
    /// canvas showing what they are. Half-typed source is the normal case in
    /// an editor, and tearing live systems down on every keystroke would make
    /// the tile unusable.
    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let resolver = DbResolver::snapshot(&self.db);
        let compiled = match Compiled::module(&self.editor.text, &resolver) {
            Ok(compiled) => Arc::new(compiled),
            Err(diags) => {
                self.diagnostics = diags
                    .iter()
                    .map(|d| {
                        (
                            d.span.start as usize..d.span.end as usize,
                            d.message.clone(),
                        )
                    })
                    .collect();
                self.paint_diagnostics(cx);
                cx.notify();
                return;
            }
        };
        self.diagnostics.clear();
        self.manifest = Some(compiled.manifest.clone());

        let worker = cx.global::<DynamicWorker>().handle().clone();
        for (span, why) in self
            .systems
            .reconcile(&compiled, &self.db, &resolver, &worker)
        {
            self.diagnostics
                .push((span.start as usize..span.end as usize, why));
        }
        self.paint_diagnostics(cx);
        cx.notify();
    }

    /// Underline every span the last compile complained about.
    fn paint_diagnostics(&mut self, cx: &App) {
        let color = theme(cx).error_accent;
        self.editor.marks = self
            .diagnostics
            .iter()
            .map(|(span, _)| (span.clone(), color))
            .collect();
    }

    /// Move one card, by whichever mechanism owns its position.
    ///
    /// This is the round trip the whole design rests on: a Python card's
    /// position is written into the source and read back out by the compiler,
    /// so what the canvas shows after a drag is what the file says — never a
    /// second copy of it.
    fn place(&mut self, id: &SharedString, at: (f32, f32), cx: &mut Context<Self>) {
        let layout = self.model(cx).card(id).and_then(|card| match &card.origin {
            Origin::Python { layout, .. } => Some(*layout),
            Origin::Native { .. } => None,
        });
        match layout {
            Some(layout) => {
                let source = layout.place(&self.editor.text, at.0, at.1);
                self.editor.set_text(source);
                self.schedule_rebuild(cx);
            }
            None => {
                self.overrides.insert(id.clone(), at);
            }
        }
        cx.notify();
    }

    fn toggle_collapsed(&mut self, path: &str, cx: &mut Context<Self>) {
        if !self.collapsed.remove(path) {
            self.collapsed.insert(path.to_string());
        }
        self.selection = None;
        self.selected_edge = None;
        cx.notify();
    }

    /// The declaration behind a card, when the card is one.
    fn declaration(&self, id: &SharedString, cx: &App) -> Option<metor_expr::Decl> {
        match self.model(cx).card(id)?.origin {
            Origin::Python { decl, .. } => Some(decl),
            Origin::Native { .. } => None,
        }
    }

    /// The selected card, if it is one this tile may edit.
    pub(crate) fn selected_declaration(
        &self,
        cx: &App,
    ) -> Option<(SharedString, metor_expr::Decl)> {
        let id = self.selection.clone()?;
        let decl = self.declaration(&id, cx)?;
        Some((id, decl))
    }

    /// Take an edited source, if the edit produced one.
    ///
    /// Every gesture funnels through here, which is why there is exactly one
    /// place that decides what happens after one: the text becomes the new
    /// truth and everything else is re-derived from it.
    fn apply(&mut self, edited: Option<String>, cx: &mut Context<Self>) {
        let Some(source) = edited else { return };
        self.editor.set_text(source);
        self.schedule_rebuild(cx);
        cx.notify();
    }

    /// Point one of a card's ports at a different producer.
    fn connect(&mut self, to: &SharedString, port: usize, cx: &mut Context<Self>) {
        let Some(from) = self.link.take() else { return };
        let (Some(consumer), Some(manifest)) = (self.declaration(to, cx), self.manifest.clone())
        else {
            return;
        };
        let edited = edit::connect(&manifest, &self.editor.text, consumer, port, &from);
        self.apply(edited, cx);
    }

    pub(crate) fn rename_selected(&mut self, to: &str, cx: &mut Context<Self>) {
        let (Some((_, decl)), Some(manifest)) =
            (self.selected_declaration(cx), self.manifest.clone())
        else {
            return;
        };
        let edited = edit::rename(&manifest, &self.editor.text, decl, to);
        if edited.is_some() {
            self.selection = Some(SharedString::from(to.to_string()));
        }
        self.apply(edited, cx);
    }

    fn delete_selected(&mut self, cx: &mut Context<Self>) {
        let (Some((_, decl)), Some(manifest)) =
            (self.selected_declaration(cx), self.manifest.clone())
        else {
            return;
        };
        let edited = edit::delete(&manifest, &self.editor.text, decl);
        self.selection = None;
        self.apply(edited, cx);
    }

    /// Add a declaration from the palette, and select what it made.
    fn add(&mut self, entry: &palette::Entry, cx: &mut Context<Self>) {
        let Some(manifest) = self.manifest.clone() else {
            return;
        };
        let (source, name) =
            edit::insert(&manifest, &self.editor.text, entry.stem, &entry.template);
        self.palette = false;
        self.selection = Some(SharedString::from(name));
        self.apply(Some(source), cx);
    }

    /// Forget every hand-placed native position. Python positions live in the
    /// source, so this deliberately leaves them alone — re-laying those out is
    /// an edit to the file, not a view reset.
    pub(crate) fn relayout(&mut self, cx: &mut Context<Self>) {
        self.overrides.clear();
        cx.notify();
    }

    fn open_node_inspector(
        &self,
        id: SharedString,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let proxy =
            cx.new(|_| crate::views::system_graph::inspector_rows::SelectedGraphNode { id });
        window.dispatch_action(
            Box::new(crate::inspector::InspectEntity {
                entity: proxy.into_any(),
                position,
            }),
            cx,
        );
    }

    fn on_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        // The canvas is a view of the text, so one key turns it over rather
        // than the two being separate places to be.
        let mods = &event.keystroke.modifiers;
        let primary = if cfg!(target_os = "macos") {
            mods.platform
        } else {
            mods.control
        };
        if primary && event.keystroke.key.as_str() == "g" {
            self.text = !self.text;
            self.completion = None;
            cx.notify();
            return;
        }
        if !self.text {
            match event.keystroke.key.as_str() {
                "delete" | "backspace" => self.delete_selected(cx),
                "escape" if self.palette => {
                    self.palette = false;
                    cx.notify();
                }
                _ => {}
            }
            return;
        }

        let key = event.keystroke.key.as_str();
        // While the menu is up, the navigation keys are its keys; everything
        // else falls through to the editor and re-queries.
        if let Some(menu) = &mut self.completion {
            match key {
                "escape" => {
                    self.completion = None;
                    cx.notify();
                    return;
                }
                "up" => {
                    menu.selected = menu.selected.saturating_sub(1);
                    cx.notify();
                    return;
                }
                "down" => {
                    menu.selected = (menu.selected + 1).min(menu.completions.items.len() - 1);
                    cx.notify();
                    return;
                }
                "enter" | "return" | "tab" if !mods.shift => {
                    self.accept_completion(cx);
                    return;
                }
                _ => {}
            }
        }
        if mods.control && key == "space" {
            self.refresh_completion(true);
            cx.notify();
            return;
        }
        if self.editor.handle_key_down(event, cx) {
            self.editor.follow_cursor();
            self.schedule_rebuild(cx);
            self.refresh_completion(false);
            cx.notify();
        }
    }

    /// Recompute what the caret could take, opening or closing the menu.
    ///
    /// The menu opens itself only while a prefix is being typed; an empty
    /// position offers everything, but only when asked (ctrl-space), so the
    /// popup never chases the caret through plain navigation.
    fn refresh_completion(&mut self, explicit: bool) {
        let resolver = crate::inspector::completion::resolver(&self.db);
        let mut completions = metor_expr::complete::complete(
            &self.editor.text,
            self.editor.cursor as u32,
            metor_expr::complete::Scope::Module,
            resolver.as_ref(),
            self.manifest.as_ref(),
        );
        crate::inspector::completion::rank(&mut completions);
        let open = (explicit || !completions.prefix.is_empty()) && !completions.items.is_empty();
        self.completion = open.then_some(CompletionMenu {
            completions,
            selected: 0,
        });
    }

    /// Splice the selected candidate over its replace range and recompile.
    fn accept_completion(&mut self, cx: &mut Context<Self>) {
        let Some(menu) = self.completion.take() else {
            return;
        };
        let Some(item) = menu.completions.items.get(menu.selected) else {
            return;
        };
        let (start, end) = (
            menu.completions.replace.start as usize,
            menu.completions.replace.end as usize,
        );
        let mut text = self.editor.text.clone();
        text.replace_range(start..end, &item.insert);
        self.editor.set_text(text);
        let caret = start + item.caret.map(|c| c as usize).unwrap_or(item.insert.len());
        self.editor.cursor = caret;
        self.editor.mark = caret;
        self.editor.follow_cursor();
        self.schedule_rebuild(cx);
        cx.notify();
    }
}

impl Focusable for GraphCanvas {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for GraphCanvas {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let body = match self.text {
            true => {
                let mut editor = div()
                    .flex_1()
                    .p_2()
                    .overflow_hidden()
                    .text_size(px(12.0))
                    .child(self.editor.lines_element());
                if let Some(popup) = self.render_completion(window, cx) {
                    editor = editor.child(popup);
                }
                editor.into_any_element()
            }
            false => self.render_canvas(cx).into_any_element(),
        };
        div()
            // `TextInput` is what keeps single-key shortcuts — the leader
            // above all — out of the editor. Declared only while the editor is
            // showing: on the canvas there is nothing typing into, so the
            // shortcuts should work as they do anywhere.
            .key_context(match self.text {
                true => "GraphCanvas TextInput",
                false => "GraphCanvas",
            })
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _w, cx| this.on_key(event, cx)))
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.bg_primary)
            .child(body)
            .child(self.render_status(&theme))
    }
}

impl GraphCanvas {
    /// The completion popup, hung from the caret's last painted position.
    ///
    /// A short window onto the ranked list: the top candidates are already
    /// the answer, and a taller panel would only cover the code being
    /// written. Clicking a row accepts it exactly as Enter does.
    fn render_completion(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        const VISIBLE: usize = 8;
        let menu = self.completion.as_ref()?;
        let theme = theme(cx);
        let mut list = div()
            .flex()
            .flex_col()
            .py(px(2.0))
            .w(px(320.0))
            .bg(theme.bg_elevated)
            .border_1()
            .border_color(theme.border_primary)
            .rounded(px(4.0))
            .shadow_sm();
        // Keep the selection on screen: slide the window, not the caret.
        let first = menu.selected.saturating_sub(VISIBLE - 1);
        for (ix, item) in menu
            .completions
            .items
            .iter()
            .enumerate()
            .skip(first)
            .take(VISIBLE)
        {
            let selected = ix == menu.selected;
            let row = div()
                .id(("completion-row", ix))
                .px(px(8.0))
                .py(px(2.0))
                .when(selected, |d| d.bg(theme.selection_bg))
                .hover(|d| d.bg(theme.selection_bg))
                .cursor_pointer()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _window, cx| {
                        if let Some(menu) = &mut this.completion {
                            menu.selected = ix;
                        }
                        this.accept_completion(cx);
                        cx.stop_propagation();
                    }),
                )
                .child(crate::inspector::completion::candidate_content(
                    item, None, window, cx,
                ));
            list = list.child(row);
        }
        Some(
            deferred(
                anchored()
                    .position(self.editor.caret_position())
                    .snap_to_window_with_margin(px(8.0))
                    .child(list),
            )
            .with_priority(1),
        )
    }

    fn render_canvas(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let model = self.model(cx);
        let viewport = self.viewport.clone();
        let axis = self.flow_axis();
        let selected_edge = self.selected_edge.clone();

        let local = |p: (f32, f32)| point(px(p.0 - viewport.x), px(p.1 - viewport.y));
        let edges: Vec<(Edge, RoutePoints, gpui::Hsla, bool)> = model
            .edges
            .iter()
            .filter_map(|edge| {
                let route: RoutePoints = match &edge.route {
                    Some(r) => std::iter::once(local(r.source))
                        .chain(r.waypoints.iter().map(|&w| local(w)))
                        .chain(std::iter::once(local(r.target)))
                        .collect(),
                    None => {
                        let (from, to) = (model.card(&edge.from)?, model.card(&edge.to)?);
                        let (source, target) = match self.direction {
                            Direction::LeftRight => (
                                (from.pos.0 + CARD_WIDTH, from.pos.1 + from.height / 2.0),
                                (to.pos.0, to.pos.1 + to.height / 2.0),
                            ),
                            Direction::TopBottom => (
                                (from.pos.0 + CARD_WIDTH / 2.0, from.pos.1 + from.height),
                                (to.pos.0 + CARD_WIDTH / 2.0, to.pos.1),
                            ),
                        };
                        [local(source), local(target)].into_iter().collect()
                    }
                };
                let mut color = match edge.kind {
                    EdgeKind::Frame => theme.frame_edge_color(),
                    EdgeKind::Msg => theme.msg_edge_color(),
                };
                if selected_edge.as_ref() == Some(edge) {
                    color = theme.line_colors[0];
                }
                Some((edge.clone(), route, color, edge.delayed))
            })
            .collect();

        // The wire being dragged, if one is: from the producer's outgoing
        // side to wherever the pointer is. It is painted, never modelled —
        // there is no half-made edge in the graph.
        let in_flight = self.link.as_ref().and_then(|from| {
            let card = model.card(from)?;
            let source = match self.direction {
                Direction::LeftRight => (card.pos.0 + CARD_WIDTH, card.pos.1 + card.height / 2.0),
                Direction::TopBottom => (card.pos.0 + CARD_WIDTH / 2.0, card.pos.1 + card.height),
            };
            let route: RoutePoints = [local(source), self.pointer].into_iter().collect();
            Some((route, theme.line_colors[0]))
        });

        let theme_for_paint = theme.clone();
        let edges_for_paint = edges.clone();
        let canvas_layer = canvas(
            {
                let weak = cx.entity().downgrade();
                move |bounds: Bounds<Pixels>, _window, cx| {
                    let _ = weak.update(cx, |this, _| this.canvas_origin = Some(bounds.origin));
                    bounds
                }
            },
            move |_, bounds, window, _cx| {
                paint_grid(bounds, theme_for_paint.grid_color, window);
                for (_e, route, color, dashed) in &edges_for_paint {
                    paint_route(bounds.origin, route, axis, *color, *dashed, window);
                }
                if let Some((route, color)) = &in_flight {
                    paint_route(bounds.origin, route, axis, *color, true, window);
                }
            },
        )
        .absolute()
        .inset_0();

        let geometry: Arc<Vec<(Edge, RoutePoints)>> = Arc::new(
            edges
                .iter()
                .map(|(e, route, _, _)| (e.clone(), route.clone()))
                .collect(),
        );

        let mut root = div()
            .relative()
            .flex_1()
            .overflow_hidden()
            .bg(theme.bg_primary)
            .on_mouse_down(MouseButton::Left, {
                let edges = geometry.clone();
                cx.listener(move |this, ev: &gpui::MouseDownEvent, _w, cx| {
                    let Some(origin) = this.canvas_origin else {
                        return;
                    };
                    if let Some(edge) =
                        hit_test_edges(&edges, ev.position - origin, this.flow_axis())
                    {
                        this.selected_edge = Some(edge);
                        this.selection = None;
                        cx.notify();
                        return;
                    }
                    if this.selection.is_some() || this.selected_edge.is_some() {
                        this.selection = None;
                        this.selected_edge = None;
                        cx.notify();
                    }
                })
            })
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(|this, ev: &gpui::MouseDownEvent, _w, _cx| {
                    this.pan_drag = Some(PanDrag {
                        pointer_origin: ev.position,
                        viewport_origin: this.viewport.clone(),
                    });
                }),
            )
            .on_mouse_move(cx.listener(|this, ev: &gpui::MouseMoveEvent, _w, cx| {
                if let Some(origin) = this.canvas_origin {
                    this.pointer = ev.position - origin;
                    if this.link.is_some() {
                        cx.notify();
                    }
                }
                if let Some(pan) = &this.pan_drag {
                    let dx = f32::from(ev.position.x - pan.pointer_origin.x);
                    let dy = f32::from(ev.position.y - pan.pointer_origin.y);
                    this.viewport.x = pan.viewport_origin.x - dx;
                    this.viewport.y = pan.viewport_origin.y - dy;
                    cx.notify();
                    return;
                }
                if let Some(drag) = &mut this.node_drag {
                    let dx = f32::from(ev.position.x - drag.pointer_origin.x);
                    let dy = f32::from(ev.position.y - drag.pointer_origin.y);
                    if dx.abs() > 2.0 || dy.abs() > 2.0 {
                        drag.moved = true;
                    }
                    let at = (drag.node_origin.0 + dx, drag.node_origin.1 + dy);
                    this.preview = Some((drag.id.clone(), at));
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, ev: &gpui::MouseUpEvent, window, cx| {
                    // A wire released on empty canvas simply does not land;
                    // there is no half-made edge to leave behind.
                    if this.link.take().is_some() {
                        cx.notify();
                    }
                    let Some(drag) = this.node_drag.take() else {
                        return;
                    };
                    match this.preview.take() {
                        // The release is the edit: one rewrite per gesture.
                        Some((id, at)) if drag.moved => this.place(&id, at, cx),
                        _ => this.open_node_inspector(drag.id, ev.position, window, cx),
                    }
                }),
            )
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(|this, _ev: &gpui::MouseUpEvent, _w, cx| {
                    if this.pan_drag.take().is_some() {
                        cx.notify();
                    }
                }),
            )
            .on_scroll_wheel(cx.listener(|this, event: &gpui::ScrollWheelEvent, _w, cx| {
                let delta = event.delta.pixel_delta(px(20.0));
                this.viewport.x -= f32::from(delta.x);
                this.viewport.y -= f32::from(delta.y);
                cx.notify();
            }));

        root = root.child(canvas_layer);
        for card in &model.cards {
            root = root.child(self.render_card(card, &theme, cx));
        }
        root = root.child(self.render_toolbar(&model, &theme, cx));
        if self.palette {
            root = root.child(self.render_palette(&theme, cx));
        }
        root
    }

    fn render_card(
        &self,
        card: &Card,
        theme: &crate::theme::Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let id = card.id.clone();
        let selected = self.selection.as_ref() == Some(&id);

        let (accent, tag) = match &card.origin {
            Origin::Python { decl, .. } => match decl {
                metor_expr::Decl::System(_) => (theme.line_colors[2], "python"),
                metor_expr::Decl::Stage(_) => (theme.line_colors[3], "resample"),
            },
            Origin::Native { kind, .. } => match kind {
                GraphNodeKind::System => (theme.border_primary, "system"),
                GraphNodeKind::Slot => (theme.line_colors[5], "slot"),
                GraphNodeKind::Coordinator => (theme.line_colors[6], "coordinator"),
                GraphNodeKind::ScopeGroup => (theme.line_colors[4], "scope"),
            },
        };
        let border = match selected {
            true => theme.line_colors[0],
            false => accent,
        };

        let title = match &card.origin {
            Origin::Native {
                kind: GraphNodeKind::ScopeGroup,
                ..
            } => scope_leaf(&id),
            _ => id.clone(),
        };
        let mut element = div()
            .absolute()
            .left(px(card.pos.0 - self.viewport.x))
            .top(px(card.pos.1 - self.viewport.y))
            .w(px(CARD_WIDTH))
            .h(px(card.height))
            .bg(theme.bg_elevated)
            .border_1()
            .border_color(border)
            .rounded_md()
            .overflow_hidden()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .px_2()
                    .h(px(HEADER_HEIGHT))
                    .border_b_1()
                    .border_color(theme.border_primary)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_size(px(11.0))
                            .text_color(theme.text_primary)
                            .child(title),
                    )
                    .child(
                        div()
                            .text_size(px(9.0))
                            .text_color(theme.text_tertiary)
                            .child(SharedString::new_static(tag)),
                    ),
            );

        element = match card.origin.is_python() {
            true => element.child(self.sockets(card, theme, cx)),
            false => element.child(
                div()
                    .flex()
                    .flex_col()
                    .px_2()
                    .py_1()
                    .text_size(px(10.0))
                    .text_color(theme.text_secondary)
                    .child(card.subtitle.clone()),
            ),
        };

        // A group card toggles collapse; everything else selects and drags,
        // and a release that never moved opens the inspector.
        match &card.origin {
            Origin::Native {
                kind: GraphNodeKind::ScopeGroup,
                ..
            } => {
                let path = id.to_string();
                element.on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _ev: &gpui::MouseDownEvent, _w, cx| {
                        this.toggle_collapsed(&path, cx);
                        cx.stop_propagation();
                    }),
                )
            }
            _ => {
                let origin = card.pos;
                element.on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, ev: &gpui::MouseDownEvent, _w, cx| {
                        this.selection = Some(id.clone());
                        this.selected_edge = None;
                        this.node_drag = Some(NodeDrag {
                            id: id.clone(),
                            pointer_origin: ev.position,
                            node_origin: origin,
                            moved: false,
                        });
                        cx.stop_propagation();
                        cx.notify();
                    }),
                )
            }
        }
    }

    fn render_toolbar(
        &self,
        model: &Model,
        theme: &crate::theme::Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let chip = |text: SharedString| {
            div()
                .px_2()
                .py_0p5()
                .rounded_md()
                .bg(theme.bg_elevated)
                .border_1()
                .border_color(theme.border_primary)
                .text_size(px(10.0))
                .child(text)
        };
        div()
            .absolute()
            .left_2()
            .top_2()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(
                chip(SharedString::from(format!(
                    "{} nodes · {} edges",
                    model.cards.len(),
                    model.edges.len()
                )))
                .text_color(theme.text_secondary),
            )
            .child(
                chip(SharedString::new_static("Re-layout"))
                    .text_color(theme.text_primary)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _ev: &gpui::MouseDownEvent, _w, cx| {
                            this.relayout(cx);
                            cx.stop_propagation();
                        }),
                    ),
            )
            .child(
                chip(SharedString::new_static(match self.direction {
                    Direction::LeftRight => "Flow →",
                    Direction::TopBottom => "Flow ↓",
                }))
                .text_color(theme.text_primary)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _ev: &gpui::MouseDownEvent, _w, cx| {
                        this.set_direction(this.direction.cycle(), cx);
                        cx.stop_propagation();
                    }),
                ),
            )
            .child(
                chip(SharedString::new_static("Add"))
                    .text_color(theme.text_primary)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _ev: &gpui::MouseDownEvent, _w, cx| {
                            this.palette = !this.palette;
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    ),
            )
            .child(
                chip(SharedString::new_static("Text"))
                    .text_color(theme.text_primary)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _ev: &gpui::MouseDownEvent, _w, cx| {
                            this.text = true;
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    ),
            )
    }

    /// The strip under both halves: what each declaration is doing, or why the
    /// source did not compile.
    fn render_status(&self, theme: &crate::theme::Theme) -> impl IntoElement {
        let mut strip = div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .px_2()
            .py_1()
            .border_t_1()
            .border_color(theme.border_primary)
            .bg(theme.bg_secondary)
            .text_size(px(11.0));

        for (span, message) in &self.diagnostics {
            let line = self.editor.text[..span.start.min(self.editor.text.len())]
                .bytes()
                .filter(|b| *b == b'\n')
                .count()
                + 1;
            strip = strip.child(
                div()
                    .text_color(theme.error_accent)
                    .child(SharedString::from(format!("{line}: {message}"))),
            );
        }

        for running in self.systems.iter() {
            let (color, detail) = match running.health.fault() {
                Some(why) => (theme.error_accent, why),
                None => (theme.text_secondary, running.publishes.join(", ")),
            };
            strip = strip.child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        div()
                            .text_color(theme.text_primary)
                            .child(SharedString::from(running.name.clone())),
                    )
                    .child(div().text_color(color).child(SharedString::from(detail))),
            );
        }
        strip
    }
}

impl GraphCanvas {
    /// A Python card's body: one row per socket, inputs then outputs, each
    /// naming what it carries.
    ///
    /// The dots are the connect gesture. Pressing an output dot starts a wire
    /// and releasing on an input dot lands it — which is one rewrite of the
    /// consumer's binding, because an edge has no existence apart from the two
    /// names at its ends.
    fn sockets(
        &self,
        card: &Card,
        theme: &crate::theme::Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut body = div().flex().flex_col().px_2().py_1();
        let linking = self.link.is_some();
        for (index, socket, output) in card
            .inputs
            .iter()
            .enumerate()
            .map(|(i, s)| (i, s, false))
            .chain(card.outputs.iter().enumerate().map(|(i, s)| (i, s, true)))
        {
            let mut dot = div()
                .w(px(8.0))
                .h(px(8.0))
                .rounded_full()
                .border_1()
                .border_color(theme.bg_elevated)
                .bg(match (output, linking) {
                    (true, _) => theme.line_colors[2],
                    // While a wire is in flight every input is a target, and
                    // saying so is the whole of the affordance.
                    (false, true) => theme.line_colors[0],
                    (false, false) => theme.text_tertiary,
                });
            let id = card.id.clone();
            dot = match output {
                true => dot.on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _ev: &gpui::MouseDownEvent, _w, cx| {
                        this.link = Some(id.clone());
                        cx.stop_propagation();
                        cx.notify();
                    }),
                ),
                false => dot.on_mouse_up(
                    MouseButton::Left,
                    cx.listener(move |this, _ev: &gpui::MouseUpEvent, _w, cx| {
                        this.connect(&id, index, cx);
                        cx.stop_propagation();
                    }),
                ),
            };

            let name = div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_size(px(10.0))
                .text_color(theme.text_primary)
                .child(SharedString::from(socket.name.clone()));
            let detail = div()
                .text_size(px(9.0))
                .text_color(theme.text_tertiary)
                .child(SharedString::from(socket.detail.clone()));
            let row = div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1p5()
                .h(px(SOCKET_ROW_HEIGHT));
            body = body.child(match output {
                true => row.child(name).child(detail).child(dot),
                false => row.child(dot).child(name).child(detail),
            });
        }
        body
    }

    /// The add-a-declaration list.
    ///
    /// Hugging the toolbar rather than filling the canvas, because it is a
    /// short list of short labels and a modal would be a bigger gesture than
    /// the one being made.
    fn render_palette(
        &self,
        theme: &crate::theme::Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selected = self.selection.clone();
        let entries = palette::entries(
            self.manifest.as_ref(),
            selected.as_ref().map(|s| s.as_ref()),
        );
        let mut list = div()
            .absolute()
            .left_2()
            .top(px(36.0))
            .flex()
            .flex_col()
            .w(px(220.0))
            .rounded_md()
            .bg(theme.bg_elevated)
            .border_1()
            .border_color(theme.border_primary)
            .overflow_hidden();
        for entry in entries {
            let label = SharedString::from(entry.label.clone());
            let detail = SharedString::new_static(entry.detail);
            list = list.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .px_2()
                    .py_0p5()
                    .text_size(px(10.0))
                    .hover(|s| s.bg(theme.bg_secondary))
                    .child(div().text_color(theme.text_primary).child(label))
                    .child(div().text_color(theme.text_tertiary).child(detail))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _ev: &gpui::MouseDownEvent, _w, cx| {
                            this.add(&entry, cx);
                            cx.stop_propagation();
                        }),
                    ),
            );
        }
        list
    }
}

/// The last dotted segment of a scope path, for the group card's header.
fn scope_leaf(path: &str) -> SharedString {
    match path.rsplit_once('.') {
        Some((_, leaf)) => SharedString::from(leaf.to_string()),
        None => SharedString::from(path.to_string()),
    }
}

/// The tile's persisted state: the source, and the view over it.
///
/// The view half is exactly the system graph's, flattened, which is what lets
/// a layout saved before this tile existed open unchanged.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct GraphCanvasConfig {
    #[serde(flatten)]
    pub system: SystemGraphConfig,
    #[serde(flatten)]
    pub program: ProgramState,
}

/// The half of the tile's state that is about the program rather than the view.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ProgramState {
    pub source: String,
    /// Whether the tile opens on the text rather than the canvas.
    pub text: bool,
}

impl PaneItem for GraphCanvas {
    type Config = GraphCanvasConfig;

    fn tab_title(&self, _cx: &App) -> SharedString {
        "Graph".into()
    }

    /// The system graph's key, because this tile *is* the system graph with a
    /// second source added — so every layout that already names it opens on
    /// the canvas it always did.
    fn serialization_key() -> &'static str {
        "system_graph"
    }

    fn to_config(&self, _cx: &App) -> GraphCanvasConfig {
        GraphCanvasConfig {
            system: SystemGraphConfig::from_state(
                self.viewport.clone(),
                &self.overrides,
                &self.collapsed,
                self.direction,
            ),
            program: ProgramState {
                source: self.editor.text.clone(),
                text: self.text,
            },
        }
    }
}
