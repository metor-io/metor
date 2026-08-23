use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use gpui::{AnyElement, App, SharedString, Window, div, prelude::*, px};
use metor_db::DB;
use metor_proto::types::ComponentId;

use crate::inspector::rows::PreviewSpec;

use crate::inspector::rows::{
    BoolRow, CommandRow, ExpressionRow, InspectorRow, NavRow, RowAction, row_base,
};
use crate::theme::theme;
use crate::views::time_series::Trace;

thread_local! {
    /// Name-sorted component list cached against the DB's vtable generation.
    /// Rebuilt only when a component is added/removed, so the many palette and
    /// picker call sites avoid re-locking DB state and re-sorting every open.
    /// gpui is single-threaded, so a thread-local needs no synchronization.
    static COMPONENT_LIST_CACHE: std::cell::RefCell<Option<(u64, Arc<[(ComponentId, String)]>)>> =
        const { std::cell::RefCell::new(None) };
}

/// Every known component sorted by name, suitable for palette display.
pub(crate) fn list_components(db: &DB) -> Vec<(ComponentId, String)> {
    let generation = db.vtable_gen.latest();
    let cached = COMPONENT_LIST_CACHE.with(|cache| match &*cache.borrow() {
        Some((cached_gen, list)) if *cached_gen == generation => Some(list.clone()),
        _ => None,
    });
    let list = cached.unwrap_or_else(|| {
        let mut components: Vec<(ComponentId, String)> = db.with_state(|state| {
            state
                .component_metadata_iter()
                .filter(|(_, meta)| !meta.is_hidden())
                .map(|(id, meta)| (*id, meta.name.clone()))
                .collect()
        });
        components.sort_by(|a, b| a.1.cmp(&b.1));
        let list: Arc<[(ComponentId, String)]> = components.into();
        COMPONENT_LIST_CACHE.with(|cache| {
            *cache.borrow_mut() = Some((generation, list.clone()));
        });
        list
    });
    list.to_vec()
}

/// Rows listing every known component; selecting one invokes `on_select`.
///
/// The list is preceded by the expression row, so typing `=` turns the same
/// search field into an expression field and `on_select` receives a computed
/// channel exactly as it would a picked one. Every consumer of this picker
/// gains that without knowing it happened.
pub(crate) fn component_picker_rows(
    db: Arc<DB>,
    on_select: impl Fn(ComponentId, String, &mut App) + 'static,
) -> Vec<Box<dyn InspectorRow>> {
    let on_select = Arc::new(on_select);
    let mut rows: Vec<Box<dyn InspectorRow>> = vec![Box::new(ExpressionRow::new(db.clone(), {
        let on_select = on_select.clone();
        Arc::new(move |id, text, _window, cx| {
            on_select(id, text, cx);
            RowAction::Dismiss
        })
    }))];
    rows.extend(list_components(&db).into_iter().map(|(id, name)| {
        let on_select = on_select.clone();
        Box::new(CommandRow::new(
            SharedString::from(name.clone()),
            Arc::new(move |_window, cx| on_select(id, name.clone(), cx)),
        )) as Box<dyn InspectorRow>
    }));
    rows
}

/// Element labels for `component_id`, derived from its schema dimension.
/// Empty when the component is missing from the DB.
pub fn element_names_for_component(db: &DB, component_id: ComponentId) -> Vec<String> {
    db.with_state(|state| {
        state
            .get_component(component_id)
            .map(|c| element_names(c.schema.dim.as_slice()))
            .unwrap_or_default()
    })
}

/// Invoked with the [`Trace`]s the user built through the wizard.
pub type OnTracesSelected = Arc<dyn Fn(Vec<Trace>, &mut Window, &mut App)>;

/// Builds an inspector page spec to drill into when the wizard commits.
///
/// Used by the "Plot Component" palette command to push a transient plot
/// preview onto the inspector's page stack instead of dismissing.
pub type BuildPreview = Arc<dyn Fn(Vec<Trace>, &mut App) -> PreviewSpec>;

