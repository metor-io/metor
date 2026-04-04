use std::net::SocketAddr;
use std::sync::Arc;

use gpui::{
    App, Application, Bounds, Context, Entity, IntoElement, Render, SharedString, Window,
    WindowBounds, WindowOptions, div, prelude::*, px, size,
};
use metor_db::{DB, Server};
use metor_panel::elements::TimeSeriesPlot;
use metor_panel::tiles::drag::SplitDirection;
use metor_panel::tiles::item::PaneItem;
use metor_panel::tiles::pane::Pane;
use metor_panel::tiles::TileGroup;
use metor_proto::types::ComponentId;
use stellarator::{net::TcpListener, struc_con::stellar};

/// A simple colored label panel, useful as a placeholder tile.
struct ColorPanel {
    title: SharedString,
    color: gpui::Hsla,
}

impl ColorPanel {
    fn new(title: impl Into<SharedString>, color: gpui::Hsla) -> Self {
        Self {
            title: title.into(),
            color,
        }
    }
}

impl Render for ColorPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(self.color)
            .flex()
            .items_center()
            .justify_center()
            .text_color(gpui::rgb(0xffffff))
            .text_size(px(18.0))
            .child(self.title.clone())
    }
}

impl PaneItem for ColorPanel {
    fn tab_title(&self, _cx: &App) -> SharedString {
        self.title.clone()
    }

    fn serialization_key() -> &'static str {
        "color_panel"
    }

    fn serialize(&self, _cx: &App) -> serde_json::Value {
        serde_json::json!({ "title": self.title.as_ref() })
    }
}

/// A thin wrapper that holds a TimeSeriesPlot and implements PaneItem.
struct PlotPanel {
    plot: Entity<TimeSeriesPlot>,
    label: SharedString,
}

impl PlotPanel {
    fn new(
        db: Arc<DB>,
        component_id: ComponentId,
        elements: Vec<usize>,
        label: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) -> Self {
        let plot = cx.new(|cx| TimeSeriesPlot::from_component(db, component_id, elements, cx));
        Self {
            plot,
            label: label.into(),
        }
    }
}

impl Render for PlotPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.plot.clone())
    }
}

impl PaneItem for PlotPanel {
    fn tab_title(&self, _cx: &App) -> SharedString {
        self.label.clone()
    }

    fn serialization_key() -> &'static str {
        "plot_panel"
    }

    fn serialize(&self, _cx: &App) -> serde_json::Value {
        serde_json::json!({})
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

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
        let bounds = Bounds::centered(None, size(px(1024.), px(600.)), cx);
        let db = db.clone();
        let component_id = args
            .get(1)
            .map(|s| ComponentId::new(s.as_str()));

        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            move |_window, cx| {
                cx.new(|cx| {
                    // Build the tile layout depending on whether a component was given.
                    match component_id {
                        Some(cid) => build_plot_layout(db, cid, cx),
                        None => build_color_layout(cx),
                    }
                })
            },
        )
        .unwrap();
    });
}

/// Layout with colored placeholder panels:
///
/// ┌──────────┬──────────┐
/// │  Panel A  │  Panel B  │
/// │           ├──────────┤
/// │           │  Panel C  │
/// └──────────┴──────────┘
fn build_color_layout(cx: &mut Context<TileGroup>) -> TileGroup {
    use metor_panel::theme::DARK;

    let panel_a: Box<dyn metor_panel::tiles::item::PaneItemHandle> =
        Box::new(cx.new(|_| ColorPanel::new("Panel A", DARK.line_colors[0])));
    let panel_b: Box<dyn metor_panel::tiles::item::PaneItemHandle> =
        Box::new(cx.new(|_| ColorPanel::new("Panel B", DARK.line_colors[1])));
    let panel_c: Box<dyn metor_panel::tiles::item::PaneItemHandle> =
        Box::new(cx.new(|_| ColorPanel::new("Panel C", DARK.line_colors[2])));
    let panel_d: Box<dyn metor_panel::tiles::item::PaneItemHandle> =
        Box::new(cx.new(|_| ColorPanel::new("Panel D", DARK.line_colors[3])));

    // Start with Panel A + Panel D in a single pane (two tabs)
    let mut group = TileGroup::new(vec![panel_a, panel_d], cx);

    // Split right to create a second pane with Panel B
    let left_pane = group.panes()[0].clone();
    let right_pane = cx.new(|cx| Pane::new(vec![panel_b], cx));
    group.split_pane(&left_pane, right_pane, SplitDirection::Right, cx);

    // Split the right pane downward for Panel C
    let right_pane = group.panes()[1].clone();
    let bottom_right_pane = cx.new(|cx| Pane::new(vec![panel_c], cx));
    group.split_pane(&right_pane, bottom_right_pane, SplitDirection::Down, cx);

    group
}

/// Layout with time-series plots for a given component:
///
/// ┌──────────┬──────────┐
/// │  Plot [0] │  Plot [1] │
/// └──────────┴──────────┘
fn build_plot_layout(
    db: Arc<DB>,
    component_id: ComponentId,
    cx: &mut Context<TileGroup>,
) -> TileGroup {
    let plot_a: Box<dyn metor_panel::tiles::item::PaneItemHandle> = Box::new(cx.new(|cx| {
        PlotPanel::new(db.clone(), component_id, vec![0], "Element [0]", cx)
    }));
    let plot_b: Box<dyn metor_panel::tiles::item::PaneItemHandle> = Box::new(cx.new(|cx| {
        PlotPanel::new(db.clone(), component_id, vec![1], "Element [1]", cx)
    }));

    let mut group = TileGroup::new(vec![plot_a], cx);

    let left = group.panes()[0].clone();
    let right = cx.new(|cx| Pane::new(vec![plot_b], cx));
    group.split_pane(&left, right, SplitDirection::Right, cx);

    group
}
