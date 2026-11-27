//! A gallery of metor-gpui elements that runs in the browser: a sidebar of side tabs, a table,
//! a graph, a Gantt chart, a banner and a modal.
//!
//! Build it for the web with `just gpui-gallery` and serve `target/web_gallery`, or run it
//! natively with `cargo run -p metor-gpui --example web_gallery`.

#![cfg_attr(target_family = "wasm", no_main)]

use gpui::{
    AnyElement, App, Bounds, Context, Entity, SharedString, Window, WindowBounds, WindowOptions,
    div, prelude::*, px, size,
};
use metor_gpui::gantt::{Gantt, GanttBar, GanttLane, GanttModel};
use metor_gpui::graph::{EdgeStyle, GraphEdge, GraphModel, GraphNode, GraphView};
use metor_gpui::{
    Banner, Column, ColumnSort, Icon, IconAssets, Modal, StateTone, Table, TableDelegate,
    font_family, side_tab, side_tabs, theme,
};
use smallvec::smallvec;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Jobs,
    Graph,
    Timeline,
}

const JOBS: [(&str, StateTone, &str); 6] = [
    ("deploy-site", StateTone::Active, "1m 12s"),
    ("build-firmware", StateTone::Success, "4m 03s"),
    ("hil-tests", StateTone::Failure, "12m 40s"),
    ("lint", StateTone::Success, "0m 21s"),
    ("nightly-sim", StateTone::Queued, ""),
    ("docs", StateTone::Cancelled, "0m 05s"),
];

struct Jobs;

impl TableDelegate for Jobs {
    fn columns(&self) -> Vec<Column> {
        vec![
            Column::new("State", 110.0),
            Column::new("Name", 200.0).sortable().flex(),
            Column::new("Duration", 120.0),
        ]
    }

    fn rows_count(&self) -> usize {
        JOBS.len()
    }

    fn row_height(&self) -> gpui::Pixels {
        px(30.0)
    }

    fn render_cell(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<Table<Self>>,
    ) -> AnyElement {
        let theme = theme(cx);
        let (name, tone, duration) = JOBS[row_ix];
        let cell = div()
            .px(px(12.0))
            .h_full()
            .flex()
            .items_center()
            .gap(px(6.0));
        match col_ix {
            0 => cell
                .child(
                    div()
                        .size(px(8.0))
                        .rounded_full()
                        .bg(theme.state_color(tone)),
                )
                .child(format!("{tone:?}")),
            1 => cell.text_color(theme.text_primary).child(name),
            _ => cell.child(duration),
        }
        .into_any_element()
    }

    fn sort_column(&mut self, _col_ix: usize, _sort: ColumnSort, _cx: &mut Context<Table<Self>>) {}
}

fn graph_model() -> GraphModel {
    let tasks = [
        "checkout", "deps", "build", "unit", "lint", "package", "hil", "deploy",
    ];
    let edges = [
        (0, 1),
        (1, 2),
        (1, 4),
        (2, 3),
        (2, 5),
        (3, 6),
        (5, 6),
        (6, 7),
        (4, 7),
    ];
    let tones = [
        StateTone::Success,
        StateTone::Success,
        StateTone::Success,
        StateTone::Success,
        StateTone::Success,
        StateTone::Active,
        StateTone::Queued,
        StateTone::Idle,
    ];
    GraphModel {
        nodes: tasks
            .iter()
            .enumerate()
            .map(|(id, name)| GraphNode {
                state: Some(tones[id]),
                detail: Some(format!("{}s", 10 + id * 7).into()),
                ..GraphNode::new(id as u64, *name)
            })
            .collect(),
        edges: edges
            .iter()
            .map(|&(from, to)| GraphEdge {
                from,
                to,
                style: EdgeStyle::Solid,
            })
            .collect(),
    }
}

