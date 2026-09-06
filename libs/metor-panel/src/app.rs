#![allow(unexpected_cfgs)]

use std::net::SocketAddr;
use std::sync::Arc;

use crate::connections::{
    AddressResolver, ConnectionOption, ConnectionTarget, OptionSpec, RegistryHandle, TargetId,
};
use crate::icons::Icon;
use crate::inspector::Inspector;
use crate::inspector::edits::{
    self, edit_value_rows, pending_edits, pending_edits_mut, review_rows,
};
use crate::inspector::palette::{Category, InspectionItem, ItemProvider, ItemRegistry};
use crate::inspector::rows::InspectorRow;
use crate::inspector::{InspectorMode, InspectorRequest, OpenInspectorGlobal};
use crate::tiles::panels::{
    AlarmPanel, AnnunciatorPanel, AnnunciatorPanelConfig, BrowserPanel, LogPanel, OutlinePanel,
    OutlinePanelConfig, PlotPanel, SequenceGridPanel, SequencePanel,
};
use crate::tiles::{
    OpenOutlineAction, PlotComponentAction, PreviewPlotAction, TileGroup, TileGroupEvent,
};
use crate::views::dashboard::{DashboardPanel, deserialize_dashboard};
use gpui::{
    App, Application, Bounds, Context, Entity, FocusHandle, Focusable, IntoElement, KeyBinding,
    MouseUpEvent, Pixels, Point, Render, SharedString, Window, actions, div, point, prelude::*, px,
    size,
};
use metor_db::{DB, Server};
use stellarator::{net::TcpListener, struc_con::stellar};

actions!(
    metor_panel,
    [
        OpenPalette,
        OpenLeader,
        CycleTabForward,
        CycleTabBackward,
        ToggleCmdLock,
        OpenReviewEdits,
        OpenConnections,
    ]
);

const TITLEBAR_HEIGHT: f32 = 36.0;

/// Window-level view that owns the tile tree and any active inspector overlay.
///
/// Inspector requests come in asynchronously (from palette callbacks, tile
/// events, pending-edit flushes). They're queued into `pending_*` fields and
/// drained inside `render` so entity creation always happens on the render
/// thread with access to [`Window`] for focus transfer.
pub(crate) struct AppRoot {
    db: Arc<DB>,
    tiles: Entity<TileGroup>,
    /// This window's own instances of the consumer-supplied overlays. Built
    /// per window from [`OverlayBuilders`] — two windows never share one
    /// view entity, since element state and hitboxes are per-window.
    overlays: Vec<gpui::AnyView>,
    inspector: Option<Entity<Inspector>>,
    /// Anchored, non-focused plot preview shown while the user holds shift
    /// over a component name. Tracked separately from `inspector` so the
    /// underlying surface keeps focus and the modifiers-changed listener
    /// can dismiss without disturbing the regular inspector slot.
    hover_preview: Option<HoverPreview>,
    pending_inspector_request: Option<(Box<dyn crate::tiles::PaneItemHandle>, Point<Pixels>)>,
    pending_pane_inspector_request: Option<(Entity<crate::tiles::Pane>, Point<Pixels>)>,
    pending_inspector_open: Option<InspectorRequest>,
    /// The transient chord menu, present only while open. Dropped (and focus
    /// returned to the root) once it dismisses, mirroring `inspector`.
    transient: Option<Entity<crate::transient::Transient>>,
    /// The connection picker, present only while open, mirroring `inspector`.
    connection_picker: Option<Entity<crate::connections::ConnectionPicker>>,
    /// Queued picker open; `true` means the picker must stay up until
    /// something connects (the first-open case). Drained in `render` like
    /// the other pending requests so focus transfer has a `Window`.
    pending_connection_picker: Option<bool>,
    /// Armed by mouse-down on the titlebar; the next mouse-move hands the
    /// drag to the compositor via `start_window_move` (Linux — macOS and
    /// Windows drag natively through the transparent titlebar / HTCAPTION).
    should_move: bool,
    focus_handle: FocusHandle,
}

struct HoverPreview {
    inspector: Entity<Inspector>,
    /// Identifies the source so a redundant `PreviewPlotAction` for the same
    /// (component, indices) is a no-op rather than a flicker-rebuild.
    key: (
        metor_proto::types::ComponentId,
        smallvec::SmallVec<[usize; 4]>,
    ),
}

impl AppRoot {
    pub(crate) fn new(
        db: Arc<DB>,
        tiles: Option<Entity<TileGroup>>,
        show_picker_if_disconnected: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let tiles = tiles.unwrap_or_else(|| cx.new(|cx| TileGroup::new(vec![], cx)));
        cx.subscribe(&tiles, Self::handle_tile_event).detach();
        // Repaint the status bar when alarms change.
        if let Some(store) = crate::alarms::try_global(cx) {
            cx.observe(&store, |_, _, cx| cx.notify()).detach();
        }
        // Repaint the titlebar chip when connections change, and greet a
        // fresh session (nothing connected, nothing auto-connected) with
        // the picker instead of an empty tile tree.
        let mut pending_connection_picker = None;
        if let Some(store) = crate::connections::try_global(cx) {
            cx.observe(&store, |_, _, cx| cx.notify()).detach();
            if show_picker_if_disconnected && store.read(cx).active().is_empty() {
                pending_connection_picker = Some(true);
            }
        }
        let overlay_builders = cx
            .try_global::<OverlayBuilders>()
            .map(|b| b.0.clone())
            .unwrap_or_default();
        let overlays = overlay_builders.iter().map(|build| build(cx)).collect();
        Self {
            db,
            tiles,
            overlays,
            inspector: None,
            hover_preview: None,
            pending_inspector_request: None,
            pending_pane_inspector_request: None,
            pending_inspector_open: None,
            transient: None,
            connection_picker: None,
            pending_connection_picker,
            should_move: false,
            focus_handle: cx.focus_handle(),
        }
    }

    fn open_inspector_with(
        &mut self,
        rows: Vec<Box<dyn crate::inspector::rows::InspectorRow>>,
        mode: InspectorMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let parent_focus = self.focus_handle.clone();
        let inspector = cx.new(|cx| {
            let mut insp = Inspector::new(rows, mode, cx);
            insp.set_parent_focus(parent_focus);
            insp
        });
        inspector.focus_handle(cx).focus(window);
        self.inspector = Some(inspector);
        cx.notify();
    }

    pub(crate) fn tiles(&self) -> &Entity<TileGroup> {
        &self.tiles
    }

    fn toggle_palette(&mut self, _: &OpenPalette, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(inspector) = &self.inspector
            && !inspector.read(cx).dismissed
        {
            self.inspector = None;
            self.focus_handle.focus(window);
            cx.notify();
            return;
        }

        let rows = ItemRegistry::root_rows(&self.db, cx);
        self.open_inspector_with(rows, InspectorMode::Centered, window, cx);
    }

