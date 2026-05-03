//! Inspector rows for the currently-selected node.
//!
//! A `SelectedNodeProxy` entity carries `(editor, graph, flow_id)`. The
//! inspector's reflection registry is told that proxies of this type produce
//! their rows from a custom builder rather than facet reflection (which
//! would lose the editor's `bump_rebuild` hook).
//!
//! The builder dispatches per-variant: each arg becomes one existing row
//! (`ScalarRow` / `TextRow` / `EnumRow`) wired so `on_change` updates the
//! spec on the graph entity *and* asks the editor to debounce-rebuild.

use std::sync::Arc;

use gpui::{AnyEntity, App, Entity, SharedString};
use metor_db::DB;

use crate::inspector::registry::InspectorRegistry;
use crate::inspector::rows::{EnumRow, InspectorRow, ScalarRow, TextRow};
use crate::node_editor::graph::{BuildState, FlowId, NodeGraph};
use crate::node_editor::pane::NodeEditor;
use crate::node_editor::registry::descriptor_for;
use crate::node_editor::spec::NodeSpec;

/// Entity placed into the inspector when a node is selected. Created on
/// right-click; reused for the lifetime of the selection.
pub struct SelectedNodeProxy {
    pub editor: Entity<NodeEditor>,
    pub graph: Entity<NodeGraph>,
    pub flow_id: FlowId,
}

pub fn register_inspector_rows(cx: &mut App) {
    cx.global_mut::<InspectorRegistry>()
        .register_type_builder::<SelectedNodeProxy>(Arc::new(build_rows));
}