fn gantt_model() -> GanttModel {
    let now = web_time::SystemTime::now()
        .duration_since(web_time::UNIX_EPOCH)
        .map_or(0, |d| d.as_micros() as i64);
    let second = 1_000_000;
    let lane = |id: u64, label: &str, queued: i64, start: i64, end: Option<i64>, tone| GanttLane {
        id,
        label: SharedString::from(label.to_string()),
        bars: smallvec![
            GanttBar {
                start: now - queued * second,
                end: Some(now - start * second),
                tone: StateTone::Queued,
                faint: true,
            },
            GanttBar {
                start: now - start * second,
                end: end.map(|end: i64| now - end * second),
                tone,
                faint: false,
            },
        ],
    };
    GanttModel {
        lanes: vec![
            lane(0, "checkout", 300, 295, Some(280), StateTone::Success),
            lane(1, "deps", 300, 280, Some(220), StateTone::Success),
            lane(2, "build", 300, 220, Some(120), StateTone::Success),
            lane(3, "unit", 300, 120, Some(60), StateTone::Failure),
            lane(4, "package", 300, 120, None, StateTone::Active),
        ],
        links: vec![(0, 1), (1, 2), (2, 3), (2, 4)],
    }
}

struct Gallery {
    page: Page,
    jobs: Entity<Table<Jobs>>,
    graph: Entity<GraphView>,
    gantt: Entity<Gantt>,
    confirming: bool,
}

impl Gallery {
    fn close_confirm(&mut self, _: &gpui::ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.confirming = false;
        cx.notify();
    }

    fn new(cx: &mut Context<Self>) -> Self {
        Self {
            page: Page::Jobs,
            jobs: cx.new(|_| Table::new(Jobs)),
            graph: cx.new(|cx| GraphView::new(graph_model(), cx)),
            gantt: cx.new(|cx| Gantt::new(gantt_model(), cx)),
            confirming: false,
        }
    }

    fn tab(
        &self,
        page: Page,
        label: &'static str,
        icon: Icon,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        side_tab(label, label)
            .icon(icon)
            .active(self.page == page)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.page = page;
                cx.notify();
            }))
    }
}

impl Render for Gallery {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let page: AnyElement = match self.page {
            Page::Jobs => self.jobs.clone().into_any_element(),
            Page::Graph => self.graph.clone().into_any_element(),
            Page::Timeline => self.gantt.clone().into_any_element(),
        };
        let sidebar = side_tabs(cx)
            .w(px(200.0))
            .h_full()
            .child(self.tab(Page::Jobs, "Jobs", Icon::Job, cx))
            .child(self.tab(Page::Graph, "Graph", Icon::Dag, cx))
            .child(self.tab(Page::Timeline, "Timeline", Icon::Gantt, cx))
            .child(
                side_tab("Webhooks", "Webhooks")
                    .icon(Icon::Webhook)
                    .disabled(true),
            )
            .child(
                side_tab("Cancel", "Cancel job…")
                    .icon(Icon::Cancel)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.confirming = true;
                        cx.notify();
                    })),
            );
        div()
            .relative()
            .size_full()
            .flex()
            .flex_row()
            .font_family(font_family(cx))
            .bg(theme.bg_primary)
            .text_color(theme.text_secondary)
            .text_size(px(12.0))
            .child(sidebar)
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .child(Banner::new(
                        StateTone::Queued,
                        "Reconnecting to the server…",
                    ))
                    .child(div().flex_1().min_h_0().child(page)),
            )
            .when(self.confirming, |root| {
                root.child(Modal::confirm(
                    "Cancel job #42?",
                    "Running tasks are stopped and the job ends as cancelled.",
                    "Cancel job",
                    StateTone::Failure,
                    cx.listener(Self::close_confirm),
                    cx.listener(Self::close_confirm),
                ))
            })
    }
}

fn run() {
    gpui_platform::application()
        .with_assets(IconAssets)
        .run(|cx: &mut App| {
            metor_gpui::init(cx);
            let bounds = Bounds::centered(None, size(px(1100.), px(700.)), cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                |_, cx| cx.new(Gallery::new),
            )
            .expect("open window");
            cx.activate(true);
        });
}

#[cfg(not(target_family = "wasm"))]
fn main() {
    run();
}

#[cfg(target_family = "wasm")]
#[wasm_bindgen::prelude::wasm_bindgen(start)]
pub fn start() {
    gpui_platform::web_init();
    run();
}