    /// Open the transient chord menu. The leader keybinding is suppressed while
    /// a text field (Inspector search, node-editor inline edit) or the menu
    /// itself holds focus, so this only fires from the app's normal focus.
    fn open_leader(&mut self, _: &OpenLeader, window: &mut Window, cx: &mut Context<Self>) {
        let Some(on_open) = crate::inspector::open_inspector(cx) else {
            return;
        };
        let leader: SharedString = cx
            .global::<crate::theme::FontSettings>()
            .config
            .leader
            .clone()
            .into();
        let nodes =
            crate::transient::menu::default_menu(self.db.clone(), self.tiles.clone(), on_open);
        let parent_focus = self.focus_handle.clone();
        let transient = cx.new(|cx| {
            let mut t = crate::transient::Transient::new(leader, nodes, cx);
            t.set_parent_focus(parent_focus);
            t
        });
        transient.focus_handle(cx).focus(window);
        self.transient = Some(transient);
        cx.notify();
    }

    fn open_connections(
        &mut self,
        _: &OpenConnections,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(picker) = &self.connection_picker
            && !picker.read(cx).dismissed
        {
            return;
        }
        self.pending_connection_picker = Some(false);
        cx.notify();
    }

    fn open_connection_picker(
        &mut self,
        require_connection: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(store) = crate::connections::try_global(cx) else {
            return;
        };
        let parent_focus = self.focus_handle.clone();
        let picker = cx.new(|cx| {
            let mut picker =
                crate::connections::ConnectionPicker::new(store, require_connection, cx);
            picker.set_parent_focus(parent_focus);
            picker
        });
        picker.focus_handle(cx).focus(window);
        self.connection_picker = Some(picker);
        cx.notify();
    }

    fn cycle_tab_forward(
        &mut self,
        _: &CycleTabForward,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pane) = self.tiles.read(cx).active_pane(cx) else {
            return;
        };
        pane.update(cx, |pane, cx| pane.cycle_forward(cx));
    }

    fn cycle_tab_backward(
        &mut self,
        _: &CycleTabBackward,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pane) = self.tiles.read(cx).active_pane(cx) else {
            return;
        };
        pane.update(cx, |pane, cx| pane.cycle_backward(cx));
    }

    fn toggle_cmd_lock(&mut self, _: &ToggleCmdLock, _window: &mut Window, cx: &mut Context<Self>) {
        let locked = !pending_edits(cx).locked;
        pending_edits_mut(cx).locked = locked;
        cx.notify();
    }

    fn open_review_edits(
        &mut self,
        _: &OpenReviewEdits,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let rows = review_rows(self.db.clone(), cx);
        self.open_inspector_with(rows, InspectorMode::Centered, window, cx);
    }

    fn open_inspector(
        &mut self,
        item: &dyn crate::tiles::PaneItemHandle,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let entity = item.entity_any(cx);
        self.open_entity_inspector(&entity, position, window, cx);
    }

    fn open_entity_inspector(
        &mut self,
        entity: &gpui::AnyEntity,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(rows) = crate::inspector::reflect::rows_for_any_entity(entity, &self.db, cx) {
            self.open_inspector_with(rows, InspectorMode::Anchored(position), window, cx);
        } else {
            tracing::debug!("no inspector rows for entity");
        }
    }

    /// Anchored inspector for a tab-bar right-click: the pane's own settings
    /// plus a "New Tab" submenu reusing the palette's new-panel rows, so both
    /// entry points create tabs through the same wizards.
    fn open_pane_inspector(
        &mut self,
        pane: Entity<crate::tiles::Pane>,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut rows =
            crate::inspector::reflect::rows_for_any_entity(&pane.clone().into_any(), &self.db, cx)
                .unwrap_or_default();
        let db = self.db.clone();
        let on_open_inspector = crate::inspector::open_inspector(cx);
        rows.push(Box::new(crate::inspector::rows::NavRow::new(
            "New Tab",
            SharedString::new_static(""),
            Box::new(move |_cx| {
                crate::tiles::panels::new_panel_rows(
                    db.clone(),
                    pane.clone(),
                    on_open_inspector.clone(),
                    _cx,
                )
            }),
        )));
        self.open_inspector_with(rows, InspectorMode::Anchored(position), window, cx);
    }

    fn handle_inspect_entity(
        &mut self,
        action: &crate::inspector::InspectEntity,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_entity_inspector(&action.entity, action.position, window, cx)
    }

    fn handle_plot_component_action(
        &mut self,
        action: &PlotComponentAction,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pane) = self.tiles.read(cx).active_pane(cx) else {
            return;
        };
        let db = self.db.clone();
        let component_id = action.component_id;
        let indices = action.indices.clone();
        let plot = cx.new(|cx| PlotPanel::new(db, component_id, &indices, cx));
        pane.update(cx, |pane, cx| pane.add_item(Box::new(plot), cx));
    }

    fn handle_open_outline_action(
        &mut self,
        action: &OpenOutlineAction,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pane) = self.tiles.read(cx).active_pane(cx) else {
            return;
        };
        let db = self.db.clone();
        let cfg = OutlinePanelConfig {
            root: action.root.to_string(),
            ..Default::default()
        };
        let outline = cx.new(|cx| OutlinePanel::from_config(cfg, db, cx));
        pane.update(cx, |pane, cx| pane.add_item(Box::new(outline), cx));
    }

    fn handle_preview_plot_action(
        &mut self,
        action: &PreviewPlotAction,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = (action.component_id, action.indices.clone());
        if self
            .hover_preview
            .as_ref()
            .is_some_and(|hp| hp.key == key && !hp.inspector.read(cx).dismissed)
        {
            return;
        }

        let traces = crate::inspector::plot_preview::preview_traces(
            &self.db,
            action.component_id,
            &action.indices,
            cx,
        );
        if traces.is_empty() {
            return;
        }

        let spec = crate::inspector::plot_preview::build_plot_preview(self.db.clone(), traces, cx);
        let mode = InspectorMode::Anchored(action.anchor);
        let inspector = cx.new(|cx| {
            let mut insp = Inspector::with_view(spec, mode, cx);
            insp.set_passive();
            insp
        });
        self.hover_preview = Some(HoverPreview { inspector, key });
        cx.notify();
    }

    /// A left-button release outside this window's viewport while a tab drag
    /// is live: tear the tab out into its own window at the drop point.
    ///
    /// Registered as `on_mouse_up_out`, which also fires for a release over
    /// occluded chrome (the titlebar, an overlay) — the viewport test is what
    /// separates "dropped on nothing in this window" (a no-op, as before)
    /// from "dropped on the desktop".
    fn handle_tab_drag_out(
        &mut self,
        event: &MouseUpEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !cx.has_active_drag() {
            return;
        }
        let Some(drag) = crate::tiles::take_active_tab_drag(cx) else {
            return;
        };
        let viewport = window.viewport_size();
        let inside = event.position.x >= px(0.)
            && event.position.y >= px(0.)
            && event.position.x <= viewport.width
            && event.position.y <= viewport.height;
        if inside {
            return;
        }
        // Screen-space drop point, nudged so the cursor lands on the new
        // window's tab strip rather than its top-left corner.
        let origin = window.bounds().origin + event.position - point(px(60.0), px(20.0));
        let outer = drag.pane.read(cx).current_outer_size();
        let window_size = size(
            px(outer.width.max(400.0)),
            px(outer.height.max(300.0) + TITLEBAR_HEIGHT),
        );
        let db = self.db.clone();
        // Deferred: opening and closing windows mid-dispatch re-enters the
        // platform layer.
        cx.defer(move |cx: &mut App| {
            drag.pane
                .update(cx, |pane, cx| pane.remove_item(drag.ix, cx));
            let pane = cx.new(|cx| crate::tiles::Pane::new(vec![drag.item], cx));
            let tiles = cx.new(|cx| TileGroup::from_pane(pane, cx));
            crate::workspace::open_panel_window(
                db,
                Some(Bounds {
                    origin,
                    size: window_size,
                }),
                Some(tiles),
                false,
                cx,
            );
            // Tearing out a window's only tab reads as moving the window:
            // close the emptied source unless the picker is holding it open.
            if let Some(source) = drag.source_window.downcast::<AppRoot>() {
                let _ = source.update(cx, |root, window, cx| {
                    let picker_up = root
                        .connection_picker
                        .as_ref()
                        .is_some_and(|p| !p.read(cx).dismissed);
                    if !root.tiles.read(cx).has_items(cx) && !picker_up {
                        window.remove_window();
                    }
                });
            }
        });
    }

    fn handle_tile_event(
        &mut self,
        _tiles: Entity<TileGroup>,
        event: &TileGroupEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            TileGroupEvent::Inspect { item, position } => {
                let position = *position;
                let item = item.clone_handle();
                self.pending_inspector_request = Some((item, position));
                cx.notify();
            }
            TileGroupEvent::InspectPane { pane, position } => {
                self.pending_pane_inspector_request = Some((pane.clone(), *position));
                cx.notify();
            }
        }
    }
}

