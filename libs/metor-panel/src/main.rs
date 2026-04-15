#![allow(unexpected_cfgs)]

use std::net::SocketAddr;
use std::sync::Arc;

use gpui::{
    App, Application, Bounds, Context, Entity, FocusHandle, Focusable, IntoElement, KeyBinding,
    Pixels, Point, Render, SharedString, TitlebarOptions, Window, WindowBounds, WindowOptions,
    actions, div, point, prelude::*, px, size,
};
use metor_db::{DB, Server};
use metor_panel::inspector::Inspector;
use metor_panel::inspector::{InspectorMode, InspectorRequest};
use metor_panel::items::ItemRegistry;
use metor_panel::pending_edits::{
    self, edit_value_rows, pending_edits, pending_edits_mut, review_rows,
};
use metor_panel::tiles::{TileGroup, TileGroupEvent};
use stellarator::{net::TcpListener, struc_con::stellar};

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

struct AppRoot {
    db: Arc<DB>,
    tiles: Entity<TileGroup>,
    inspector: Option<Entity<Inspector>>,
    pending_inspector_request: Option<(Box<dyn metor_panel::tiles::PaneItemHandle>, Point<Pixels>)>,
    pending_inspector_open: Option<InspectorRequest>,
    focus_handle: FocusHandle,
}

impl AppRoot {
    fn new(db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let tiles = cx.new(|cx| TileGroup::new(vec![], cx));
        cx.subscribe(&tiles, Self::handle_tile_event).detach();
        let on_open_inspector = Self::make_on_open_inspector(cx.entity().clone());
        metor_panel::items::register_builtin_providers(
            db.clone(),
            tiles.clone(),
            on_open_inspector,
            cx,
        );
        Self {
            db,
            tiles,
            inspector: None,
            pending_inspector_request: None,
            pending_inspector_open: None,
            focus_handle: cx.focus_handle(),
        }
    }

    fn open_inspector_with(
        &mut self,
        rows: Vec<Box<dyn metor_panel::widgets::InspectorRow>>,
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

    fn make_on_open_inspector(
        root: Entity<AppRoot>,
    ) -> metor_panel::inspector::OpenInspectorCallback {
        Arc::new(move |request, _window, cx| {
            root.update(cx, |this, cx| {
                this.pending_inspector_open = Some(request);
                cx.notify();
            });
        })
    }

    fn toggle_palette(&mut self, _: &OpenPalette, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(inspector) = &self.inspector {
            if !inspector.read(cx).dismissed {
                self.inspector = None;
                self.focus_handle.focus(window);
                cx.notify();
                return;
            }
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
        item: &dyn metor_panel::tiles::PaneItemHandle,
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
        if let Some(rows) = metor_panel::reflect::rows_for_any_entity(entity, &self.db, cx) {
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
        action: &metor_panel::inspector::InspectEntity,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_entity_inspector(&action.entity, action.position, window, cx)
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
        if let Some(inspector) = &self.inspector {
            if inspector.read(cx).dismissed {
                self.inspector = None;
                self.focus_handle.focus(window);
            }
        }

        if let Some((item, position)) = self.pending_inspector_request.take() {
            self.open_inspector(&*item, position, window, cx);
        }

        if let Some(request) = self.pending_inspector_open.take() {
            self.open_inspector_with(request.rows, request.mode, window, cx);
        }

        if let Some(request) = pending_edits_mut(cx).pending_request.take() {
            let rows = edit_value_rows(self.db.clone(), request);
            self.open_inspector_with(rows, InspectorMode::Centered, window, cx);
        }

        if pending_edits(cx).open_review_requested {
            pending_edits_mut(cx).open_review_requested = false;
            let rows = review_rows(self.db.clone(), cx);
            self.open_inspector_with(rows, InspectorMode::Centered, window, cx);
        }

        let theme = metor_panel::theme::theme(cx);
        let titlebar = self.render_titlebar(&theme, cx);

        let mut root = div()
            .id("app-root")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::toggle_palette))
            .on_action(cx.listener(Self::cycle_tab_forward))
            .on_action(cx.listener(Self::cycle_tab_backward))
            .on_action(cx.listener(Self::toggle_cmd_lock))
            .on_action(cx.listener(Self::handle_inspect_entity))
            .on_action(cx.listener(Self::open_review_edits))
            .font_family(theme.font_family)
            .flex()
            .flex_col()
            .size_full()
            .child(titlebar)
            .child(div().flex_1().min_h_0().child(self.tiles.clone()));

        if let Some(inspector) = &self.inspector {
            root = root.child(inspector.clone());
        }

        root
    }
}

impl AppRoot {
    fn render_titlebar(&self, theme: &metor_panel::theme::Theme, cx: &App) -> impl IntoElement {
        let pending = pending_edits(cx);
        let edit_count = pending.edits.len();
        let locked = pending.locked;

        let icon = if locked {
            metor_panel::icons::Icon::Lock
        } else {
            metor_panel::icons::Icon::LockOpen
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
            .gap(px(6.0))
            .pr(px(8.0));

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
    }
}

fn main() {
    let tmp = std::env::temp_dir().join("metor_panel");
    let db = Arc::new(DB::create(tmp).unwrap());
    let server_db = db.clone();
    stellar(move || async move {
        let server = Server {
            listener: TcpListener::bind(SocketAddr::new([127, 0, 0, 1].into(), 2240)).unwrap(),
            db: server_db,
        };
        server.run().await
    });

    Application::new()
        .with_assets(metor_panel::icons::IconAssets)
        .run(move |cx: &mut App| {
            metor_panel::theme::register_fonts(cx);
            cx.set_global(metor_panel::theme::ActiveTheme(Arc::new(
                metor_panel::theme::DARK.clone(),
            )));
            pending_edits::init(cx);
            ItemRegistry::init(cx);
            metor_panel::widget_registry::WidgetRegistry::init(db.clone(), cx);
            set_dock_icon();
            cx.bind_keys([
                KeyBinding::new("cmd-p", OpenPalette, None),
                KeyBinding::new("ctrl-tab", CycleTabForward, None),
                KeyBinding::new("shift-ctrl-tab", CycleTabBackward, None),
                KeyBinding::new("cmd-l", ToggleCmdLock, None),
                KeyBinding::new("cmd-shift-e", OpenReviewEdits, None),
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

#[cfg(target_os = "macos")]
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
