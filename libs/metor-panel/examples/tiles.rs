use std::net::SocketAddr;
use std::sync::Arc;

use gpui::{
    App, Application, Bounds, Context, Entity, FocusHandle, Focusable, IntoElement, KeyBinding,
    Render, Window, WindowBounds, WindowOptions, actions, div, prelude::*, px, size,
};
use metor_db::{DB, Server};
use metor_panel::command_palette::{CommandPalette, PalettePage};
use metor_panel::tiles::panels::tile_palette_page;
use metor_panel::tiles::{TileGroup, TileGroupEvent};
use stellarator::{net::TcpListener, struc_con::stellar};

actions!(tiles_example, [OpenPalette]);

struct ExampleRoot {
    db: Arc<DB>,
    tiles: Entity<TileGroup>,
    palette: Option<Entity<CommandPalette>>,
    pending_inspect: Option<PalettePage>,
    focus_handle: FocusHandle,
}

impl ExampleRoot {
    fn new(db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let tiles = cx.new(|cx| TileGroup::new(vec![], cx));
        cx.subscribe(&tiles, Self::handle_tile_event).detach();
        Self {
            db,
            tiles,
            palette: None,
            pending_inspect: None,
            focus_handle: cx.focus_handle(),
        }
    }

    fn open_palette(&mut self, page: PalettePage, window: &mut Window, cx: &mut Context<Self>) {
        let parent_focus = self.focus_handle.clone();
        let palette = cx.new(|cx| {
            let mut p = CommandPalette::new(page, cx);
            p.set_parent_focus(parent_focus);
            p
        });
        palette.focus_handle(cx).focus(window);
        self.palette = Some(palette);
        cx.notify();
    }

    fn make_on_inspect(
        root: Entity<ExampleRoot>,
    ) -> impl Fn(PalettePage, &mut Window, &mut App) + 'static {
        move |page, _window, cx| {
            root.update(cx, |this, cx| {
                this.pending_inspect = Some(page);
                cx.notify();
            });
        }
    }

    fn toggle_palette(&mut self, _: &OpenPalette, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(palette) = &self.palette {
            if palette.read(cx).dismissed {
                self.palette = None;
            }
        }

        if self.palette.is_some() {
            self.palette = None;
            self.focus_handle.focus(window);
            cx.notify();
        } else {
            let pane = self.tiles.read(cx).panes()[0].clone();
            let root = cx.entity().clone();
            let page = tile_palette_page(
                self.db.clone(),
                pane,
                &self.tiles,
                Self::make_on_inspect(root),
                cx,
            );
            self.open_palette(page, window, cx);
        }
    }

    fn handle_tile_event(
        &mut self,
        _tiles: Entity<TileGroup>,
        event: &TileGroupEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            TileGroupEvent::Inspect { item } => {
                if let Some(page) = item.inspect_page(Some(self.db.clone()), cx) {
                    self.pending_inspect = Some(page);
                    cx.notify();
                }
            }
        }
    }
}

impl Focusable for ExampleRoot {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ExampleRoot {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(palette) = &self.palette {
            if palette.read(cx).dismissed {
                self.palette = None;
                self.focus_handle.focus(window);
            }
        }

        if let Some(page) = self.pending_inspect.take() {
            self.open_palette(page, window, cx);
        }

        let mut root = div()
            .id("tiles-root")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::toggle_palette))
            .font_family(metor_panel::theme::DARK.font_family)
            .size_full()
            .child(self.tiles.clone());

        if let Some(palette) = &self.palette {
            root = root.child(palette.clone());
        }

        root
    }
}

fn main() {
    let tmp = std::env::temp_dir().join("metor_tiles_example");
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
            cx.bind_keys([KeyBinding::new("cmd-p", OpenPalette, None)]);

            let bounds = Bounds::centered(None, size(px(1024.), px(600.)), cx);
            let db = db.clone();
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                move |_window, cx| cx.new(|cx| ExampleRoot::new(db, cx)),
            )
            .unwrap();
        });
}