impl Focusable for AppRoot {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for AppRoot {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(inspector) = &self.inspector
            && inspector.read(cx).dismissed
        {
            self.inspector = None;
            self.focus_handle.focus(window);
        }

        if let Some(transient) = &self.transient
            && transient.read(cx).dismissed
        {
            self.transient = None;
            self.focus_handle.focus(window);
        }

        if let Some(picker) = &self.connection_picker
            && picker.read(cx).dismissed
        {
            self.connection_picker = None;
            self.focus_handle.focus(window);
        }

        if let Some(require_connection) = self.pending_connection_picker.take() {
            self.open_connection_picker(require_connection, window, cx);
        }

        if let Some((item, position)) = self.pending_inspector_request.take() {
            self.open_inspector(&*item, position, window, cx);
        }

        if let Some((pane, position)) = self.pending_pane_inspector_request.take() {
            self.open_pane_inspector(pane, position, window, cx);
        }

        if let Some(request) = self.pending_inspector_open.take() {
            self.open_inspector_with(request.rows, request.mode, window, cx);
        }

        if let Some(request) = pending_edits_mut(cx).pending_request.take() {
            let mode = request
                .anchor
                .map(InspectorMode::Anchored)
                .unwrap_or(InspectorMode::Centered);
            let rows = edit_value_rows(self.db.clone(), request);
            self.open_inspector_with(rows, mode, window, cx);
        }

        if pending_edits(cx).open_review_requested {
            pending_edits_mut(cx).open_review_requested = false;
            let rows = review_rows(self.db.clone(), cx);
            self.open_inspector_with(rows, InspectorMode::Centered, window, cx);
        }

        let theme = crate::theme::theme(cx);
        let font_family = crate::theme::font_family(cx);
        let titlebar = self.render_titlebar(&theme, window, cx);

        let mut root = div()
            .id("app-root")
            // Names the root so context-gated keybindings (the leader, which is
            // bound with `!Inspector && !RowList && !Transient`) have a non-empty
            // context stack to evaluate against — gpui's predicate eval returns
            // false for an empty stack, so without this the leader never fires.
            .key_context("AppRoot")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::toggle_palette))
            .on_action(cx.listener(Self::open_leader))
            .on_action(cx.listener(Self::cycle_tab_forward))
            .on_action(cx.listener(Self::cycle_tab_backward))
            .on_action(cx.listener(Self::toggle_cmd_lock))
            .on_action(cx.listener(Self::handle_inspect_entity))
            .on_action(cx.listener(Self::handle_plot_component_action))
            .on_action(cx.listener(Self::handle_open_outline_action))
            .on_action(cx.listener(Self::handle_preview_plot_action))
            .on_action(cx.listener(Self::open_review_edits))
            .on_action(cx.listener(Self::open_connections))
            .on_modifiers_changed(cx.listener(
                |this, event: &gpui::ModifiersChangedEvent, _window, cx| {
                    if !event.modifiers.shift && this.hover_preview.take().is_some() {
                        cx.notify();
                    }
                },
            ))
            // An in-window mouse-up ends any tab drag through the normal
            // drop handlers; clear the mirror so it can't go stale. Fires
            // exactly when `on_mouse_up_out` below doesn't (hover is the
            // gate for both, in opposite directions).
            .capture_any_mouse_up(|_, _, cx| {
                crate::tiles::take_active_tab_drag(cx);
            })
            .on_mouse_up_out(
                gpui::MouseButton::Left,
                cx.listener(Self::handle_tab_drag_out),
            )
            .font_family(font_family)
            .flex()
            .flex_col()
            .size_full()
            // Linux client-side decorations render into a transparent surface,
            // so the root must paint its own opaque background.
            .bg(theme.bg_primary)
            .child(titlebar)
            .child(div().flex_1().min_h_0().child(self.tiles.clone()));

        if let Some(inspector) = &self.inspector {
            root = root.child(inspector.clone());
        }

        if let Some(transient) = &self.transient {
            root = root.child(transient.clone());
        }

        if let Some(picker) = &self.connection_picker {
            root = root.child(picker.clone());
        }

        if let Some(preview) = &self.hover_preview {
            root = root.child(preview.inspector.clone());
        }

        // Consumer-supplied overlays, mounted last so they draw over everything.
        // Each is an opaque view this window built for itself at construction
        // (e.g. a modal that renders nothing until its own state says otherwise).
        for view in &self.overlays {
            root = root.child(view.clone());
        }

        crate::window_controls::client_side_decorations(root, window, cx)
    }
}

/// Consumer-supplied overlay constructors, installed via [`PanelApp::overlay`].
/// Held in a global as constructors rather than views so every window builds
/// its own instances in [`AppRoot::new`].
struct OverlayBuilders(Vec<std::rc::Rc<dyn Fn(&mut App) -> gpui::AnyView>>);

impl gpui::Global for OverlayBuilders {}