/// Starting index into the theme's categorical color palette.
///
/// Lets the wizard continue an existing plot's color sequence (return the
/// current trace count) or restart from zero (return `0`).
pub type ColorBasis = Arc<dyn Fn(&App) -> usize>;

/// Multi-component, multi-element selection accumulated as the user drills
/// through the wizard.
///
/// Order is preserved so the resulting traces follow the user's pick order
/// across components, which determines the palette walk at commit time.
#[derive(Default)]
pub(crate) struct TraceSelection {
    by_component: Vec<(ComponentId, BTreeSet<usize>)>,
}

impl TraceSelection {
    pub fn total_traces(&self) -> usize {
        self.by_component.iter().map(|(_, s)| s.len()).sum()
    }

    pub fn count_for(&self, id: ComponentId) -> usize {
        self.by_component
            .iter()
            .find(|(c, _)| *c == id)
            .map(|(_, s)| s.len())
            .unwrap_or(0)
    }

    pub fn contains(&self, id: ComponentId, idx: usize) -> bool {
        self.by_component
            .iter()
            .find(|(c, _)| *c == id)
            .map(|(_, s)| s.contains(&idx))
            .unwrap_or(false)
    }

    pub fn has_component(&self, id: ComponentId) -> bool {
        self.by_component.iter().any(|(c, _)| *c == id)
    }

    pub fn set_index(&mut self, id: ComponentId, idx: usize, checked: bool) {
        let entry = self.entry(id);
        if checked {
            entry.insert(idx);
        } else {
            entry.remove(&idx);
        }
    }

    pub fn set_all(&mut self, id: ComponentId, elem_count: usize, checked: bool) {
        let entry = self.entry(id);
        entry.clear();
        if checked {
            for i in 0..elem_count {
                entry.insert(i);
            }
        }
    }

    fn entry(&mut self, id: ComponentId) -> &mut BTreeSet<usize> {
        if let Some(pos) = self.by_component.iter().position(|(c, _)| *c == id) {
            return &mut self.by_component[pos].1;
        }
        self.by_component.push((id, BTreeSet::new()));
        &mut self.by_component.last_mut().unwrap().1
    }
}

/// Multi-select wizard for constructing [`Trace`]s.
///
/// The user accumulates picks across multiple components — each component
/// page exposes a checkbox per element plus an "All" toggle — then commits
/// everything in one shot via the pinned "Continue" row. Shared between the
/// "New Panel → Time Series" flow and the in-plot "Add Trace" button so both
/// entry points present the same UX.
pub fn select_traces_wizard_rows(
    db: Arc<DB>,
    color_basis: ColorBasis,
    on_select: OnTracesSelected,
) -> Vec<Box<dyn InspectorRow>> {
    let selection = Arc::new(Mutex::new(TraceSelection::default()));
    component_list_rows(
        db,
        color_basis,
        ContinueAction::Dismiss(on_select),
        selection,
    )
}

/// Variant of [`select_traces_wizard_rows`] that, on Continue, pushes an
/// inspector view page built from `build` instead of dismissing. The
/// inspector keeps the wizard pages on its stack so Escape walks back.
pub fn select_traces_wizard_view(
    db: Arc<DB>,
    color_basis: ColorBasis,
    build: BuildPreview,
) -> Vec<Box<dyn InspectorRow>> {
    let selection = Arc::new(Mutex::new(TraceSelection::default()));
    component_list_rows(
        db,
        color_basis,
        ContinueAction::CascadeView(build),
        selection,
    )
}

/// What the wizard's pinned "Continue" row does when the user commits.
#[derive(Clone)]
enum ContinueAction {
    /// Hand traces to a callback and dismiss the inspector.
    Dismiss(OnTracesSelected),
    /// Push a view-hosting page onto the inspector stack.
    CascadeView(BuildPreview),
}

