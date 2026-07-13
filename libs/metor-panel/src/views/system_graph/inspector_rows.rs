//! Inspector rows for a clicked system-graph node.
//!
//! Clicking a node dispatches an `InspectEntity` carrying a
//! [`SelectedGraphNode`] proxy; the inspector's registry maps that type to
//! [`build_rows`], which re-derives the node's detail from the live wiring
//! store. The rows are read-only — the tile shows topology, it never edits it.

use std::sync::Arc;

use gpui::{AnyEntity, App, SharedString};
use metor_db::DB;
use metor_fsw_2::ir::{ParamSource, SourceRef, Wiring};

use crate::inspector::registry::InspectorRegistry;
use crate::inspector::rows::{HeaderRow, InspectorRow, TextRow};

/// Entity placed into the inspector when a graph node is selected. Carries
/// only the node id; [`build_rows`] looks the details up in the live store so
/// the rows always reflect the latest manifest.
pub struct SelectedGraphNode {
    pub id: SharedString,
}

pub fn register_inspector_rows(cx: &mut App) {
    cx.global_mut::<InspectorRegistry>()
        .register_type_builder::<SelectedGraphNode>(Arc::new(build_rows));
}

fn text_row(label: &'static str, value: impl Into<SharedString>) -> Box<dyn InspectorRow> {
    Box::new(TextRow::readonly(SharedString::new_static(label), value.into()))
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
        ParamSource::Kdl(_) => "kdl".into(),
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

fn build_rows(any: AnyEntity, _db: &Arc<DB>, cx: &App) -> Vec<Box<dyn InspectorRow>> {
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
        return system_rows(sys);
    }
    if let Some(slot) = wiring.slots.iter().find(|s| s.name == id.as_str()) {
        return slot_rows(slot);
    }
    scope_or_coordinator_rows(&id, wiring)
}

fn system_rows(sys: &metor_fsw_2::ir::SystemSpec) -> Vec<Box<dyn InspectorRow>> {
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
    rows
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
    if id.as_str() == super::layout::COORDINATOR_INSTANCE {
        return vec![
            Box::new(HeaderRow::new("Coordinator")),
            text_row("Name", id.clone()),
            text_row("Cycle rate (Hz)", format!("{}", wiring.coordinator.cycle_rate)),
            text_row("Clock", format!("{:?}", wiring.coordinator.clock)),
        ];
    }
    // A collapsed-scope group node: id is the scope path.
    let members = wiring
        .systems
        .iter()
        .filter(|s| scope_path_of(wiring, s.scope).is_some_and(|p| p == id.as_str() || p.starts_with(&format!("{id}."))))
        .count()
        + wiring
            .slots
            .iter()
            .filter(|s| scope_path_of(wiring, s.scope).is_some_and(|p| p == id.as_str() || p.starts_with(&format!("{id}."))))
            .count();
    vec![
        Box::new(HeaderRow::new("Scope")),
        text_row("Path", id.clone()),
        text_row("Members", format!("{members}")),
    ]
}

fn scope_path_of(wiring: &Wiring, scope: Option<usize>) -> Option<&str> {
    scope.and_then(|i| wiring.scopes.get(i)).map(|s| s.path.as_str())
}