impl AppRoot {
    /// Active-alarm summary shown on the left of the title bar; clicking opens the alarm
    /// panel. Colored by the highest active severity.
    fn render_alarm_summary(
        &self,
        theme: &crate::theme::Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let mut bar = div()
            .id("alarm-summary")
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .text_size(px(11.0))
            .text_color(theme.text_secondary)
            .cursor_pointer()
            .on_click(cx.listener(|this, _, _, cx| this.open_alarms(cx)));

        if let Some(store) = crate::alarms::try_global(cx) {
            let store = store.read(cx);
            let state = store.state();
            // Pending, not active: a latched alarm still wants an operator, and a
            // shelved one deliberately does not light the bar.
            let pending = state.pending_count();
            if pending == 0 {
                bar = bar.child(Icon::Dot.svg_color(7.0, theme.control_active));
            } else {
                if let Some(severity) = state.highest_pending_severity() {
                    let idx = crate::alarms::severity_index(severity);
                    bar = bar.child(Icon::Dot.svg_color(7.0, theme.alarm_color(idx)));
                }
                let counts = state.counts_by_severity();
                for idx in (0..counts.len()).rev() {
                    if counts[idx] == 0 {
                        continue;
                    }
                    let label = match idx {
                        2 => "crit",
                        1 => "warn",
                        _ => "info",
                    };
                    bar = bar.child(
                        div()
                            .text_color(theme.alarm_color(idx))
                            .child(SharedString::from(format!("{} {}", counts[idx], label))),
                    );
                }
                bar = bar.child(SharedString::from(format!(
                    "{pending} pending, {} unacked",
                    state.unacked_count()
                )));
            }
        }

        bar.into_any_element()
    }

    /// Reveal the alarm panel: focus the existing alarms tab wherever it lives
    /// in the layout, opening a fresh one in the active pane only when none is
    /// open.
    fn open_alarms(&mut self, cx: &mut Context<Self>) {
        let db = self.db.clone();
        self.tiles.update(cx, |tiles, cx| {
            tiles.focus_or_open(
                <AlarmPanel as crate::tiles::PaneItem>::serialization_key(),
                |cx| Box::new(cx.new(|cx| AlarmPanel::new(db, cx))),
                cx,
            );
        });
    }

    /// A flat, borderless titlebar control: quiet at rest, background on
    /// hover. The titlebar reads as one surface instead of a row of pills.
    fn titlebar_segment(
        theme: &crate::theme::Theme,
        id: &'static str,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id)
            .px(px(8.0))
            .py(px(3.0))
            .rounded(px(4.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .text_size(px(12.0))
            .text_color(theme.text_primary)
            .cursor_pointer()
            .hover(|s| s.bg(theme.bg_primary))
    }

    fn titlebar_separator(theme: &crate::theme::Theme) -> gpui::Div {
        div().w(px(1.0)).h(px(14.0)).bg(theme.border_primary)
    }

    /// The titlebar's identity: which system(s) this panel is looking at.
    /// Sits on the left like an editor's project breadcrumb; clicking opens
    /// the connection dialog.
    fn render_connection_segment(
        &self,
        theme: &crate::theme::Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        use crate::connections::ConnectionStatus;

        let summary = crate::connections::try_global(cx).map(|store| {
            let store = store.read(cx);
            let active = store.active();
            let label: SharedString = match active {
                [] => SharedString::new_static("not connected"),
                [only] => only.target.name.clone(),
                many => SharedString::from(format!("{} systems", many.len())),
            };
            // The dot shows the worst status so a degraded mirror is
            // visible even while other connections are healthy.
            let worst = active
                .iter()
                .map(|c| match c.status() {
                    ConnectionStatus::Failed(_) => 2,
                    ConnectionStatus::Connecting | ConnectionStatus::Reconnecting => 1,
                    _ => 0,
                })
                .max();
            let dot = match worst {
                None => theme.text_tertiary,
                Some(2) => theme.error_accent,
                Some(1) => theme.text_secondary,
                Some(_) => theme.control_active,
            };
            (label, dot)
        });
        let Some((label, dot)) = summary else {
            return div().into_any_element();
        };

        Self::titlebar_segment(theme, "connection-segment")
            .child(crate::icons::Icon::Dot.svg_color(7.0, dot))
            .child(label)
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, _window, cx| {
                    this.pending_connection_picker = Some(false);
                    cx.notify();
                }),
            )
            .into_any_element()
    }

    fn render_titlebar(
        &self,
        theme: &crate::theme::Theme,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let pending = pending_edits(cx);
        let edit_count = pending.edits.len();
        let locked = pending.locked;

        let icon = if locked {
            crate::icons::Icon::Lock
        } else {
            crate::icons::Icon::LockOpen
        };
        let icon_color = if locked {
            theme.text_tertiary
        } else {
            theme.text_primary
        };
        let lock_button = div()
            .id("cmd-lock")
            .flex()
            .items_center()
            .justify_center()
            .w(px(28.0))
            .h(px(24.0))
            .rounded(px(3.0))
            .child(icon.svg_color(14.0, icon_color))
            .cursor_pointer()
            .hover(|s| s.bg(theme.bg_primary))
            .on_mouse_down(gpui::MouseButton::Left, |_, _window, cx| {
                let locked = !pending_edits(cx).locked;
                pending_edits_mut(cx).locked = locked;
                cx.refresh_windows();
            });

        // Occluded so these controls win the non-client hit-test: without it,
        // Windows resolves clicks over the segments to the surrounding
        // titlebar drag area (HTCAPTION) and drags the window instead of
        // clicking. Left: the panel's identity — what it's connected to.
        let left = div()
            .occlude()
            .flex()
            .flex_row()
            .items_center()
            .pl(px(if cfg!(target_os = "macos") { 78.0 } else { 8.0 }))
            .child(self.render_connection_segment(theme, cx));

        let mut right = div()
            .occlude()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .pr(px(8.0));

        let time = crate::temporal::snapshot(cx);
        let mode = if time.as_ref().is_some_and(|t| t.playing) {
            "Playing"
        } else if crate::temporal::is_live(cx) {
            "Live"
        } else {
            "Paused"
        };
        let config = crate::temporal::config(cx);
        let view_label = if mode == "Live" {
            "Live".to_string()
        } else {
            format!(
                "{mode} {}",
                crate::temporal::view_time(cx)
                    .map(|t| crate::temporal::display::label(t, cx))
                    .unwrap_or_else(|| "Unavailable".into())
            )
        };
        right = right.child(
            Self::titlebar_segment(theme, "global-view-time")
                .child(SharedString::from(view_label))
                .tooltip(|_, cx| {
                    let c = crate::temporal::config(cx);
                    let text = crate::temporal::view_time(cx)
                        .map(|t| crate::temporal::model::timestamp_text(t, &c.timezone))
                        .unwrap_or_else(|| "Unavailable".into());
                    crate::views::tooltip::TooltipText::build(text.into(), cx)
                })
                .on_mouse_down(gpui::MouseButton::Left, |event, window, cx| {
                    crate::temporal::picker::open(
                        Some(crate::temporal::picker::Target::View),
                        InspectorMode::Anchored(event.position),
                        window,
                        cx,
                    )
                })
                .on_mouse_down(gpui::MouseButton::Right, |event, window, cx| {
                    crate::temporal::picker::open(
                        None,
                        InspectorMode::Anchored(event.position),
                        window,
                        cx,
                    )
                }),
        );
        if mode != "Live" {
            right = right.child(
                Self::titlebar_segment(theme, "time-go-live")
                    .child(crate::icons::Icon::SkipForward.svg_color(14.0, theme.text_secondary))
                    .tooltip(|_, cx| {
                        crate::views::tooltip::TooltipText::build("Go live".into(), cx)
                    })
                    .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| {
                        let _ = crate::temporal::dispatch(crate::temporal::TimeAction::Live, cx);
                    }),
            );
        }
        right = right.child(
            Self::titlebar_segment(theme, "global-time-range")
                .child(SharedString::from(time.as_ref().map_or_else(
                    || config.range.to_string(),
                    |t| crate::temporal::display::range(config.range, &config, &t.context),
                )))
                .child(crate::icons::Icon::ChevronDown.svg_color(10.0, theme.text_secondary))
                .on_mouse_down(gpui::MouseButton::Left, |event, window, cx| {
                    crate::temporal::picker::open(
                        Some(crate::temporal::picker::Target::Range),
                        InspectorMode::Anchored(event.position),
                        window,
                        cx,
                    )
                })
                .on_mouse_down(gpui::MouseButton::Right, |event, window, cx| {
                    crate::temporal::picker::open(
                        None,
                        InspectorMode::Anchored(event.position),
                        window,
                        cx,
                    )
                }),
        );
        let action = if mode == "Paused" {
            crate::temporal::TimeAction::Play { from_start: false }
        } else {
            crate::temporal::TimeAction::Pause
        };
        right = right.child(
            Self::titlebar_segment(theme, "time-transport")
                .child(
                    if mode == "Paused" {
                        crate::icons::Icon::Play
                    } else {
                        crate::icons::Icon::Pause
                    }
                    .svg_color(14.0, theme.text_secondary),
                )
                .tooltip(move |_, cx| {
                    crate::views::tooltip::TooltipText::build(
                        if mode == "Paused" { "Play" } else { "Pause" }.into(),
                        cx,
                    )
                })
                .on_mouse_down(gpui::MouseButton::Left, move |event, window, cx| {
                    if crate::temporal::dispatch(action.clone(), cx).is_err() {
                        crate::temporal::picker::open(
                            None,
                            InspectorMode::Anchored(event.position),
                            window,
                            cx,
                        );
                    }
                }),
        );

