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
//!
//! ## Why not full Facet reflection?
//!
//! `NodeSpec` does derive `Facet`, but the inspector's default enum
//! dispatch (`registry/dispatch.rs::default_row_for_shape`) renders an
//! enum as a single variant *picker*. To expose the active variant's
//! per-field rows automatically, the walker would need a nested write
//! path (parent `Poke` → `PokeEnum::field(i).set::<T>(v)`), which the
//! current `set_field` helper doesn't model.
//!
//! Adding a new `NodeSpec` variant with `f64`/`String` args therefore
//! requires extending the match below. The per-variant match also keeps
//! variant-specific UX (`Persist` name field, `FromDb` component picker)
//! out of the generic walker.

use std::sync::Arc;

use gpui::{AnyEntity, App, Entity, SharedString};
use metor_db::DB;

use crate::inspector::registry::InspectorRegistry;
use crate::inspector::rows::{
    ActionRow, CommandRow, DefaultActionRow, EnumRow, InspectorRow, RowAction, ScalarRow, TextRow,
};
use crate::node_editor::graph::{BuildState, FlowId, NodeGraph};
use crate::node_editor::pane::NodeEditor;
use crate::node_editor::registry::{ALL as ALL_OPS, descriptor_for};
use crate::node_editor::spec::{NodeSpec, NodeSpecKind};

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
    cx.global_mut::<InspectorRegistry>()
        .register_type_builder::<NodeEditor>(Arc::new(build_editor_rows));
}

/// Rows for an inspected `NodeEditor` pane. Surfaces "Add Node" (with a
/// wizard for `Persist`/`FromDb` so the user always supplies a component
/// name / picks an existing component) and a delete row for the current
/// selection.
fn build_editor_rows(any: AnyEntity, _db: &Arc<DB>, cx: &App) -> Vec<Box<dyn InspectorRow>> {
    let Ok(editor) = any.downcast::<NodeEditor>() else {
        return Vec::new();
    };
    let mut rows: Vec<Box<dyn InspectorRow>> = Vec::new();

    rows.push(Box::new(
        ActionRow::new(
            SharedString::new_static("Add Node"),
            {
                let editor = editor.clone();
                Arc::new(move |_window, cx| {
                    RowAction::Cascade(build_add_node_rows(editor.clone(), cx))
                })
            },
        )
        .with_tag(SharedString::new_static("editor")),
    ));

    let has_selection = editor.read(cx).selected_node().is_some();
    let has_edge = editor.read(cx).selected_edge_ref().is_some();
    if has_selection || has_edge {
        let editor_for_delete = editor.clone();
        rows.push(Box::new(CommandRow::new(
            SharedString::new_static(if has_edge { "Delete Edge" } else { "Delete Node" }),
            Arc::new(move |_window, cx| {
                editor_for_delete.update(cx, |ed, cx| ed.delete_selection(cx));
            }),
        )));
    }

    rows
}

/// Build the "Add Node" submenu rows. Most ops insert immediately; `Persist`
/// and `FromDb` cascade into a small wizard that requires the user to
/// supply the missing field before the node lands in the graph.
pub fn build_add_node_rows(
    editor: Entity<NodeEditor>,
    cx: &App,
) -> Vec<Box<dyn InspectorRow>> {
    let db = editor.read(cx).db().clone();
    let mut rows: Vec<Box<dyn InspectorRow>> = Vec::new();
    for descriptor in ALL_OPS {
        let label =
            SharedString::from(format!("{} · {}", descriptor.category, descriptor.label));
        match descriptor.kind {
            NodeSpecKind::Persist => {
                let editor = editor.clone();
                rows.push(Box::new(
                    ActionRow::new(
                        label,
                        Arc::new(move |_window, _cx| {
                            RowAction::Cascade(build_persist_wizard(editor.clone()))
                        }),
                    ),
                ));
            }
            NodeSpecKind::FromDb => {
                let editor = editor.clone();
                let db = db.clone();
                rows.push(Box::new(ActionRow::new(
                    label,
                    Arc::new(move |_window, _cx| {
                        RowAction::Cascade(build_from_db_wizard(editor.clone(), db.clone()))
                    }),
                )));
            }
            _ => {
                let editor = editor.clone();
                let descriptor = descriptor;
                rows.push(Box::new(CommandRow::new(
                    label,
                    Arc::new(move |_window, cx| {
                        editor.update(cx, |ed, cx| ed.add_node(descriptor, cx));
                    }),
                )));
            }
        }
    }
    rows
}

/// Single-row wizard: type a name, press Enter, get a `Persist` node.
fn build_persist_wizard(editor: Entity<NodeEditor>) -> Vec<Box<dyn InspectorRow>> {
    vec![Box::new(DefaultActionRow {
        label: SharedString::new_static("Type a component name and press Enter"),
        callback: Arc::new(move |name, _window, cx| {
            if name.trim().is_empty() {
                return;
            }
            editor.update(cx, |ed, cx| {
                ed.add_node_with_spec("Persist", NodeSpec::Persist { name: name.clone() }, cx);
            });
        }),
    })]
}

/// Single-row wizard: pick an existing component from the DB.
fn build_from_db_wizard(
    editor: Entity<NodeEditor>,
    db: Arc<DB>,
) -> Vec<Box<dyn InspectorRow>> {
    let components = crate::inspector::trace_picker::list_components(&db);
    if components.is_empty() {
        return vec![Box::new(TextRow::new_readonly(
            SharedString::new_static("No components"),
            SharedString::new_static("create a Persist first"),
        ))];
    }
    components
        .into_iter()
        .map(|(id, name)| {
            let editor = editor.clone();
            Box::new(CommandRow::new(
                SharedString::from(name),
                Arc::new(move |_window, cx| {
                    editor.update(cx, |ed, cx| {
                        ed.add_node_with_spec(
                            "From DB",
                            NodeSpec::FromDb { component_id: id.0 },
                            cx,
                        );
                    });
                }),
            )) as Box<dyn InspectorRow>
        })
        .collect()
}

fn build_rows(any: AnyEntity, db: &Arc<DB>, cx: &App) -> Vec<Box<dyn InspectorRow>> {
    let Ok(proxy) = any.downcast::<SelectedNodeProxy>() else {
        return Vec::new();
    };
    let proxy_ref = proxy.read(cx);
    let editor = proxy_ref.editor.clone();
    let graph = proxy_ref.graph.clone();
    let flow_id = proxy_ref.flow_id.clone();
    // Use the DB threaded through by the inspector framework rather than
    // reading it off the editor entity — `update_inspector` invokes us
    // from inside the editor's own update closure, so reading the editor
    // here would panic with a re-entrant borrow.
    let db = db.clone();

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
    rows.push(Box::new(TextRow::new_readonly(
        SharedString::from(format!("{} · {}", descriptor.category, descriptor.label)),
        SharedString::from(status),
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
            // Square shares Sin's arg layout; the variant guard inside each
            // closure keeps the spec write type-safe.
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
    Box::new(ScalarRow::new(
        SharedString::from(label),
        value,
        Arc::new(move |new_value, _window, cx| {
            let id = flow_id.clone();
            graph.update(cx, |g, _| {
                if let Some(entry) = g.nodes.get_mut(&id) {
                    apply(&mut entry.spec, new_value);
                }
            });
            editor.update(cx, |ed, cx| ed.bump_rebuild(cx));
        }),
    ))
}
