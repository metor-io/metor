use std::sync::Arc;
use std::time::Duration;

use gpui::{
    App, Context, FocusHandle, Focusable, Hsla, IntoElement, MouseButton, SharedString, Window,
    div, prelude::*, px,
};
use metor_db::DB;
use metor_proto::types::{ComponentId, ElementValue};
use regex::Regex;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use crate::inspector::plot_preview::shift_hover_listener;

use super::binding::{component_meta, spawn_on_stream};
use super::tooltip::TooltipText;
use super::traffic_light::traffic_light_swatch;
use crate::inspector::rows::{DefaultActionRow, InspectorRow};
use crate::theme::theme;

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct TrafficLightGridConfig {
    pub pattern: String,
    pub color: Option<Hsla>,
}

/// Cell tracked by the grid for one matched component.
struct GridCell {
    id: ComponentId,
    name: SharedString,
    is_bool: bool,
    element_names: Vec<SharedString>,
    latest_on: Option<bool>,
    _task: gpui::Task<()>,
}

/// Live grid of traffic-light tiles, one per component matching `pattern`.
///
/// `pattern` uses the same glob syntax as the component browser
/// (`*`, `?`). The watcher task re-evaluates the regex every time a new
/// component registers, so producers that come up after the grid is placed
/// still appear without needing the user to retype.
#[derive(facet::Facet)]
pub struct TrafficLightGrid {
    pub pattern: SharedString,
    pub color: Hsla,
    #[facet(opaque)]
    db: Arc<DB>,
    /// Pattern the current `regex` was compiled from. Compared against
    /// `pattern` at render time so direct field writes (via the reflective
    /// inspector) trigger a recompile without going through `set_pattern`.
    #[facet(skip)]
    compiled_pattern: SharedString,
    #[facet(opaque)]
    regex: Option<Regex>,
    #[facet(opaque)]
    cells: Vec<GridCell>,
    #[facet(opaque)]
    focus: FocusHandle,
    #[facet(opaque)]
    _watcher: gpui::Task<()>,
}

impl TrafficLightGrid {
    pub fn new(db: Arc<DB>, pattern: SharedString, cx: &mut Context<Self>) -> Self {
        let color = theme(cx).control_active;
        let regex = compile_pattern(&pattern);
        let watcher = spawn_watcher(db.clone(), cx);
        Self {
            compiled_pattern: pattern.clone(),
            pattern,
            color,
            db,
            regex,
            cells: Vec::new(),
            focus: cx.focus_handle(),
            _watcher: watcher,
        }
    }

    pub fn pattern(&self) -> &SharedString {
        &self.pattern
    }

    pub fn color(&self) -> Hsla {
        self.color
    }

    pub fn set_pattern(&mut self, pattern: SharedString, cx: &mut Context<Self>) {
        if self.pattern == pattern {
            return;
        }
        self.pattern = pattern;
        self.recompile_and_reconcile(cx);
    }

    /// Recompile the regex from `self.pattern` and rebuild the cell list.
    ///
    /// Used both by `set_pattern` (programmatic) and at render time when the
    /// inspector has written `pattern` directly through the reflective field
    /// adapter. After mutation, `cx.notify()` is emitted so the next frame
    /// shows the new cell set.
    fn recompile_and_reconcile(&mut self, cx: &mut Context<Self>) {
        self.compiled_pattern = self.pattern.clone();
        self.regex = compile_pattern(&self.pattern);
        self.reconcile_cells(cx);
    }

    pub fn set_color(&mut self, color: Hsla, cx: &mut Context<Self>) {
        if self.color == color {
            return;
        }
        self.color = color;
        cx.notify();
    }

    /// Diff the matched component set against `self.cells`, dropping cells
    /// that no longer match and creating tasks for newly-matched components.
    /// Existing cells keep their stream tasks so we don't drop in-flight
    /// values just because a sibling component registered.
    fn reconcile_cells(&mut self, cx: &mut Context<Self>) {
        let matches: Vec<(ComponentId, String)> = match &self.regex {
            Some(re) => crate::inspector::trace_picker::list_components(&self.db)
                .into_iter()
                .filter(|(_, name)| re.is_match(name))
                .collect(),
            None => Vec::new(),
        };

        self.cells
            .retain(|c| matches.iter().any(|(id, _)| *id == c.id));

        for (id, name) in matches {
            if let Some(cell) = self.cells.iter_mut().find(|c| c.id == id) {
                // A kept cell may have been built before its component
                // registered; re-read metadata so is_bool/element_names
                // catch up without tearing down the stream task.
                let meta = component_meta(&self.db, id);
                cell.is_bool = meta.is_bool;
                cell.element_names = meta.element_names;
                continue;
            }
            let cell = build_cell(self.db.clone(), id, SharedString::from(name), cx);
            self.cells.push(cell);
        }

        self.cells.sort_by(|a, b| a.name.cmp(&b.name));
        cx.notify();
    }

