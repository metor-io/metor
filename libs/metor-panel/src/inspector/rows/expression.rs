use std::sync::Arc;

use gpui::{AnyElement, App, SharedString, Window, div, prelude::*, px};
use metor_db::DB;
use metor_expr::Resolver;
use metor_expr::complete::{CompletionItem, CompletionKind, Scope};
use metor_proto::types::ComponentId;

use super::{InspectorRow, RowAction, row_base};
use crate::dynamic::expressions::{self, SIGIL};
use crate::inspector::completion;
use crate::theme::theme;

/// What a picker does with a committed channel — a picked component and a
/// computed expression arrive the same way.
///
/// It receives the component to read and the text that produced it (a plain
/// name, or a `=`-sigiled expression for round-tripping), and decides what
/// happens next — most callers dismiss, while a multi-select wizard hands
/// back traces and closes itself.
pub type OnExpression = Arc<dyn Fn(ComponentId, String, &mut Window, &mut App) -> RowAction>;

/// How a page spells a matched component: a single-pick picker commits it, the
/// trace wizard drills into its element checkboxes, and a page with a shape
/// requirement — the list plot's vectors — answers `None` for what it cannot
/// take. The provider stays out of all of that.
pub type ComponentRowBuilder =
    Arc<dyn Fn(ComponentId, &CompletionItem, &App) -> Option<Box<dyn InspectorRow>>>;

/// A pinned row a page keeps visible under the candidates — the wizard's
/// Continue row, which would otherwise vanish while a query is typed.
pub type TailRowBuilder = Arc<dyn Fn() -> Box<dyn InspectorRow>>;

/// The picker's completion provider, riding along as its first row.
///
/// On an empty query it is a hint line; the moment something is typed,
/// [`query_rows`](InspectorRow::query_rows) takes over the page. A query
/// that is just a name searches — every component, whatever its type, ranked
/// by the same matcher as before, spelled by the page's own
/// [`ComponentRowBuilder`]. A query that is an expression builds: candidates
/// insert into the field, and a pinned compute row carries the commit,
/// showing the compiler's first complaint while the text does not compile.
///
/// The `=` sigil is accepted but no longer required — it forces expression
/// interpretation, and serialized bindings still round-trip through it.
pub struct ExpressionRow {
    db: Arc<DB>,
    on_select: OnExpression,
    component_row: ComponentRowBuilder,
    tail: Option<TailRowBuilder>,
}

impl ExpressionRow {
    pub fn new(
        db: Arc<DB>,
        on_select: OnExpression,
        component_row: ComponentRowBuilder,
        tail: Option<TailRowBuilder>,
    ) -> Self {
        Self {
            db,
            on_select,
            component_row,
            tail,
        }
    }

    /// A single-pick page's component spelling: the candidate line, committing
    /// through `on_select` like any picked component.
    pub fn commit_component_row(on_select: OnExpression) -> ComponentRowBuilder {
        Arc::new(move |id, item, _cx| {
            let name = item.label.clone();
            let on_select = on_select.clone();
            Some(Box::new(CandidateRow {
                item: item.clone(),
                action: CandidateAction::Commit(Arc::new(move |window, cx| {
                    on_select(id, name.clone(), window, cx)
                })),
            }) as Box<dyn InspectorRow>)
        })
    }
}

