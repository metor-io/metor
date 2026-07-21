#![allow(unexpected_cfgs)]

use std::net::SocketAddr;
use std::sync::Arc;

use crate::connections::{AddressResolver, ConnectionTarget, RegistryHandle, TargetId};
use crate::icons::Icon;
use crate::inspector::Inspector;
use crate::inspector::edits::{
    self, edit_value_rows, pending_edits, pending_edits_mut, review_rows,
};
use crate::inspector::palette::{Category, InspectionItem, ItemProvider, ItemRegistry};
use crate::inspector::rows::InspectorRow;
use crate::inspector::{InspectorMode, InspectorRequest, OpenInspectorGlobal};
use crate::tiles::panels::{
    AlarmPanel, BrowserPanel, DataTablePanel, ListPlotPanel, LogPanel, PlotPanel,
    SequenceGridPanel, SequencePanel, TablePanel, TextPanel, TrafficLightGridPanel,
    TrafficLightPanel, Viewer3dPanel, XyPlotPanel,
};
use crate::tiles::{PlotComponentAction, PreviewPlotAction, TileGroup, TileGroupEvent};
use crate::views::dashboard::{DashboardPanel, deserialize_dashboard};
use crate::views::time_series::time_range::GlobalTimeRange;
use gpui::{
    App, Application, Bounds, Context, Entity, FocusHandle, Focusable, IntoElement, KeyBinding,
    Pixels, Point, Render, SharedString, TitlebarOptions, Window, WindowBounds, WindowOptions,
    actions, div, point, prelude::*, px, size,
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
struct AppRoot {
    db: Arc<DB>,
    tiles: Entity<TileGroup>,
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
    fn new(db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let tiles = cx.new(|cx| TileGroup::new(vec![], cx));
        cx.subscribe(&tiles, Self::handle_tile_event).detach();
        let on_open_inspector = Self::make_on_open_inspector(cx.entity().clone());
        cx.set_global(OpenInspectorGlobal(on_open_inspector.clone()));
        crate::inspector::palette::register_builtin_providers(
            db.clone(),
            tiles.clone(),
            on_open_inspector,
            cx,
        );
        crate::node_editor::palette_provider::register(tiles.clone(), cx);
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
            store.update(cx, |store, _| store.set_tiles(tiles.downgrade()));
            if store.read(cx).active().is_empty() {
                pending_connection_picker = Some(true);
            }
        }
        Self {
            db,
            tiles,
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

    fn make_on_open_inspector(root: Entity<AppRoot>) -> crate::inspector::OpenInspectorCallback {
        Arc::new(move |request, _window, cx| {
            root.update(cx, |this, cx| {
                this.pending_inspector_open = Some(request);
                cx.notify();
            });
        })
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
            let parent_focus = self.focus_handle.clone();
            let inspector = cx.new(|cx| {
                let mut insp = Inspector::new(rows, InspectorMode::Anchored(position), cx);
                insp.set_parent_focus(parent_focus);
                insp
            });
            inspector.focus_handle(cx).focus(window);
            self.inspector = Some(inspector);
            cx.notify();
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
        // Each is an opaque view the consumer built at startup (e.g. a modal that
        // renders nothing until its own state says otherwise).
        if let Some(overlays) = cx.try_global::<OverlayViews>() {
            for view in &overlays.0 {
                root = root.child(view.clone());
            }
        }

        crate::window_controls::client_side_decorations(root, window, cx)
    }
}

/// Consumer-supplied root overlays, installed via [`PanelApp::overlay`] and read
/// by [`AppRoot::render`]. Held in a global because overlays are built before the
/// window (and thus `AppRoot`) exists.
struct OverlayViews(Vec<gpui::AnyView>);

impl gpui::Global for OverlayViews {}

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
            let active = state.active_count();
            if active == 0 {
                bar = bar.child(Icon::Dot.svg_color(7.0, theme.control_active));
            } else {
                if let Some(severity) = state.highest_active_severity() {
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
                    "{active} active, {} unacked",
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

        // Global time range: the window every Auto-range plot follows;
        // clicking opens a preset/custom picker.
        let range_label = SharedString::from(format!("{}", GlobalTimeRange::get(cx)));
        right = right.child(
            Self::titlebar_segment(theme, "global-time-range")
                .child(range_label)
                .child(crate::icons::Icon::ChevronDown.svg_color(10.0, theme.text_secondary))
                .on_mouse_down(gpui::MouseButton::Left, |event, window, cx| {
                    let Some(open) = crate::inspector::open_inspector(cx) else {
                        return;
                    };
                    open(
                        InspectorRequest {
                            rows: global_time_range_rows(cx),
                            mode: InspectorMode::Anchored(event.position),
                        },
                        window,
                        cx,
                    );
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

/// Picker rows for the global time range: every preset plus a freeform
/// text field using `TimeRangeBehavior`'s string grammar.
fn global_time_range_rows(cx: &gpui::App) -> Vec<Box<dyn InspectorRow>> {
    crate::views::time_series::time_range::picker_rows(
        SharedString::from(format!("{}", GlobalTimeRange::get(cx))),
        Arc::new(|behavior, _window, cx| GlobalTimeRange::set(cx, behavior)),
    )
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
    command_providers: Vec<(Category, ItemProvider)>,
    init_hooks: Vec<Box<dyn FnOnce(&mut App)>>,
    overlays: Vec<Box<dyn FnOnce(&mut App) -> gpui::AnyView>>,
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
            command_providers: Vec::new(),
            init_hooks: Vec::new(),
            overlays: Vec::new(),
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

    /// Mount a consumer-built view over the window root. `build` runs at startup
    /// (with `&mut App`, after the built-in registries exist) and returns any
    /// [`Render`] entity as an [`AnyView`](gpui::AnyView); it is drawn above the
    /// tile tree and inspector every frame. The consumer owns the view's state
    /// and updates — a modal that renders nothing until shown, say. Call more
    /// than once to stack overlays in insertion order.
    pub fn overlay<V, F>(mut self, build: F) -> Self
    where
        V: Render,
        F: FnOnce(&mut App) -> Entity<V> + 'static,
    {
        self.overlays.push(Box::new(move |cx| build(cx).into()));
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

    /// Boot the gpui application and block until the last window closes.
    ///
    /// Registers the theme, fonts, icons, edit queue, palette providers,
    /// inspector registry, and widget registry, drains any consumer-supplied
    /// command providers and init hooks, then opens the root window.
    pub fn run(self) {
        let PanelApp {
            db,
            server_addr,
            targets,
            connection_sources,
            auto_connect,
            address_resolver,
            command_providers,
            init_hooks,
            overlays,
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

        let mut targets = Some(targets);
        let mut connection_sources = Some(connection_sources);
        let mut auto_connect = Some(auto_connect);
        let mut address_resolver = Some(address_resolver);
        let mut command_providers = Some(command_providers);
        let mut init_hooks = Some(init_hooks);
        let mut overlays = Some(overlays);
        Application::new()
            .with_assets(crate::icons::IconAssets)
            .run(move |cx: &mut App| {
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
                ItemRegistry::init(cx);
                crate::inspector::registry::InspectorRegistry::init(db.clone(), cx);
                crate::views::dashboard::WidgetRegistry::init(cx);
                crate::dynamic::DynamicRegistry::init(cx);
                crate::node_editor::GraphCoordinator::init(cx);
                crate::node_editor::DynamicWorker::init(cx);
                crate::node_editor::inspector_rows::register_inspector_rows(cx);
                crate::views::system_graph::inspector_rows::register_inspector_rows(cx);
                crate::alarms::AlarmStore::init(db.clone(), cx);
                crate::logs::LogStore::init(db.clone(), cx);
                crate::sequences::SequenceStore::init(db.clone(), cx);
                crate::plot_events::EventSourceRegistry::init(cx);
                crate::wiring::WiringStore::init(db.clone(), cx);
                register_pane_item_deserializers(db.clone(), cx);

                // Connections: seed builder-registered targets, hand the
                // registry to discovery sources, then fire auto-connects.
                // LoD spawning is a store concern — it starts with the first
                // local-authority connection rather than at boot.
                let registry = crate::connections::ConnectionsStore::init(db.clone(), cx);
                if let Some(store) = crate::connections::try_global(cx) {
                    store.update(cx, |store, cx| {
                        if let Some(resolver) = address_resolver.take().flatten() {
                            store.set_resolver(resolver);
                        }
                        for target in targets.take().unwrap_or_default() {
                            store.upsert_target(target, cx);
                        }
                        for id in auto_connect.take().unwrap_or_default() {
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
                for source in connection_sources.take().unwrap_or_default() {
                    source(registry.clone());
                }
                register_connection_commands(cx);

                // Consumer extensions: register custom palette providers, run
                // init hooks, then build overlays. All happen after the built-in
                // registries exist so they can call into any of them; overlays
                // run last so they can rely on anything a hook installed.
                for (category, provider) in command_providers.take().unwrap_or_default() {
                    ItemRegistry::register(cx, category, provider);
                }
                for hook in init_hooks.take().unwrap_or_default() {
                    hook(cx);
                }
                let built: Vec<gpui::AnyView> = overlays
                    .take()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|build| build(cx))
                    .collect();
                cx.set_global(OverlayViews(built));

                set_dock_icon();
                // `secondary-` resolves to cmd on macOS and ctrl elsewhere
                // (`cmd-` would be the Win/Super key off-macOS).
                cx.bind_keys([
                    KeyBinding::new("secondary-p", OpenPalette, None),
                    // The leader opens the transient chord menu. Suppressed while
                    // a text field (Inspector search, node-editor inline edit via
                    // RowList) or the menu itself holds focus, so the key still
                    // types normally there.
                    KeyBinding::new(
                        leader.as_str(),
                        OpenLeader,
                        Some("!Inspector && !RowList && !Transient"),
                    ),
                    KeyBinding::new("ctrl-tab", CycleTabForward, None),
                    KeyBinding::new("shift-ctrl-tab", CycleTabBackward, None),
                    KeyBinding::new("secondary-l", ToggleCmdLock, None),
                    KeyBinding::new("secondary-shift-e", OpenReviewEdits, None),
                    // Excluded when a `RowList` is focused so editing a node's
                    // inline arg field (typing Backspace, hitting Delete) doesn't
                    // also delete the surrounding node.
                    KeyBinding::new(
                        "delete",
                        crate::node_editor::DeleteSelected,
                        Some("NodeEditor && !RowList"),
                    ),
                    KeyBinding::new(
                        "backspace",
                        crate::node_editor::DeleteSelected,
                        Some("NodeEditor && !RowList"),
                    ),
                ]);

                let bounds = Bounds::centered(None, size(px(1024.), px(600.)), cx);
                let db = db.clone();
                cx.open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(bounds)),
                        // `appears_transparent` hides the native titlebar on
                        // macOS (leaving the traffic lights) and on Windows
                        // (leaving nothing — the app draws its own controls).
                        titlebar: Some(TitlebarOptions {
                            appears_transparent: true,
                            traffic_light_position: Some(point(px(12.0), px(8.0))),
                            ..Default::default()
                        }),
                        // Linux-only: request client-side decorations; the
                        // window root wraps itself in resize borders and a
                        // shadow via `window_controls::client_side_decorations`.
                        window_decorations: Some(gpui::WindowDecorations::Client),
                        app_id: Some("metor-panel".into()),
                        ..Default::default()
                    },
                    move |_window, cx| cx.new(|cx| AppRoot::new(db, cx)),
                )
                .unwrap();
            });
    }
}

/// Palette entries for the connection system: "Connect…" opens the picker,
/// and each active connection contributes a "Disconnect <name>" command.
/// Pull-based like every provider, so the disconnect list always matches
/// the live set.
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
            if let Some(store) = crate::connections::try_global(cx) {
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
            }
            items
        }),
    );
}

/// Populate the pane-item registry so [`TileGroup::from_json`] can rehydrate
/// every built-in panel kind. One closure per kind, parsing the kind's
/// `*Config` blob with `facet-json` and constructing the panel via its
/// `from_config` constructor (or, for [`DashboardPanel`], the dedicated
/// `deserialize_dashboard` helper). The populated registry is installed as a
/// gpui `Global` so palette callbacks can fetch it without threading.
fn register_pane_item_deserializers(db: Arc<DB>, cx: &mut App) {
    use crate::tiles::ItemRegistry as PaneItemRegistry;
    let mut reg = PaneItemRegistry::default();

    register_panel::<TextPanel>(&mut reg, db.clone(), TextPanel::from_config);
    register_panel::<AlarmPanel>(&mut reg, db.clone(), AlarmPanel::from_config);
    register_panel::<LogPanel>(&mut reg, db.clone(), LogPanel::from_config);
    register_panel::<SequencePanel>(&mut reg, db.clone(), SequencePanel::from_config);
    register_panel::<SequenceGridPanel>(&mut reg, db.clone(), SequenceGridPanel::from_config);
    register_panel::<TablePanel>(&mut reg, db.clone(), TablePanel::from_config);
    register_panel::<DataTablePanel>(&mut reg, db.clone(), DataTablePanel::from_config);
    register_panel::<BrowserPanel>(&mut reg, db.clone(), BrowserPanel::from_config);
    register_panel::<PlotPanel>(&mut reg, db.clone(), PlotPanel::from_config);
    register_panel::<XyPlotPanel>(&mut reg, db.clone(), XyPlotPanel::from_config);
    register_panel::<ListPlotPanel>(&mut reg, db.clone(), ListPlotPanel::from_config);
    register_panel::<Viewer3dPanel>(&mut reg, db.clone(), Viewer3dPanel::from_config);
    register_panel::<TrafficLightPanel>(&mut reg, db.clone(), TrafficLightPanel::from_config);
    register_panel::<TrafficLightGridPanel>(
        &mut reg,
        db.clone(),
        TrafficLightGridPanel::from_config,
    );
    register_panel::<crate::node_editor::pane::NodeEditor>(
        &mut reg,
        db.clone(),
        crate::node_editor::pane::NodeEditor::from_config,
    );
    register_panel::<crate::views::system_graph::SystemGraphPanel>(
        &mut reg,
        db.clone(),
        crate::views::system_graph::SystemGraphPanel::from_config,
    );

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
        let cfg: T::Config = facet_json::from_str(state).unwrap_or_default();
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
