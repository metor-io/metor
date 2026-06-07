#![allow(unexpected_cfgs)]

use std::sync::Arc;

use crate::inspector::Inspector;
use crate::inspector::edits::{
    self, edit_value_rows, pending_edits, pending_edits_mut, review_rows,
};
use crate::inspector::palette::ItemRegistry;
use crate::inspector::{InspectorMode, InspectorRequest, OpenInspectorGlobal};
use crate::tiles::panels::{
    AlarmPanel, BrowserPanel, DataTablePanel, ListPlotPanel, PlotPanel, TablePanel, TextPanel,
    TrafficLightGridPanel, TrafficLightPanel, Viewer3dPanel, XyPlotPanel,
};
use crate::tiles::{PlotComponentAction, PreviewPlotAction, TileGroup, TileGroupEvent};
use crate::views::dashboard::{DashboardPanel, deserialize_dashboard};
use gpui::{
    App, Application, Bounds, Context, Entity, FocusHandle, Focusable, IntoElement, KeyBinding,
    Pixels, Point, Render, SharedString, TitlebarOptions, Window, WindowBounds, WindowOptions,
    actions, div, point, prelude::*, px, size,
};
use metor_db::DB;

actions!(
    metor_panel,
    [
        OpenPalette,
        CycleTabForward,
        CycleTabBackward,
        ToggleCmdLock,
        OpenReviewEdits,
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
        Self {
            db,
            tiles,
            inspector: None,
            hover_preview: None,
            pending_inspector_request: None,
            pending_pane_inspector_request: None,
            pending_inspector_open: None,
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

    fn cycle_tab_forward(
        &mut self,
        _: &CycleTabForward,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pane = self.tiles.read(cx).panes()[0].clone();
        pane.update(cx, |pane, cx| pane.cycle_forward(cx));
    }

    fn cycle_tab_backward(
        &mut self,
        _: &CycleTabBackward,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pane = self.tiles.read(cx).panes()[0].clone();
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
            println!("no rows for entity");
        }
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
        let Some(pane) = self.tiles.read(cx).panes().first().cloned() else {
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

        if let Some((item, position)) = self.pending_inspector_request.take() {
            self.open_inspector(&*item, position, window, cx);
        }

        if let Some((pane, position)) = self.pending_pane_inspector_request.take() {
            self.open_entity_inspector(&pane.into_any(), position, window, cx);
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
        let titlebar = self.render_titlebar(&theme, cx);

        let mut root = div()
            .id("app-root")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::toggle_palette))
            .on_action(cx.listener(Self::cycle_tab_forward))
            .on_action(cx.listener(Self::cycle_tab_backward))
            .on_action(cx.listener(Self::toggle_cmd_lock))
            .on_action(cx.listener(Self::handle_inspect_entity))
            .on_action(cx.listener(Self::handle_plot_component_action))
            .on_action(cx.listener(Self::handle_preview_plot_action))
            .on_action(cx.listener(Self::open_review_edits))
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
            .child(titlebar)
            .child(div().flex_1().min_h_0().child(self.tiles.clone()));

        if let Some(inspector) = &self.inspector {
            root = root.child(inspector.clone());
        }

        if let Some(preview) = &self.hover_preview {
            root = root.child(preview.inspector.clone());
        }

        root
    }
}

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
                // Healthy: a bare green dot keeps the title bar compact.
                bar = bar.child(div().text_color(theme.control_active).child("\u{25cf}"));
            } else {
                if let Some(severity) = state.highest_active_severity() {
                    let idx = crate::alarms::severity_index(severity);
                    bar = bar.child(div().text_color(theme.alarm_color(idx)).child("\u{25cf}"));
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

    /// Open the alarm panel in the first pane, unless one is already open.
    fn open_alarms(&mut self, cx: &mut Context<Self>) {
        let panes = self.tiles.read(cx).panes().to_vec();
        let already_open = panes.iter().any(|pane| {
            pane.read(cx)
                .items()
                .iter()
                .any(|item| item.serialization_key() == "alarm")
        });
        if already_open {
            return;
        }
        let Some(pane) = panes.first().cloned() else {
            return;
        };
        let db = self.db.clone();
        pane.update(cx, |pane, cx| {
            let item: Box<dyn crate::tiles::PaneItemHandle> =
                Box::new(cx.new(|cx| AlarmPanel::new(db, cx)));
            pane.add_item(item, cx);
        });
    }

    fn render_titlebar(
        &self,
        theme: &crate::theme::Theme,
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

        let mut right = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .pr(px(8.0));

        right = right.child(self.render_alarm_summary(theme, cx));

        if edit_count > 0 {
            let label = SharedString::from(format!("{} pending", edit_count));
            right = right.child(
                div()
                    .id("pending-pill")
                    .px(px(8.0))
                    .py(px(2.0))
                    .bg(theme.pill_bg)
                    .border_1()
                    .border_color(theme.pill_border)
                    .rounded(px(4.0))
                    .text_size(px(12.0))
                    .text_color(theme.text_primary)
                    .child(label)
                    .cursor_pointer()
                    .on_mouse_down(gpui::MouseButton::Left, |_, _window, cx| {
                        pending_edits_mut(cx).open_review_requested = true;
                        cx.refresh_windows();
                    }),
            );
        }

        right = right.child(lock_button);

        div()
            .bg(theme.bg_secondary)
            .border_b_1()
            .border_color(theme.border_primary)
            .w_full()
            .flex_shrink_0()
            .h(px(TITLEBAR_HEIGHT))
            .flex()
            .flex_row()
            .items_center()
            .justify_end()
            .child(right)
            .into_any_element()
    }
}

/// Boot the gpui application and block until the last window closes.
///
/// Registers the theme, fonts, icons, edit queue, palette providers,
/// inspector registry, and widget registry before opening the root window.
pub fn run(db: Arc<metor_db::DB>) {
    Application::new()
        .with_assets(crate::icons::IconAssets)
        .run(move |cx: &mut App| {
            crate::theme::register_fonts(cx);
            let cfg = crate::config::load();
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
            crate::alarms::AlarmStore::init(db.clone(), cx);
            register_pane_item_deserializers(db.clone(), cx);
            set_dock_icon();
            cx.bind_keys([
                KeyBinding::new("cmd-p", OpenPalette, None),
                KeyBinding::new("ctrl-tab", CycleTabForward, None),
                KeyBinding::new("shift-ctrl-tab", CycleTabBackward, None),
                KeyBinding::new("cmd-l", ToggleCmdLock, None),
                KeyBinding::new("cmd-shift-e", OpenReviewEdits, None),
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
                    titlebar: Some(TitlebarOptions {
                        appears_transparent: true,
                        traffic_light_position: Some(point(px(12.0), px(8.0))),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                move |_window, cx| cx.new(|cx| AppRoot::new(db, cx)),
            )
            .unwrap();
        });
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
