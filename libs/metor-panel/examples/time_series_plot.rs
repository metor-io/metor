use std::net::SocketAddr;
use std::sync::Arc;

use gpui::{
    App, Application, Bounds, Context, Entity, FocusHandle, Focusable, IntoElement, KeyBinding,
    Window, WindowBounds, WindowDecorations, WindowOptions, actions, div, prelude::*, px, size,
};
use metor_db::{DB, Server};
use metor_panel::command_palette::CommandPalette;
use metor_panel::elements::TimeSeriesPlot;
use metor_panel::inspectable::palette_page_for_inspectable;
use metor_proto::types::ComponentId;
use stellarator::{net::TcpListener, struc_con::stellar};

actions!(example, [TogglePalette]);

struct ExampleRoot {
    db: Arc<DB>,
    plot: Entity<TimeSeriesPlot>,
    palette: Option<Entity<CommandPalette>>,
    focus_handle: FocusHandle,
}

impl ExampleRoot {
    fn new(db: Arc<DB>, component_id: ComponentId, cx: &mut Context<Self>) -> Self {
        let plot =
            cx.new(|cx| TimeSeriesPlot::from_component(db.clone(), component_id, &[0], cx));
        Self {
            db,
            plot,
            palette: None,
            focus_handle: cx.focus_handle(),
        }
    }

    fn toggle_palette(&mut self, _: &TogglePalette, window: &mut Window, cx: &mut Context<Self>) {
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
            let page = palette_page_for_inspectable(self.plot.clone(), Some(self.db.clone()), cx);
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
        // Clean up dismissed palette and reclaim focus
        if let Some(palette) = &self.palette {
            if palette.read(cx).dismissed {
                self.palette = None;
                self.focus_handle.focus(window);
            }
        }

        let mut root = div()
            .id("example-root")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::toggle_palette))
            .size_full()
            .child(self.plot.clone());

        if let Some(palette) = &self.palette {
            root = root.child(palette.clone());
        }

        root
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {} <component_name>", args[0]);
        std::process::exit(1);
    }
    let component_id = ComponentId::new(args[1].as_str());
    let tmp = std::env::temp_dir().join("metor_panel_example");
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
        cx.set_global(metor_panel::theme::ActiveTheme(
            std::sync::Arc::new(metor_panel::theme::DARK.clone()),
        ));
        cx.bind_keys([KeyBinding::new("cmd-p", TogglePalette, None)]);

        let bounds = Bounds::centered(None, size(px(800.), px(400.)), cx);
        let db = db.clone();
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_window, cx| cx.new(|cx| ExampleRoot::new(db, component_id, cx)),
        )
        .unwrap();
    });
}