fn component_list_rows(
    db: Arc<DB>,
    color_basis: ColorBasis,
    on_continue: ContinueAction,
    selection: Arc<Mutex<TraceSelection>>,
) -> Vec<Box<dyn InspectorRow>> {
    let components: Vec<(ComponentId, String, usize)> = list_components(&db)
        .into_iter()
        .map(|(id, name)| {
            let elem_count = element_names_for_component(&db, id).len().max(1);
            (id, name, elem_count)
        })
        .collect();

    let mut rows: Vec<Box<dyn InspectorRow>> = Vec::new();

    // Typing `=` commits one computed trace on its own, rather than joining
    // the multi-select: an expression is already exactly one channel, so
    // there is nothing to check off.
    rows.push(Box::new(ExpressionRow::new(db.clone(), {
        let on_continue = on_continue.clone();
        let color_basis = color_basis.clone();
        Arc::new(move |component, text, window, cx| {
            let color = {
                let theme = theme(cx);
                theme.line_colors[(color_basis)(cx) % theme.line_colors.len()]
            };
            let mut trace = Trace::new(component, 0, color);
            trace.label = SharedString::from(crate::dynamic::expressions::body(&text).to_string());
            match &on_continue {
                ContinueAction::Dismiss(on_select) => {
                    on_select(vec![trace], window, cx);
                    RowAction::Dismiss
                }
                ContinueAction::CascadeView(build) => RowAction::CascadeView(build(vec![trace], cx)),
            }
        })
    })));

    rows.push(Box::new(ContinueRow {
        selection: selection.clone(),
        db: db.clone(),
        color_basis: color_basis.clone(),
        on_continue: on_continue.clone(),
    }));

    for (comp_id, comp_name, elem_count) in components {
        let sel_for_summary = selection.clone();
        let sel_for_cascade = selection.clone();
        let db_for_cascade = db.clone();
        let name_for_cascade = comp_name.clone();

        rows.push(Box::new(NavRow::with_dynamic_summary(
            SharedString::from(comp_name),
            Box::new(move |_cx| {
                let n = sel_for_summary.lock().unwrap().count_for(comp_id);
                if n == 0 {
                    SharedString::new_static("")
                } else if n == elem_count {
                    SharedString::new_static("all")
                } else {
                    SharedString::from(format!("{n} selected"))
                }
            }),
            Box::new(move |_cx| {
                element_page_rows(
                    db_for_cascade.clone(),
                    comp_id,
                    name_for_cascade.clone(),
                    elem_count,
                    sel_for_cascade.clone(),
                )
            }),
        )));
    }

    rows
}

fn element_page_rows(
    db: Arc<DB>,
    comp_id: ComponentId,
    comp_name: String,
    elem_count: usize,
    selection: Arc<Mutex<TraceSelection>>,
) -> Vec<Box<dyn InspectorRow>> {
    let elem_names = element_names_for_component(&db, comp_id);
    let elem_names = if elem_names.is_empty() {
        vec!["value".to_string()]
    } else {
        elem_names
    };

    {
        let mut sel = selection.lock().unwrap();
        if !sel.has_component(comp_id) {
            sel.set_all(comp_id, elem_count, true);
        }
    }

    let mut rows: Vec<Box<dyn InspectorRow>> = Vec::new();
    rows.push(Box::new(DoneRow));

    for (idx, elem_name) in elem_names.into_iter().enumerate() {
        let display = if elem_name.is_empty() {
            format!("[{}]", idx)
        } else {
            elem_name
        };
        let label = SharedString::from(format!("{}.{}", comp_name, display));
        let sel_v = selection.clone();
        let sel_t = selection.clone();
        rows.push(Box::new(BoolRow::dynamic(
            label,
            Arc::new(move |_cx| sel_v.lock().unwrap().contains(comp_id, idx)),
            Arc::new(move |checked, _w, _cx| {
                sel_t.lock().unwrap().set_index(comp_id, idx, checked);
            }),
        )));
    }

    rows
}

/// Pinned commit row for the component-list page. Shows the current trace
/// count in its label; on activation either dismisses the inspector
/// (after handing the built [`Trace`]s to a callback) or cascades into a
/// view page, depending on [`ContinueAction`]. Activating with an empty
/// selection is a no-op.
struct ContinueRow {
    selection: Arc<Mutex<TraceSelection>>,
    db: Arc<DB>,
    color_basis: ColorBasis,
    on_continue: ContinueAction,
}

impl InspectorRow for ContinueRow {
    fn label(&self) -> &str {
        "Continue"
    }