        right = right.child(Self::titlebar_separator(theme));
        right = right.child(self.render_alarm_summary(theme, cx));

        if edit_count > 0 {
            let label = SharedString::from(format!("{} pending", edit_count));
            right = right.child(Self::titlebar_separator(theme));
            right = right.child(
                Self::titlebar_segment(theme, "pending-edits")
                    .text_color(theme.text_secondary)
                    .child(label)
                    .on_mouse_down(gpui::MouseButton::Left, |_, _window, cx| {
                        pending_edits_mut(cx).open_review_requested = true;
                        cx.refresh_windows();
                    }),
            );
        }

        right = right.child(Self::titlebar_separator(theme));
        right = right.child(lock_button);

        div()
            .id("titlebar")
            .window_control_area(gpui::WindowControlArea::Drag)
            // Occluded so a titlebar press never hovers the root's
            // `track_focus` hitbox: gpui's focus-transfer listener calls
            // `prevent_default()`, and the Windows backend treats a
            // default-prevented mouse-down as handled — swallowing the
            // WM_NCLBUTTONDOWN that starts the native caption drag.
            .occlude()
            .bg(theme.bg_secondary)
            .border_b_1()
            .border_color(theme.border_primary)
            .w_full()
            .flex_shrink_0()
            .h(px(TITLEBAR_HEIGHT))
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            // Drag gesture for compositors that need an explicit hand-off:
            // mouse-down arms, the first mouse-move starts the compositor
            // drag. macOS drags natively via the transparent titlebar and
            // Windows via the HTCAPTION hit-test, so these never fire there.
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, _, _| this.should_move = true),
            )
            .on_mouse_move(cx.listener(|this, _, window, _| {
                if this.should_move {
                    this.should_move = false;
                    window.start_window_move();
                }
            }))
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(|this, _, _, _| this.should_move = false),
            )
            .on_mouse_down_out(cx.listener(|this, _, _, _| this.should_move = false))
            .on_click(|event, window, _| {
                if event.click_count() == 2 {
                    if cfg!(target_os = "macos") {
                        window.titlebar_double_click();
                    } else {
                        window.zoom_window();
                    }
                }
            })
            .when(
                matches!(
                    window.window_decorations(),
                    gpui::Decorations::Client { .. }
                ) && window.window_controls().window_menu,
                |bar| {
                    bar.on_mouse_down(gpui::MouseButton::Right, |event, window, _| {
                        window.show_window_menu(event.position);
                    })
                },
            )
            .child(left)
            .child(right)
            .when(
                crate::window_controls::needs_window_controls(window),
                |bar| bar.child(crate::window_controls::window_controls(theme, window)),
            )
            .into_any_element()
    }
}

/// Builder that owns construction of the panel application.
///
/// A consumer supplies a [`DB`], optionally asks the builder to boot the
/// metor-db wire-protocol server, registers custom command-palette providers,
/// and finally calls [`run`](PanelApp::run) to open the window. Custom
/// commands reuse the existing [`InspectionItem`]/[`ItemProvider`] types and
/// reach the tile tree and inspector through the gpui globals installed during
/// startup, so no extra context has to be threaded into their callbacks.
///
/// [`on_init`](PanelApp::on_init) is the general escape hatch: it runs with
/// `&mut App` after every built-in registry is initialized but before the
/// window opens, which is where future custom-tile/widget registration will
/// hook in.
pub struct PanelApp {
    db: Arc<DB>,
    server_addr: Option<SocketAddr>,
    targets: Vec<ConnectionTarget>,
    connection_sources: Vec<Box<dyn FnOnce(RegistryHandle)>>,
    auto_connect: Vec<TargetId>,
    address_resolver: Option<AddressResolver>,
    default_options: OptionSpec,
    command_providers: Vec<(Category, ItemProvider)>,
    init_hooks: Vec<Box<dyn FnOnce(&mut App)>>,
    overlays: Vec<std::rc::Rc<dyn Fn(&mut App) -> gpui::AnyView>>,
    views: Vec<(
        crate::views::dashboard::WidgetKind,
        crate::views::dashboard::WidgetSpec,
    )>,
}

impl PanelApp {
    /// Start from a consumer-owned database.
    pub fn new(db: Arc<DB>) -> Self {
        Self {
            db,
            server_addr: None,
            targets: Vec::new(),
            connection_sources: Vec::new(),
            auto_connect: Vec::new(),
            address_resolver: None,
            default_options: OptionSpec::default(),
            command_providers: Vec::new(),
            init_hooks: Vec::new(),
            overlays: Vec::new(),
            views: Vec::new(),
        }
    }

    /// Pre-register a connectable target in the picker.
    pub fn connection(mut self, target: ConnectionTarget) -> Self {
        self.targets.push(target);
        self
    }

    /// Hand a [`RegistryHandle`] to a discovery source at startup. The
    /// source owns its own threads (an mDNS scan, a cloud poll) and upserts
    /// targets as it finds them; they appear in the picker reactively.
    pub fn connection_source(mut self, source: impl FnOnce(RegistryHandle) + 'static) -> Self {
        self.connection_sources.push(Box::new(source));
        self
    }