fn build_rows(any: AnyEntity, _db: &Arc<DB>, cx: &App) -> Vec<Box<dyn InspectorRow>> {
    let Ok(proxy) = any.downcast::<SelectedNodeProxy>() else {
        return Vec::new();
    };
    let proxy_ref = proxy.read(cx);
    let editor = proxy_ref.editor.clone();
    let graph = proxy_ref.graph.clone();
    let flow_id = proxy_ref.flow_id.clone();
    let db = editor.read(cx).db().clone();

    let Some(spec) = graph.read(cx).nodes.get(&flow_id).map(|e| e.spec.clone()) else {
        return Vec::new();
    };
    let descriptor = descriptor_for(&spec);

    let mut rows: Vec<Box<dyn InspectorRow>> = Vec::new();

    // Header row: descriptor label + status pill as the row "value".
    let status = match graph.read(cx).nodes.get(&flow_id).map(|e| &e.build) {
        Some(BuildState::Built(_)) => "built",
        Some(BuildState::Pending) => "pending",
        Some(BuildState::Error(e)) => match e {
            crate::dynamic::BuildError::ClockMismatch => "clock mismatch",
            crate::dynamic::BuildError::Cycle => "cycle",
            crate::dynamic::BuildError::ParentFailed => "parent failed",
            crate::dynamic::BuildError::EmptyInputs => "no inputs",
            crate::dynamic::BuildError::ExpectedClock(_) => "expected clock",
            crate::dynamic::BuildError::ExpectedValue => "expected value",
            crate::dynamic::BuildError::ExpectedFloat(_) => "expected float",
            crate::dynamic::BuildError::SchemaMismatch { .. } => "schema mismatch",
            crate::dynamic::BuildError::WrongArity { .. } => "wrong arity",
        },
        None => "missing",
    };
    rows.push(Box::new(TextRow::new(
        SharedString::from(format!("{} · {}", descriptor.category, descriptor.label)),
        SharedString::from(status),
        Arc::new(|_, _, _| {}),
    )));

    match &spec {
        NodeSpec::FixedRate { hz } => {
            rows.push(scalar_arg(
                "Rate (Hz)",
                *hz,
                editor.clone(),
                graph.clone(),
                flow_id.clone(),
                |spec, v| {
                    if let NodeSpec::FixedRate { hz } = spec {
                        *hz = v;
                    }
                },
            ));
        }
        NodeSpec::Sin { freq, amplitude, phase } => {
            rows.push(scalar_arg("Frequency", *freq, editor.clone(), graph.clone(), flow_id.clone(), |s, v| {
                if let NodeSpec::Sin { freq, .. } = s { *freq = v; }
            }));
            rows.push(scalar_arg("Amplitude", *amplitude, editor.clone(), graph.clone(), flow_id.clone(), |s, v| {
                if let NodeSpec::Sin { amplitude, .. } = s { *amplitude = v; }
            }));
            rows.push(scalar_arg("Phase", *phase, editor.clone(), graph.clone(), flow_id.clone(), |s, v| {
                if let NodeSpec::Sin { phase, .. } = s { *phase = v; }
            }));
        }
        NodeSpec::Square { freq, amplitude, phase } => {
            rows.push(scalar_arg("Frequency", *freq, editor.clone(), graph.clone(), flow_id.clone(), |s, v| {
                if let NodeSpec::Square { freq, .. } = s { *freq = v; }
            }));
            rows.push(scalar_arg("Amplitude", *amplitude, editor.clone(), graph.clone(), flow_id.clone(), |s, v| {
                if let NodeSpec::Square { amplitude, .. } = s { *amplitude = v; }
            }));
            rows.push(scalar_arg("Phase", *phase, editor.clone(), graph.clone(), flow_id.clone(), |s, v| {
                if let NodeSpec::Square { phase, .. } = s { *phase = v; }
            }));
        }
        NodeSpec::Random { seed } => {
            rows.push(scalar_arg(
                "Seed",
                *seed as f64,
                editor.clone(),
                graph.clone(),
                flow_id.clone(),
                |spec, v| {
                    if let NodeSpec::Random { seed } = spec {
                        *seed = v as u64;
                    }
                },
            ));
        }
        NodeSpec::Constant { value } => {
            rows.push(scalar_arg(
                "Value",
                *value,
                editor.clone(),
                graph.clone(),
                flow_id.clone(),
                |spec, v| {
                    if let NodeSpec::Constant { value } = spec {
                        *value = v;
                    }
                },
            ));
        }
        NodeSpec::Scale { k } => {
            rows.push(scalar_arg(
                "Scale (k)",
                *k,
                editor.clone(),
                graph.clone(),
                flow_id.clone(),
                |spec, v| {
                    if let NodeSpec::Scale { k } = spec {
                        *k = v;
                    }
                },
            ));
        }
        NodeSpec::Offset { k } => {
            rows.push(scalar_arg(
                "Offset",
                *k,
                editor.clone(),
                graph.clone(),
                flow_id.clone(),
                |spec, v| {
                    if let NodeSpec::Offset { k } = spec {
                        *k = v;
                    }
                },
            ));
        }
        NodeSpec::FromDb { component_id } => {
            // List every component in the DB; fold (id, name) into an EnumRow.
            let components = crate::inspector::trace_picker::list_components(&db);
            let selected_label = components
                .iter()
                .find(|(id, _)| id.0 == *component_id)
                .map(|(_, name)| SharedString::from(name.clone()))
                .unwrap_or_else(|| SharedString::from(format!("id {component_id}")));
            let options: Vec<SharedString> =
                components.iter().map(|(_, name)| SharedString::from(name.clone())).collect();
            let id_by_name: std::collections::HashMap<String, u64> = components
                .into_iter()
                .map(|(id, name)| (name, id.0))
                .collect();
            let editor = editor.clone();
            let graph = graph.clone();
            let flow_id = flow_id.clone();
            rows.push(Box::new(EnumRow {
                label: SharedString::from("Component"),
                selected: selected_label,
                options,
                on_select: Arc::new(move |chosen, _window, cx| {
                    let Some(new_id) = id_by_name.get(&chosen).copied() else {
                        return;
                    };
                    let id = flow_id.clone();
                    graph.update(cx, |g, _| {
                        if let Some(entry) = g.nodes.get_mut(&id)
                            && let NodeSpec::FromDb { component_id } = &mut entry.spec
                        {
                            *component_id = new_id;
                        }
                    });
                    editor.update(cx, |ed, cx| ed.bump_rebuild(cx));
                }),
            }));
        }
        NodeSpec::Persist { name } => {
            let editor = editor.clone();
            let graph = graph.clone();
            let flow_id = flow_id.clone();
            rows.push(Box::new(TextRow::new(
                SharedString::from("Component Name"),
                SharedString::from(name.clone()),
                Arc::new(move |new_name, _window, cx| {
                    let id = flow_id.clone();
                    graph.update(cx, |g, _| {
                        if let Some(entry) = g.nodes.get_mut(&id)
                            && let NodeSpec::Persist { name } = &mut entry.spec
                        {
                            *name = new_name;
                        }
                    });
                    editor.update(cx, |ed, cx| ed.bump_rebuild(cx));
                }),
            )));
        }
        // No editable args.
        NodeSpec::ClockOf
        | NodeSpec::Abs
        | NodeSpec::Neg
        | NodeSpec::Log
        | NodeSpec::Add
        | NodeSpec::Sub
        | NodeSpec::Mul
        | NodeSpec::Mean
        | NodeSpec::Zoh
        | NodeSpec::Linear
        | NodeSpec::LatestAt => {}
    }

    rows
}

/// Build a `ScalarRow` whose `on_change` updates the spec via `apply` and
/// nudges the editor to rebuild. Three captured clones are intentional —
/// each row owns its handles independently of any others.
fn scalar_arg(
    label: &'static str,
    value: f64,
    editor: Entity<NodeEditor>,
    graph: Entity<NodeGraph>,
    flow_id: FlowId,
    apply: impl Fn(&mut NodeSpec, f64) + 'static,
) -> Box<dyn InspectorRow> {
    let apply = Arc::new(apply);
    Box::new(ScalarRow {
        label: SharedString::from(label),
        value,
        on_change: Arc::new(move |new_value, _window, cx| {
            let id = flow_id.clone();
            graph.update(cx, |g, _| {
                if let Some(entry) = g.nodes.get_mut(&id) {
                    apply(&mut entry.spec, new_value);
                }
            });
            editor.update(cx, |ed, cx| ed.bump_rebuild(cx));
        }),
    })
}