impl InspectorRow for ExpressionRow {
    fn label(&self) -> &str {
        "Expression"
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
                    .text_size(px(11.0))
                    .text_color(theme.text_tertiary)
                    .child(SharedString::new_static(
                        "type a name to search — an expression computes a channel",
                    )),
            )
            .into_any_element()
    }

    fn activate(&mut self, _window: &mut Window, _cx: &mut App) -> RowAction {
        RowAction::Handled
    }

    /// A hint is not a choice: keeping it out of the arrow-key path leaves
    /// the default selection on a row Enter can actually take — the wizard's
    /// Continue after popping back from an element page, a component in a
    /// single-pick list.
    fn is_header(&self) -> bool {
        true
    }

    fn query_rows(
        &self,
        query: &str,
        cursor: usize,
        cx: &mut App,
    ) -> Option<Vec<Box<dyn InspectorRow>>> {
        if query.is_empty() {
            return None;
        }
        // The sigil is part of the query, never of the expression: strip it
        // and keep the offset so replace ranges map back onto the field.
        let sigil = expressions::is_expression(query);
        let off = if sigil { SIGIL.len_utf8() } else { 0 };
        let body = &query[off..];
        let cursor = cursor.saturating_sub(off).min(body.len());

        let resolver = completion::resolver(&self.db);
        let mut comps = metor_expr::complete::complete(
            body,
            cursor as u32,
            Scope::Expression,
            resolver.as_ref(),
            None,
        );

        // A query made only of name characters and spaces is a search —
        // spaces are how fuzzy queries narrow, not how expressions start.
        // Any operator, or the sigil, means an expression is being built.
        let searching = !sigil
            && !body.trim().is_empty()
            && body
                .chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | ' '));
        let mut rows: Vec<Box<dyn InspectorRow>> = Vec::new();

        let mut ids: std::collections::HashMap<String, ComponentId> = Default::default();
        let mut compute = None;
        if searching {
            // The engine offers only what the language can read (f64-shaped
            // components); a search must keep offering everything, so the
            // component candidates are rebuilt from the full list.
            comps
                .items
                .retain(|item| item.kind != CompletionKind::Component);
            for (id, name) in crate::inspector::trace_picker::list_components(&self.db) {
                let detail = resolver
                    .component(&name)
                    .map(|s| s.ty.to_string())
                    .unwrap_or_default();
                ids.insert(name.clone(), id);
                comps.items.push(CompletionItem {
                    label: name.clone(),
                    detail,
                    kind: CompletionKind::Component,
                    insert: name,
                    caret: None,
                });
            }
            completion::rank_search(&mut comps, body.trim());
        } else {
            let row = ComputeRow::new(self.db.clone(), query.to_string(), self.on_select.clone());
            // A compiling expression is the answer, so committing it is the
            // default Enter; one that does not compile yet is only feedback,
            // and the candidates — the way forward — take the selection.
            match row.complaint.is_none() {
                true => rows.push(Box::new(row)),
                false => compute = Some(row),
            }
            completion::rank(&mut comps);
        }

        let replace_start = off + comps.replace.start as usize;
        let replace_end = off + comps.replace.end as usize;
        for item in &comps.items {
            if searching && item.kind == CompletionKind::Component {
                let Some(id) = ids.get(&item.label) else {
                    continue;
                };
                if let Some(row) = (self.component_row)(*id, item, cx) {
                    rows.push(Box::new(Completing {
                        row,
                        text: item.insert.clone(),
                    }));
                }
                continue;
            }
            let mut text = String::with_capacity(query.len() + item.insert.len());
            text.push_str(&query[..replace_start]);
            text.push_str(&item.insert);
            text.push_str(&query[replace_end..]);
            let caret = replace_start + item.caret.map(|c| c as usize).unwrap_or(item.insert.len());
            rows.push(Box::new(CandidateRow {
                item: item.clone(),
                action: CandidateAction::Insert { text, caret },
            }));
        }

        if let Some(row) = compute {
            rows.push(Box::new(row));
        }
        if let Some(tail) = &self.tail {
            rows.push(tail());
        }
        Some(rows)
    }
}

/// What accepting a candidate does: rewrite the query, or commit a component
/// the way the page's list rows would.
enum CandidateAction {
    Insert { text: String, caret: usize },
    Commit(Arc<dyn Fn(&mut Window, &mut App) -> RowAction>),
}

/// One candidate line. Enter runs its action; Tab always inserts, so a
/// commit row can still be taken as text to keep typing from.
struct CandidateRow {
    item: CompletionItem,
    action: CandidateAction,
}

impl InspectorRow for CandidateRow {
    fn label(&self) -> &str {
        &self.item.label
    }