    /// Connect to a registered target immediately at startup, skipping the
    /// picker. The id must match a target registered via
    /// [`connection`](PanelApp::connection).
    pub fn auto_connect(mut self, id: impl Into<SharedString>) -> Self {
        self.auto_connect.push(TargetId(id.into()));
        self
    }

    /// Declare the knobs shown for every target that doesn't declare its
    /// own. This is the only way a discovered or address-resolved target
    /// gets a configuration surface — a wrapper never constructs those, so
    /// it can't hang a spec off them with
    /// [`ConnectionTarget::with_options`].
    pub fn default_connection_options(
        mut self,
        options: impl IntoIterator<Item = ConnectionOption>,
    ) -> Self {
        self.default_options = options.into_iter().collect();
        self
    }

    /// Replace how the dialog's "Connect to address…" input is interpreted.
    /// The default parses `host:port` into the built-in TCP mirror; a
    /// wrapper speaking its own protocol supplies a placeholder and a parse
    /// function producing any [`ConnectionTarget`]. The same resolver
    /// revives address-carrying recents across restarts.
    pub fn address_resolver(
        mut self,
        placeholder: impl Into<SharedString>,
        resolve: impl Fn(&str) -> Result<ConnectionTarget, SharedString> + Send + Sync + 'static,
    ) -> Self {
        self.address_resolver = Some(AddressResolver {
            placeholder: placeholder.into(),
            resolve: Arc::new(resolve),
        });
        self
    }

    /// Mount a consumer-built view over the window root. `build` runs once per
    /// window (with `&mut App`, after the built-in registries exist) and
    /// returns any [`Render`] entity as an [`AnyView`](gpui::AnyView); it is
    /// drawn above the tile tree and inspector every frame. The consumer owns
    /// the view's state and updates — a modal that renders nothing until
    /// shown, say. Call more than once to stack overlays in insertion order.
    pub fn overlay<V, F>(mut self, build: F) -> Self
    where
        V: Render,
        F: Fn(&mut App) -> Entity<V> + 'static,
    {
        self.overlays
            .push(std::rc::Rc::new(move |cx| build(cx).into()));
        self
    }

    /// Register a downstream view kind before the application starts.
    /// Dashboard construction, inspection identity, labeling, and live
    /// persistence are all supplied by the single [`WidgetSpec`].
    pub fn view(
        mut self,
        kind: crate::views::dashboard::WidgetKind,
        spec: crate::views::dashboard::WidgetSpec,
    ) -> Self {
        self.views.push((kind, spec));
        self
    }

    /// Boot the metor-db wire-protocol server on `addr` in a background task
    /// before opening the window. Omit to leave serving to the consumer.
    pub fn serve(mut self, addr: SocketAddr) -> Self {
        self.server_addr = Some(addr);
        self
    }

    /// Mirror a long-running remote metor-db at `addr`: live telemetry
    /// streams into the local DB, and plots hydrate remote-only history on
    /// demand (gaps render as translucent bands until their nodes land).
    ///
    /// Sugar over [`connection`](PanelApp::connection) +
    /// [`auto_connect`](PanelApp::auto_connect) with the built-in TCP target.
    pub fn remote(self, addr: SocketAddr) -> Self {
        let target = ConnectionTarget::tcp("Remote", addr);
        let id = target.id.0.clone();
        self.connection(target).auto_connect(id)
    }

    /// Register a custom palette category with a pull-based provider. The
    /// provider is evaluated on every palette open, so it always reflects the
    /// current world state.
    pub fn command_provider(mut self, category: Category, provider: ItemProvider) -> Self {
        self.command_providers.push((category, provider));
        self
    }

    /// Register a single static command under `Category::Custom(category)`.
    /// Convenience wrapper over [`command_provider`](PanelApp::command_provider)
    /// for the common "one named command" case.
    pub fn command(
        self,
        category: impl Into<SharedString>,
        label: impl Into<SharedString>,
        callback: Arc<dyn Fn(&mut Window, &mut App)>,
    ) -> Self {
        let label = label.into();
        let provider: ItemProvider = Arc::new(move |_cx| {
            vec![InspectionItem::Command {
                label: label.clone(),
                callback: callback.clone(),
            }]
        });
        self.command_provider(Category::Custom(category.into()), provider)
    }

    /// Run arbitrary setup with `&mut App` after all built-in registries are
    /// initialized but before the window opens. Hooks run in insertion order.
    pub fn on_init(mut self, hook: impl FnOnce(&mut App) + 'static) -> Self {
        self.init_hooks.push(Box::new(hook));
        self
    }

