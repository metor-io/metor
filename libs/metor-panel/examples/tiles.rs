use std::net::SocketAddr;
use std::sync::Arc;

use gpui::{
    actions, App, Application, Bounds, Context, Entity, FocusHandle, Focusable, IntoElement,
    KeyBinding, Render, Window, WindowBounds, WindowOptions, div, prelude::*, px, size,
};
use metor_db::{DB, Server};
use metor_panel::command_palette::CommandPalette;
use metor_panel::tiles::pane::Pane;
use metor_panel::tiles::panels::new_panel_palette_page;
use metor_panel::tiles::TileGroup;
use stellarator::{net::TcpListener, struc_con::stellar};

actions!(tiles_example, [NewPanel]);

struct ExampleRoot {
    db: Arc<DB>,
    tiles: Entity<TileGroup>,
    palette: Option<Entity<CommandPalette>>,
    focus_handle: FocusHandle,
}

impl ExampleRoot {
    fn new(db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let tiles = cx.new(|cx| {
            // Start with a single empty pane
            TileGroup::new(vec![], cx)
        });
        Self {
            db,
            tiles,
            palette: None,
            focus_handle: cx.focus_handle(),
        }
    }

    fn new_panel(&mut self, _: &NewPanel, window: &mut Window, cx: &mut Context<Self>) {
        // Clean up dismissed palette
        if let Some(palette) = &self.palette {
            if palette.read(cx).dismissed {
                self.palette = None;
            }
        }

        if self.palette.is_some() {
            self.palette = None;
            self.focus_handle.focus(window);
        } else {
            // Use the first pane as the target for new panels
            let pane = self.tiles.read(cx).panes()[0].clone();
            let page = new_panel_palette_page(self.db.clone(), pane);
            let parent_focus = self.focus_handle.clone();
            let palette = cx.new(|cx| {
                let mut p = CommandPalette::new(page, cx);
                p.set_parent_focus(parent_focus);
                p
            });
            palette.focus_handle(cx).focus(window);
            self.palette = Some(palette);
        }
        cx.notify();
    }
}

impl Focusable for ExampleRoot {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ExampleRoot {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Clean up dismissed palette
        if let Some(palette) = &self.palette {
            if palette.read(cx).dismissed {
                self.palette = None;
                self.focus_handle.focus(window);
            }
        }

        let mut root = div()
            .id("tiles-root")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::new_panel))
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

    Application::new().run(move |cx: &mut App| {
        cx.bind_keys([KeyBinding::new("cmd-p", NewPanel, None)]);

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