    fn consumes_search(&self) -> bool {
        true
    }

    fn render_row(
        &self,
        row_ix: usize,
        selected: bool,
        _window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let theme = theme(cx);
        let n = self.selection.lock().unwrap().total_traces();
        let text = if n == 0 {
            SharedString::new_static("Continue")
        } else {
            SharedString::from(format!("Continue ({n} traces)"))
        };
        row_base(row_ix, selected, cx)
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme.text_primary)
                    .child(text),
            )
            .into_any_element()
    }

    fn activate(&mut self, window: &mut Window, cx: &mut App) -> RowAction {
        let traces = {
            let sel = self.selection.lock().unwrap();
            if sel.total_traces() == 0 {
                return RowAction::Handled;
            }
            build_traces(&self.db, &sel, &self.color_basis, cx)
        };
        match &self.on_continue {
            ContinueAction::Dismiss(on_select) => {
                on_select(traces, window, cx);
                RowAction::Dismiss
            }
            ContinueAction::CascadeView(build) => RowAction::CascadeView(build(traces, cx)),
        }
    }
}

/// Pinned back-affordance for the element page that pops one page off the
/// inspector stack, mirroring Escape/Backspace but as a clickable row.
struct DoneRow;

impl InspectorRow for DoneRow {
    fn label(&self) -> &str {
        "Done"
    }

    fn consumes_search(&self) -> bool {
        true
    }

    fn render_row(
        &self,
        row_ix: usize,
        selected: bool,
        _window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let theme = theme(cx);
        row_base(row_ix, selected, cx)
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme.text_primary)
                    .child(SharedString::new_static("Done")),
            )
            .into_any_element()
    }

    fn activate(&mut self, _window: &mut Window, _cx: &mut App) -> RowAction {
        RowAction::Pop
    }
}

fn build_traces(db: &DB, sel: &TraceSelection, color_basis: &ColorBasis, cx: &App) -> Vec<Trace> {
    let theme = theme(cx);
    let base = (color_basis)(cx);
    let names = list_components(db);
    let name_for = |id: ComponentId| -> String {
        names
            .iter()
            .find(|(c, _)| *c == id)
            .map(|(_, n)| n.clone())
            .unwrap_or_default()
    };
    let mut out = Vec::new();
    for (comp_id, indices) in &sel.by_component {
        if indices.is_empty() {
            continue;
        }
        let comp_name = name_for(*comp_id);
        let elem_names = element_names_for_component(db, *comp_id);
        for &idx in indices {
            let elem_name = elem_names.get(idx).cloned().unwrap_or_default();
            let display = if elem_name.is_empty() {
                format!("[{}]", idx)
            } else {
                elem_name
            };
            let color = theme.line_colors[(base + out.len()) % theme.line_colors.len()];
            let mut t = Trace::new(*comp_id, idx, color);
            t.label = SharedString::from(format!("{}.{}", comp_name, display));
            out.push(t);
        }
    }
    out
}

/// Default element names for a tensor shape.
///
/// Axes consume letters in order from `NAMES` (x, y, z, w, u, v, s, t);
/// once exhausted the numeric index is used. A rank-0 scalar yields a
/// single empty string so callers can substitute `"value"`.
///
/// Example: `[3]` → `["x", "y", "z"]`; `[2, 2]` → `["xx", "xy", "yx", "yy"]`.
pub(crate) fn element_names(shape: &[usize]) -> Vec<String> {
    fn walk(shape: &[usize], prefix: &str, out: &mut Vec<String>) {
        if shape.is_empty() {
            out.push(prefix.to_string());
            return;
        }
        const NAMES: [char; 8] = ['x', 'y', 'z', 'w', 'u', 'v', 's', 't'];
        for x in 0..shape[0] {
            let mut elem = prefix.to_string();
            if let Some(c) = NAMES.get(x) {
                elem.push(*c);
            } else {
                elem.push_str(&x.to_string());
            }
            walk(&shape[1..], &elem, out);
        }
    }
    let mut out = Vec::new();
    walk(shape, "", &mut out);
    out
}