    /// Boot the gpui application and block until it quits.
    ///
    /// Registers the theme, fonts, icons, edit queue, palette providers,
    /// inspector registry, and widget registry, drains any consumer-supplied
    /// command providers and init hooks, then opens the first window. The
    /// app outlives its last window: closing them all leaves the process in
    /// the dock, and a reopen (dock click) spawns a fresh window.
    pub fn run(self) {
        let PanelApp {
            db,
            server_addr,
            targets,
            connection_sources,
            auto_connect,
            address_resolver,
            default_options,
            command_providers,
            init_hooks,
            overlays,
            views,
        } = self;

        if let Some(addr) = server_addr {
            let server_db = db.clone();
            stellar(move || async move {
                let server = Server {
                    listener: TcpListener::bind(addr).unwrap(),
                    db: server_db,
                };
                server.run().await
            });
        }

        let app = Application::new().with_assets(crate::icons::IconAssets);
        // A dock-click (macOS "reopen") with every window closed brings the
        // app back with a fresh window; the process deliberately outlives its
        // last window so the stores and connections keep running.
        let reopen_db = db.clone();
        app.on_reopen(move |cx| {
            if crate::workspace::panel_windows(cx).is_empty() {
                crate::workspace::open_panel_window(reopen_db.clone(), None, None, true, cx);
            }
        });
        app.run(move |cx: &mut App| {
            crate::theme::register_fonts(cx);
            let cfg = crate::config::load();
            // Capture the leader before `cfg` moves into the font global; it
            // parameterizes the chord-menu keybinding below.
            let leader = cfg.leader.clone();
            let family = crate::theme::resolve_font_family(cx, &cfg);
            cx.set_global(crate::theme::FontSettings {
                family,
                config: cfg,
            });
            cx.set_global(crate::theme::ActiveTheme(Arc::new(
                crate::theme::DARK.clone(),
            )));
            edits::init(cx);
            crate::temporal::TemporalController::init(db.clone(), cx);
            ItemRegistry::init(cx);
            crate::temporal::picker::register(cx);
            crate::inspector::registry::InspectorRegistry::init(db.clone(), cx);
            crate::views::dashboard::WidgetRegistry::init(cx);
            crate::dynamic::expressions::Expressions::init(cx);
            crate::dynamic::worker::DynamicWorker::init(cx);
            crate::views::map::tiles::TileStore::init(cx);
            crate::backfill::Backfiller::init(db.clone(), cx);
            crate::views::exec_timeline::inspector_rows::register_inspector_rows(cx);
            crate::views::Timeline::register(cx);
            crate::alarms::AlarmStore::init(db.clone(), cx);
            crate::logs::LogStore::init(db.clone(), cx);
            crate::presets::TargetPresetStore::init(db.clone(), cx);
            crate::sequences::SequenceStore::init(db.clone(), cx);
            crate::plot_events::EventSourceRegistry::init(cx);
            crate::wiring::WiringStore::init(db.clone(), cx);
            register_pane_item_deserializers(db.clone(), cx);
            for (kind, spec) in views {
                cx.global_mut::<crate::views::dashboard::WidgetRegistry>()
                    .register(kind, spec);
            }
            let registered_tiles = cx
                .global::<crate::views::dashboard::WidgetRegistry>()
                .tile_specs();
            for spec in registered_tiles {
                cx.global_mut::<crate::tiles::ItemRegistry>()
                    .register_view(spec, db.clone());
            }

            // Connections: seed builder-registered targets, hand the
            // registry to discovery sources, then fire auto-connects.
            // LoD spawning is a store concern — it starts with the first
            // local-authority connection rather than at boot.
            let registry = crate::connections::ConnectionsStore::init(db.clone(), cx);
            if let Some(store) = crate::connections::try_global(cx) {
                store.update(cx, |store, cx| {
                    if let Some(resolver) = address_resolver {
                        store.set_resolver(resolver);
                    }
                    store.set_default_options(default_options);
                    for target in targets {
                        store.upsert_target(target, cx);
                    }
                    for id in auto_connect {
                        let Some(target) =
                            store.state().targets().iter().find(|t| t.id == id).cloned()
                        else {
                            tracing::warn!(%id, "auto-connect target not registered");
                            continue;
                        };
                        store.connect(target, cx);
                    }
                });
            }
            for source in connection_sources {
                source(registry.clone());
            }
            register_connection_commands(cx);

            // Inspector requests route to the window they were made in:
            // the callback resolves the root of whatever `Window` the
            // caller passed, so one installation serves every window.
            cx.set_global(OpenInspectorGlobal(Arc::new(|request, window, cx| {
                let Some(root) = window.root::<AppRoot>().flatten() else {
                    return;
                };
                root.update(cx, |this, cx| {
                    this.pending_inspector_open = Some(request);
                    cx.notify();
                });
            })));
            crate::inspector::palette::register_builtin_providers(db.clone(), cx);

            // Consumer extensions: register custom palette providers, run
            // init hooks, then build overlays. All happen after the built-in
            // registries exist so they can call into any of them; overlays
            // run last so they can rely on anything a hook installed.
            for (category, provider) in command_providers {
                ItemRegistry::register(cx, category, provider);
            }
            for hook in init_hooks {
                hook(cx);
            }
            cx.set_global(OverlayBuilders(overlays));

            set_dock_icon();
            // `secondary-` resolves to cmd on macOS and ctrl elsewhere
            // (`cmd-` would be the Win/Super key off-macOS).
            cx.bind_keys([
                KeyBinding::new("secondary-p", OpenPalette, None),
                // The leader opens the transient chord menu, and it is a
                // bare key — `space` by default — so anything typing into
                // it must say so. `TextInput` is that: every host owning
                // an editable field declares it beside its own name, and
                // one negation covers all of them, including ones that do
                // not exist yet. Naming the panes individually is what let
                // the program pane swallow its own spacebar.
                KeyBinding::new(leader.as_str(), OpenLeader, Some(NOT_TYPING)),
                KeyBinding::new("ctrl-tab", CycleTabForward, None),
                KeyBinding::new("shift-ctrl-tab", CycleTabBackward, None),
                KeyBinding::new("secondary-l", ToggleCmdLock, None),
                KeyBinding::new("secondary-shift-e", OpenReviewEdits, None),
                // Scoped to a focused browser: the bar is per pane, and
                // nothing else claims the find chord yet.
                KeyBinding::new(
                    "secondary-f",
                    crate::views::column_browser::ToggleFilterBar,
                    Some("ColumnBrowser"),
                ),
                KeyBinding::new(
                    "secondary-f",
                    crate::views::column_browser::ToggleFilterBar,
                    Some("ComponentOutline"),
                ),
            ]);

            // A non-last close: re-snapshot the survivors right away, so
            // a quit inside the next autosave interval can't resurrect
            // the closed window. After the last close this is a no-op,
            // leaving the should-close snapshot that included it.
            cx.on_window_closed(|cx| {
                if !crate::workspace::panel_windows(cx).is_empty() {
                    crate::connections::flush_layout_now(cx);
                }
            })
            .detach();

            crate::workspace::open_panel_window(db.clone(), None, None, true, cx);
        });
    }
}

/// Palette entries for the connection system: "Connect…" opens the picker,
/// each active connection contributes a "Disconnect <name>" command, and
/// every configurable target contributes a sub-menu of its knobs — the same
/// rows the dialog embeds, reached without the dialog. Pull-based like every
/// provider, so both lists always match the live registry.
fn register_connection_commands(cx: &mut App) {
    ItemRegistry::register(
        cx,
        Category::Command,
        Arc::new(|cx| {
            let mut items = vec![InspectionItem::Command {
                label: SharedString::new_static("Connect\u{2026}"),
                callback: Arc::new(|window, cx| {
                    window.dispatch_action(Box::new(OpenConnections), cx);
                }),
            }];
            let Some(store) = crate::connections::try_global(cx) else {
                return items;
            };
            for conn in store.read(cx).active() {
                let id = conn.target.id.clone();
                let store = store.clone();
                items.push(InspectionItem::Command {
                    label: SharedString::from(format!("Disconnect {}", conn.target.name)),
                    callback: Arc::new(move |_window, cx| {
                        store.update(cx, |store, cx| store.disconnect(&id, cx));
                    }),
                });
            }
            // Every configurable target would otherwise add its own top-level
            // "{name} · options" row, swamping the palette when discovery
            // registers one target per deployment. Nest them under a single
            // submenu whose children are rebuilt on entry, so the target list
            // stays live without flooding the root page.
            let has_configurable = store
                .read(cx)
                .state()
                .targets()
                .iter()
                .any(|t| !store.read(cx).state().spec_for(t).is_empty());
            if has_configurable {
                let store = store.clone();
                items.push(InspectionItem::SubMenu {
                    label: SharedString::new_static("Connection options\u{2026}"),
                    summary: SharedString::new_static(""),
                    build: Arc::new(move |cx| {
                        store
                            .read(cx)
                            .state()
                            .targets()
                            .iter()
                            .filter(|t| !store.read(cx).state().spec_for(t).is_empty())
                            .cloned()
                            .map(|target| {
                                let store = store.clone();
                                Box::new(crate::inspector::rows::NavRow::new(
                                    target.name.clone(),
                                    target.detail.clone(),
                                    Box::new(move |cx| {
                                        crate::connections::options::option_rows(
                                            store.clone(),
                                            target.clone(),
                                            cx,
                                        )
                                    }),
                                )) as Box<dyn InspectorRow>
                            })
                            .collect()
                    }),
                });
            }
            items
        }),
    );
}

