//! Inspector rows for the Execution Timeline pane.
//!
//! Hand-written rather than reflected: what an operator adjusts here is which
//! *derived* lanes to show, and that list only exists at runtime — there is no
//! persisted field to point a facet widget at.
//!
//! The gutter is a second entry point: clicking a lane's name dispatches an
//! `InspectEntity` carrying a [`SelectedGraphNode`] proxy, whose rows re-derive
//! that system, slot, scope, or coordinator from the live wiring store. Those
//! rows are read-only — a native system's source of truth is Rust and
//! `target.py`, not a panel.

use std::sync::Arc;

use gpui::{AnyEntity, App, Entity, SharedString};
use metor_db::DB;
use metor_fsw_2::ir::{ParamSource, SourceRef, Wiring};

use crate::inspector::registry::InspectorRegistry;
use crate::inspector::rows::{BoolRow, CommandRow, HeaderRow, InspectorRow, NavRow, TextRow};
use crate::views::time_series::Override;
use crate::views::time_series::time_range::TimeRangeBehavior;

use super::ExecTimeline;
use super::rows::COORDINATOR;

pub fn register_inspector_rows(cx: &mut App) {
    cx.global_mut::<InspectorRegistry>()
        .register_type_builder::<ExecTimeline>(Arc::new(build_rows));
    cx.global_mut::<InspectorRegistry>()
        .register_type_builder::<SelectedGraphNode>(Arc::new(build_node_rows));
}

fn range_summary(timeline: &ExecTimeline) -> SharedString {
    match timeline.x_range.as_custom() {
        Some(range) => SharedString::from(range.to_string()),
        None => SharedString::new_static("Auto"),
    }
}

fn range_rows(timeline: Entity<ExecTimeline>) -> Vec<Box<dyn InspectorRow>> {
    let mut rows: Vec<Box<dyn InspectorRow>> = vec![Box::new(CommandRow::new(
        SharedString::new_static("Auto (follow app range)"),
        Arc::new({
            let timeline = timeline.clone();
            move |_window, cx| {
                timeline.update(cx, |t, cx| t.set_x_range(Override::Auto, cx));
            }
        }),
    ))];
    for (name, preset) in TimeRangeBehavior::PRESETS {
        let timeline = timeline.clone();
        let preset = *preset;
        rows.push(Box::new(CommandRow::new(
            SharedString::new_static(name),
            Arc::new(move |_window, cx| {
                timeline.update(cx, |t, cx| t.set_x_range(Override::Custom(preset), cx));
            }),
        )));
    }
    rows
}

fn build_rows(any: AnyEntity, _db: &Arc<DB>, cx: &App) -> Vec<Box<dyn InspectorRow>> {
    let Ok(timeline) = any.downcast::<ExecTimeline>() else {
        return Vec::new();
    };
    let view = timeline.read(cx);
    let mut rows: Vec<Box<dyn InspectorRow>> = vec![
        Box::new(HeaderRow::new("Execution Timeline")),
        Box::new(NavRow::new(
            SharedString::new_static("Time range"),
            range_summary(view),
            Box::new({
                let timeline = timeline.clone();
                move |_cx| range_rows(timeline.clone())
            }),
        )),
        Box::new(BoolRow::new(
            SharedString::new_static("Trigger on latest cycle"),
            view.trigger,
            Arc::new({
                let timeline = timeline.clone();
                move |value, _window, cx| {
                    timeline.update(cx, |t, cx| t.set_trigger(value, cx));
                }
            }),
        )),
        Box::new(BoolRow::new(
            SharedString::new_static("Show slots"),
            view.show_slots,
            Arc::new({
                let timeline = timeline.clone();
                move |value, _window, cx| {
                    timeline.update(cx, |t, cx| {
                        t.show_slots = value;
                        cx.notify();
                    });
                }
            }),
        )),
        Box::new(BoolRow::new(
            SharedString::new_static("Show coordinator row"),
            view.show_coordinator_row,
            Arc::new({
                let timeline = timeline.clone();
                move |value, _window, cx| {
                    timeline.update(cx, |t, cx| {
                        t.show_coordinator_row = value;
                        cx.notify();
                    });
                }
            }),
        )),
    ];

    let names = view.row_names();
    if !names.is_empty() {
        rows.push(Box::new(HeaderRow::new("Rows")));
    }
    for name in names {
        let visible = !view.is_row_hidden(&name);
        let timeline = timeline.clone();
        rows.push(Box::new(BoolRow::new(
            name.clone(),
            visible,
            Arc::new(move |_value, _window, cx| {
                let name = name.clone();
                timeline.update(cx, |t, cx| t.toggle_row(name, cx));
            }),
        )));
    }
    rows
}

/// Entity placed into the inspector when a gutter lane is clicked. Carries
/// only the lane's name; [`build_node_rows`] looks the details up in the live
/// store so the rows always reflect the latest manifest.
pub struct SelectedGraphNode {
    pub id: SharedString,
}

fn text_row(label: &'static str, value: impl Into<SharedString>) -> Box<dyn InspectorRow> {
    Box::new(TextRow::readonly(
        SharedString::new_static(label),
        value.into(),
    ))
}

fn src_summary(src: &Option<SourceRef>) -> Option<SharedString> {
    src.as_ref().map(|s| {
        let file = s.file.as_deref().unwrap_or("<unknown>");
        SharedString::from(format!("{file}:{}", s.line))
    })
}