    fn toggle_cell(&mut self, id: ComponentId, cx: &mut Context<Self>) {
        let Some(cell) = self.cells.iter().find(|c| c.id == id) else {
            return;
        };
        if !cell.is_bool {
            return;
        }
        let next = !cell.latest_on.unwrap_or(false);
        crate::inspector::edits::upsert_element_value(
            &self.db,
            cell.id,
            cell.name.clone(),
            cell.element_names.clone(),
            0,
            ElementValue::Bool(next),
            cx,
        );
        cx.notify();
    }
}

impl Focusable for TrafficLightGrid {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for TrafficLightGrid {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.compiled_pattern != self.pattern {
            self.recompile_and_reconcile(cx);
        }
        let theme = theme(cx);
        let color = self.color;

        let mut row = div()
            .track_focus(&self.focus)
            .size_full()
            .bg(theme.bg_primary)
            .p(px(8.0))
            .flex()
            .flex_row()
            .flex_wrap()
            .gap(px(3.0))
            .content_start();

        for cell in &self.cells {
            let id = cell.id;
            let is_bool = cell.is_bool;
            let name = cell.name.clone();
            let value = cell.latest_on;
            let id_hash = id.0 as usize;

            let mut tile = div()
                .id(("traffic-light-grid-cell", id_hash))
                .child(traffic_light_swatch(value, color, px(14.0), &theme))
                .tooltip({
                    let name = name.clone();
                    move |_window, cx| TooltipText::build(name.clone(), cx)
                })
                .on_mouse_move(shift_hover_listener(id, SmallVec::new()));

            if is_bool {
                tile = tile.cursor_pointer().on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _window, cx| {
                        this.toggle_cell(id, cx);
                    }),
                );
            }

            row = row.child(tile);
        }

        row
    }
}

fn compile_pattern(pattern: &str) -> Option<Regex> {
    if pattern.is_empty() {
        return None;
    }
    crate::views::component_browser::glob_to_regex(pattern).ok()
}

/// Spawn the watcher task that rebuilds the cell list whenever the DB
/// vtable generation advances (i.e. a component is registered).
///
/// Modelled on `ComponentBrowserDelegate::spawn_watcher`.
fn spawn_watcher(db: Arc<DB>, cx: &mut Context<TrafficLightGrid>) -> gpui::Task<()> {
    cx.spawn(async move |this, cx| {
        loop {
            let result = this.update(cx, |grid, cx| grid.reconcile_cells(cx));
            if result.is_err() {
                break;
            }
            db.vtable_gen.wait().await;
            // vtable_gen bumps once per registered component, so a startup
            // burst would run the O(components) regex reconcile N times.
            // Debounce so a burst collapses into one pass (reconcile reads
            // the latest full state regardless).
            cx.background_executor().timer(VTABLE_DEBOUNCE).await;
        }
    })
}

const VTABLE_DEBOUNCE: Duration = Duration::from_millis(50);

/// Single-question wizard row for "give me a glob pattern, build a
/// `TrafficLightGrid` from it". Empty input is a no-op so a stray Enter
/// doesn't create an empty grid. Used by both the dashboard "+ widget" menu
/// and the pane's "New Panel" menu.
pub fn glob_prompt_row(
    on_submit: Arc<dyn Fn(SharedString, &mut Window, &mut App)>,
) -> Box<dyn InspectorRow> {
    Box::new(DefaultActionRow {
        label: "Glob pattern (e.g. *.health)…".into(),
        callback: Arc::new(move |input, window, cx| {
            if input.is_empty() {
                return;
            }
            on_submit(SharedString::from(input), window, cx);
        }),
    })
}

fn build_cell(
    db: Arc<DB>,
    id: ComponentId,
    name: SharedString,
    cx: &mut Context<TrafficLightGrid>,
) -> GridCell {
    let meta = component_meta(&db, id);
    let task = spawn_on_stream(db, id, cx, move |grid, on, cx| {
        if let Some(cell) = grid.cells.iter_mut().find(|c| c.id == id) {
            cell.latest_on = Some(on);
            cx.notify();
        }
    });

    GridCell {
        id,
        name,
        is_bool: meta.is_bool,
        element_names: meta.element_names,
        latest_on: None,
        _task: task,
    }
}