/// Populate the pane-item registry so [`TileGroup::from_json`] can rehydrate
/// specialized built-in panel kinds. Ordinary registered views are adapted
/// into this registry immediately afterwards; Dashboard retains its dedicated
/// deserializer because it is itself a host. The populated registry is a gpui
/// The key context every host owning an editable field declares beside its own
/// name, and the reason bare-key shortcuts can exist at all.
///
/// A predicate naming panes individually is wrong the moment someone adds a
/// pane — which is exactly how the program pane came to swallow its own
/// spacebar — so the rule is stated once, here, and every field opts in.
pub const TEXT_INPUT: &str = "TextInput";

/// Context predicate for a bare key that must not fire while something is
/// being typed into. `Transient` is the chord menu itself, which must not
/// re-trigger its own leader.
pub const NOT_TYPING: &str = "!TextInput && !Transient";

/// Deleting the selected node: only in a node editor, and never while an
/// `Global` so palette callbacks can fetch it without threading.
fn register_pane_item_deserializers(db: Arc<DB>, cx: &mut App) {
    use crate::tiles::ItemRegistry as PaneItemRegistry;
    let mut reg = PaneItemRegistry::default();

    register_panel::<AlarmPanel>(&mut reg, db.clone(), AlarmPanel::from_config);
    register_panel::<LogPanel>(&mut reg, db.clone(), LogPanel::from_config);
    register_panel::<SequencePanel>(&mut reg, db.clone(), SequencePanel::from_config);
    register_panel::<SequenceGridPanel>(&mut reg, db.clone(), SequenceGridPanel::from_config);
    register_panel::<OutlinePanel>(&mut reg, db.clone(), OutlinePanel::from_config);
    // The outline replaced both tables; layouts saved with either key open
    // as an outline, keeping the data table's filter.
    for legacy in ["component_table", "data_table"] {
        let db = db.clone();
        reg.register_erased(legacy, move |state, cx| {
            let cfg: OutlinePanelConfig = serde_json::from_str(state).unwrap_or_default();
            let db = db.clone();
            Some(Box::new(
                cx.new(|cx| OutlinePanel::from_config(cfg, db, cx)),
            ))
        });
    }
    register_panel::<BrowserPanel>(&mut reg, db.clone(), BrowserPanel::from_config);

    // Layouts written before the annunciator rename still name it
    // `traffic_light_grid`; the alias rehydrates them and they re-save under
    // the new key.
    let db_annunciator = db.clone();
    reg.register_erased("traffic_light_grid", move |state, cx| {
        let cfg: AnnunciatorPanelConfig = serde_json::from_str(state).unwrap_or_default();
        let db = db_annunciator.clone();
        let entity = cx.new(|cx| AnnunciatorPanel::from_config(cfg, db, cx));
        Some(Box::new(entity) as Box<dyn crate::tiles::PaneItemHandle>)
    });

    // Dashboard's deserializer returns a fully-constructed entity rather
    // than a `Self`, so it doesn't fit the generic helper.
    let db_dashboard = db.clone();
    reg.register::<DashboardPanel>(move |state, cx| {
        Some(deserialize_dashboard(db_dashboard.clone(), state, cx))
    });

    cx.set_global(reg);
}

/// Wire one closure for `T` into the pane-item registry. The closure parses
/// `T::Config` out of the state blob (falling back to `Default` on parse
/// failure) and constructs `T` via the supplied `from_config` function.
fn register_panel<T: crate::tiles::PaneItem>(
    reg: &mut crate::tiles::ItemRegistry,
    db: Arc<DB>,
    from_config: fn(T::Config, Arc<DB>, &mut Context<T>) -> T,
) {
    reg.register::<T>(move |state, cx| {
        let cfg: T::Config = serde_json::from_str(state).unwrap_or_default();
        let db = db.clone();
        Some(cx.new(|cx| from_config(cfg, db, cx)))
    });
}

#[cfg(target_os = "macos")]
#[allow(deprecated, unexpected_cfgs)]
fn set_dock_icon() {
    use cocoa::appkit::NSImage;
    use cocoa::base::{id, nil};
    use cocoa::foundation::NSData;
    use objc::msg_send;
    use objc::sel;
    use objc::sel_impl;

    let icon_bytes = include_bytes!("../assets/app-icon.png");
    unsafe {
        let data = NSData::dataWithBytes_length_(
            nil,
            icon_bytes.as_ptr() as *const std::ffi::c_void,
            icon_bytes.len() as u64,
        );
        let image: id = msg_send![NSImage::alloc(nil), initWithData: data];
        if image != nil {
            let cls = objc::runtime::Class::get("NSApplication").unwrap();
            let app: id = msg_send![cls, sharedApplication];
            let _: () = msg_send![app, setApplicationIconImage: image];
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn set_dock_icon() {}

#[cfg(test)]
mod keybinding_tests {
    use super::{NOT_TYPING, TEXT_INPUT};
    use gpui::{KeyBindingContextPredicate, KeyContext};

    /// The focus path, root first — the order gpui evaluates a predicate over.
    fn path(contexts: &[&str]) -> Vec<KeyContext> {
        contexts
            .iter()
            .map(|c| KeyContext::try_from(*c).expect("a context parses"))
            .collect()
    }

    fn fires(predicate: &str, contexts: &[&str]) -> bool {
        KeyBindingContextPredicate::parse(predicate)
            .expect("a predicate parses")
            .depth_of(&path(contexts))
            .is_some()
    }

    /// A host declares its own name *and* `TextInput` from one string, which
    /// is what lets one negation cover every editable field.
    #[test]
    fn a_host_can_declare_its_name_and_text_input_together() {
        let context = KeyContext::try_from("Inspector TextInput").unwrap();
        assert!(context.contains("Inspector"));
        assert!(context.contains(TEXT_INPUT));
    }

    /// The bug: the leader is a bare `space` by default, so it must not fire
    /// anywhere a character is being typed — including panes that did not
    /// exist when the predicate was written.
    #[test]
    fn the_leader_never_fires_while_something_is_being_typed_into() {
        assert!(fires(NOT_TYPING, &["AppRoot"]));
        assert!(fires(NOT_TYPING, &["AppRoot", "ExecTimeline"]));
        assert!(fires(NOT_TYPING, &["AppRoot", "Dashboard"]));

        for typing in [
            &["AppRoot", "Dashboard TextInput"][..],
            &["AppRoot", "Inspector TextInput"][..],
            &["AppRoot", "Inspector TextInput", "RowList TextInput"][..],
            &["AppRoot", "ConnectionPicker TextInput"][..],
            // A field nested under a pane that is not itself a text host: the
            // negation looks at the whole path, not just the leaf.
            &["AppRoot", "ExecTimeline", "RowList TextInput"][..],
        ] {
            assert!(
                !fires(NOT_TYPING, typing),
                "the leader stole a keystroke from {typing:?}"
            );
        }

        // The chord menu must not re-trigger its own leader.
        assert!(!fires(NOT_TYPING, &["AppRoot", "Transient"]));
    }
}