fn params_summary(params: &ParamSource) -> SharedString {
    match params {
        ParamSource::None => "none".into(),
        ParamSource::Postcard(_) => "postcard".into(),
        ParamSource::Value(v) => {
            let s = v.to_string();
            if s.len() > 60 {
                SharedString::from(format!("{}…", &s[..60]))
            } else {
                SharedString::from(s)
            }
        }
    }
}

fn build_node_rows(any: AnyEntity, _db: &Arc<DB>, cx: &App) -> Vec<Box<dyn InspectorRow>> {
    let Ok(proxy) = any.downcast::<SelectedGraphNode>() else {
        return Vec::new();
    };
    let id = proxy.read(cx).id.clone();
    let Some(store) = crate::wiring::try_global(cx) else {
        return Vec::new();
    };
    let store = store.read(cx);
    let Some(wiring) = store.state().wiring() else {
        return Vec::new();
    };

    if let Some(sys) = wiring.systems.iter().find(|s| s.name == id.as_str()) {
        return system_rows(sys, wiring);
    }
    if let Some(slot) = wiring.slots.iter().find(|s| s.name == id.as_str()) {
        return slot_rows(slot);
    }
    scope_or_coordinator_rows(&id, wiring)
}

fn system_rows(sys: &metor_fsw_2::ir::SystemSpec, wiring: &Wiring) -> Vec<Box<dyn InspectorRow>> {
    let mut rows: Vec<Box<dyn InspectorRow>> = Vec::new();
    rows.push(Box::new(HeaderRow::new("System")));
    rows.push(text_row("Name", sys.name.clone()));
    rows.push(text_row(
        "Type",
        sys.ty.clone().unwrap_or_else(|| "(default)".into()),
    ));
    rows.push(text_row(
        "Artifact",
        sys.artifact
            .clone()
            .unwrap_or_else(|| "(static)".to_string()),
    ));
    rows.push(text_row("Process", if sys.process { "yes" } else { "no" }));
    rows.push(text_row("Params", params_summary(&sys.params)));
    if let Some(src) = src_summary(&sys.src) {
        rows.push(text_row("Source", src));
    }
    if let Some(source) = program_source(sys, wiring) {
        // A vehicle Python system: its declaration, verbatim. Read-only like
        // every row here — the source of truth is `target.py`, and edits go
        // through it.
        rows.push(Box::new(HeaderRow::new("Python")));
        rows.push(text_row("Declaration", source));
    }
    rows
}

/// The captured `@system` declaration behind a program-built spec: the slice
/// of [`Wiring::program`]'s assembled source from the declaration's offset to
/// the next declaration's (or the end).
fn program_source(sys: &metor_fsw_2::ir::SystemSpec, wiring: &Wiring) -> Option<String> {
    let program_artifact = wiring
        .artifacts
        .iter()
        .any(|a| sys.artifact.as_deref() == Some(a.id.as_str()) && a.is_program());
    if !program_artifact {
        return None;
    }
    let program = wiring.program.as_ref()?;
    let entry = sys.ty.as_deref().unwrap_or(&sys.name);
    let at = program.decls.iter().position(|d| d.name == entry)?;
    let start = program.decls[at].offset as usize;
    let end = program
        .decls
        .get(at + 1)
        .map(|d| d.offset as usize)
        .unwrap_or(program.source.len());
    Some(program.source.get(start..end)?.trim_end().to_string())
}

fn slot_rows(slot: &metor_fsw_2::ir::SlotSpec) -> Vec<Box<dyn InspectorRow>> {
    let occupants: Vec<&str> = slot.allow.iter().map(|a| a.occupant.as_str()).collect();
    let initial = match &slot.initial {
        Some(i) => format!("{} ({:?})", i.occupant, i.state),
        None => "(empty)".to_string(),
    };
    let mut rows: Vec<Box<dyn InspectorRow>> = vec![
        Box::new(HeaderRow::new("Slot")),
        text_row("Name", slot.name.clone()),
        text_row("Inputs", slot.inputs.join(", ")),
        text_row("Outputs", slot.outputs.join(", ")),
        text_row("Allowed", occupants.join(", ")),
        text_row("Process", if slot.process { "yes" } else { "no" }),
        text_row("Initial", initial),
    ];
    if let Some(src) = src_summary(&slot.src) {
        rows.push(text_row("Source", src));
    }
    rows
}

fn scope_or_coordinator_rows(id: &SharedString, wiring: &Wiring) -> Vec<Box<dyn InspectorRow>> {
    if id.as_str() == COORDINATOR {
        return vec![
            Box::new(HeaderRow::new("Coordinator")),
            text_row("Name", id.clone()),
            text_row(
                "Cycle rate (Hz)",
                format!("{}", wiring.coordinator.cycle_rate),
            ),
            text_row("Clock", format!("{:?}", wiring.coordinator.clock)),
        ];
    }
    // A collapsed-scope group node: id is the scope path.
    let members = wiring
        .systems
        .iter()
        .filter(|s| {
            scope_path_of(wiring, s.scope)
                .is_some_and(|p| p == id.as_str() || p.starts_with(&format!("{id}.")))
        })
        .count()
        + wiring
            .slots
            .iter()
            .filter(|s| {
                scope_path_of(wiring, s.scope)
                    .is_some_and(|p| p == id.as_str() || p.starts_with(&format!("{id}.")))
            })
            .count();
    vec![
        Box::new(HeaderRow::new("Scope")),
        text_row("Path", id.clone()),
        text_row("Members", format!("{members}")),
    ]
}

fn scope_path_of(wiring: &Wiring, scope: Option<usize>) -> Option<&str> {
    scope
        .and_then(|i| wiring.scopes.get(i))
        .map(|s| s.path.as_str())
}