    fn render_row(
        &self,
        row_ix: usize,
        selected: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let budget = super::label_budget(cx);
        row_base(row_ix, selected, cx)
            .child(completion::candidate_content(
                &self.item, budget, window, cx,
            ))
            .into_any_element()
    }

    fn activate(&mut self, window: &mut Window, cx: &mut App) -> RowAction {
        match &self.action {
            CandidateAction::Insert { text, caret } => RowAction::ReplaceQuery {
                text: text.clone(),
                cursor: *caret,
            },
            CandidateAction::Commit(commit) => commit(window, cx),
        }
    }

    fn insert(&mut self, _search: &str, _window: &mut Window, _cx: &mut App) -> RowAction {
        match &self.action {
            CandidateAction::Insert { text, caret } => RowAction::ReplaceQuery {
                text: text.clone(),
                cursor: *caret,
            },
            // A commit row's text is its label standing alone.
            CandidateAction::Commit(_) => RowAction::ReplaceQuery {
                cursor: self.item.insert.len(),
                text: self.item.insert.clone(),
            },
        }
    }
}

/// A page's own component row, with Tab added.
///
/// The page spells a matched component however it likes — a commit line, a
/// drill-in to its elements — and Enter keeps that meaning. Tab is the
/// provider's: it puts the component's name into the field, so a search can
/// become the start of an expression without retyping the name.
struct Completing {
    row: Box<dyn InspectorRow>,
    text: String,
}

impl InspectorRow for Completing {
    fn label(&self) -> &str {
        self.row.label()
    }

    fn render_row(
        &self,
        row_ix: usize,
        selected: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        self.row.render_row(row_ix, selected, window, cx)
    }

    fn activate(&mut self, window: &mut Window, cx: &mut App) -> RowAction {
        self.row.activate(window, cx)
    }

    fn activate_with_search(
        &mut self,
        search: &str,
        window: &mut Window,
        cx: &mut App,
    ) -> RowAction {
        self.row.activate_with_search(search, window, cx)
    }

    fn consumes_search(&self) -> bool {
        self.row.consumes_search()
    }

    fn is_header(&self) -> bool {
        self.row.is_header()
    }

    fn insert(&mut self, _search: &str, _window: &mut Window, _cx: &mut App) -> RowAction {
        RowAction::ReplaceQuery {
            cursor: self.text.len(),
            text: self.text.clone(),
        }
    }
}

/// The pinned commit line of an expression query.
///
/// Checked — not started — on every keystroke: `compile_expr` is the real
/// gate and cheap without a wasm engine behind it, so the row always knows
/// whether Enter would work and can show the compiler's first complaint
/// while it would not. Activating a compiling expression starts it through
/// [`expressions::resolve`] — unless the body is exactly one component, in
/// which case the component is handed over directly and nothing is computed:
/// `=adcs.omega_b` *is* `adcs.omega_b`.
struct ComputeRow {
    db: Arc<DB>,
    query: String,
    on_select: OnExpression,
    complaint: Option<String>,
}

impl ComputeRow {
    fn new(db: Arc<DB>, query: String, on_select: OnExpression) -> Self {
        let resolver = completion::resolver(&db);
        let body = expressions::body(&query);
        let complaint =
            match body.is_empty() {
                true => Some("an empty expression".to_string()),
                false => metor_expr::compile_expr(body, resolver.as_ref())
                    .err()
                    .map(|diags| match diags.iter().next() {
                        Some(first) => first.message.clone(),
                        None => "this expression could not be compiled".to_string(),
                    }),
            };
        Self {
            db,
            query,
            on_select,
            complaint,
        }
    }
}

impl InspectorRow for ComputeRow {
    fn label(&self) -> &str {
        "compute"
    }

