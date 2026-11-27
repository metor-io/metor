//! A 500-node random DAG in a [`GraphView`], for checking layout quality and pan/zoom speed.
//!
//! Run with `cargo run -p metor-gpui --example graph --release`; it prints how long layout took.

use gpui::{App, Bounds, WindowBounds, WindowOptions, prelude::*, px, size};
use metor_gpui::graph::{EdgeStyle, GraphEdge, GraphModel, GraphNode, GraphView};
use metor_gpui::{IconAssets, StateTone};

/// Distinct edges into later layers, mostly adjacent ones, from a fixed-seed xorshift.
fn random_dag(nodes: u64, layers: u64, edges: usize, mut seed: u64) -> Vec<(u64, u64)> {
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let per_layer = nodes.div_ceil(layers);
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::with_capacity(edges);
    while out.len() < edges {
        let from = next() % nodes;
        if from / per_layer + 1 >= layers {
            continue;
        }
        let span = if next() % 10 < 7 { 1 } else { 2 + next() % 3 };
        let to = (from / per_layer + span).min(layers - 1) * per_layer + next() % per_layer;
        if to < nodes && seen.insert((from, to)) {
            out.push((from, to));
        }
    }
    out
}

fn model() -> GraphModel {
    const TONES: [StateTone; 5] = [
        StateTone::Success,
        StateTone::Active,
        StateTone::Queued,
        StateTone::Failure,
        StateTone::Skipped,
    ];
    let nodes = (0..500)
        .map(|id| GraphNode {
            state: Some(TONES[id as usize % TONES.len()]),
            detail: Some(format!("{}s", id % 90).into()),
            ..GraphNode::new(id, format!("task-{id}"))
        })
        .collect();
    let edges = random_dag(500, 20, 1000, 0x9E37_79B9_7F4A_7C15)
        .into_iter()
        .enumerate()
        .map(|(i, (from, to))| GraphEdge {
            from,
            to,
            style: if i % 7 == 0 {
                EdgeStyle::Dashed
            } else {
                EdgeStyle::Solid
            },
        })
        .collect();
    GraphModel { nodes, edges }
}

fn main() {
    gpui_platform::application()
        .with_assets(IconAssets)
        .run(|cx: &mut App| {
            metor_gpui::init(cx);
            let bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                |_, cx| {
                    let model = model();
                    let start = std::time::Instant::now();
                    let view = cx.new(|cx| GraphView::new(model, cx));
                    eprintln!("layout took {:.2?}", start.elapsed());
                    view
                },
            )
            .expect("open window");
            cx.activate(true);
        });
}