    fn render_row(
        &self,
        row_ix: usize,
        selected: bool,
        _window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let theme = theme(cx);
        let (detail, tint) = match &self.complaint {
            Some(why) => (SharedString::from(why.clone()), theme.error_accent),
            None => (
                SharedString::new_static("computes a channel"),
                theme.text_tertiary,
            ),
        };
        row_base(row_ix, selected, cx)
            .child(
                div()
                    .flex()
                    .gap_2()
                    .items_center()
                    .min_w_0()
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme.text_primary)
                            .truncate()
                            .child(SharedString::from(format!(
                                "compute: {}",
                                expressions::body(&self.query)
                            ))),
                    )
                    .child(div().text_size(px(11.0)).text_color(tint).child(detail)),
            )
            .into_any_element()
    }

    fn activate(&mut self, window: &mut Window, cx: &mut App) -> RowAction {
        if self.complaint.is_some() {
            return RowAction::Handled;
        }
        let body = expressions::body(&self.query);
        // One component needs no computing: bind it as itself, with its own
        // name as the serialized text.
        let resolver = completion::resolver(&self.db);
        if let Some(id) = resolver.id_of(body.trim()) {
            return (self.on_select)(id, body.trim().to_string(), window, cx);
        }
        match expressions::resolve(&self.query, &self.db, cx) {
            Ok(expression) => {
                let text = format!("{SIGIL}{body}");
                (self.on_select)(expression.component_id(), text, window, cx)
            }
            // Compiled a moment ago but would not start — a port stopped
            // publishing, or the worker is gone. Stay open with the reason.
            Err(why) => {
                self.complaint = Some(why.to_string());
                RowAction::Handled
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inspector::rows::NavRow;
    use metor_db::ComponentSchema;
    use metor_proto::types::PrimType;

    fn db_with(names: &[&str]) -> (Arc<DB>, tempfile::TempDir) {
        let temp = tempfile::tempdir().unwrap();
        let db = DB::create(temp.path().join("db")).unwrap();
        for name in names {
            let id = ComponentId::new(name);
            db.with_state_mut(|s| {
                s.insert_component(id, ComponentSchema::new(PrimType::F64, &[3][..]), &db.path)
            })
            .unwrap();
            let mut metadata = metor_proto_wkt::ComponentMetadata {
                component_id: id,
                name: name.to_string(),
                metadata: Default::default(),
            };
            use metor_proto_wkt::MetadataExt;
            metadata.set("source", "test");
            db.with_state_mut(|s| s.set_component_metadata(metadata, &db.path))
                .unwrap();
        }
        (Arc::new(db), temp)
    }

    /// The wizard spells a matched component as a drill-in row. Enter keeps
    /// drilling; Tab must still put the name into the field, which is how a
    /// search becomes the first operand of an expression.
    #[gpui::test]
    fn tab_on_a_pages_own_component_row_inserts_its_name(cx: &mut gpui::TestAppContext) {
        let (db, _temp) = db_with(&["cube_sat.plant.body.omega_b"]);
        let cx = cx.add_empty_window();
        cx.update(|window, cx| {
            crate::theme::set_theme(cx, Arc::new(crate::theme::DARK.clone()));
            let drill: ComponentRowBuilder = Arc::new(|_id, item, _cx| {
                Some(Box::new(NavRow::new(
                    SharedString::from(item.label.clone()),
                    SharedString::new_static(""),
                    Box::new(|_cx| Vec::new()),
                )) as Box<dyn InspectorRow>)
            });
            let provider =
                ExpressionRow::new(db, Arc::new(|_, _, _, _| RowAction::Dismiss), drill, None);
            let mut rows = provider
                .query_rows("omega", 5, cx)
                .expect("a query takes the page");
            let row = rows
                .iter_mut()
                .find(|r| r.label() == "cube_sat.plant.body.omega_b")
                .expect("matched");
            let action = row.insert("omega", window, cx);
            let RowAction::ReplaceQuery { text, cursor } = action else {
                panic!("Tab must rewrite the query");
            };
            assert_eq!(text, "cube_sat.plant.body.omega_b");
            assert_eq!(cursor, text.len());
        });
    }
}
