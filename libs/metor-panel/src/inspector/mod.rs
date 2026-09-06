//! The unified inspector: command palette and right-click property editor.
//!
//! The inspector is a row-list backed by a page stack. Any action that needs
//! to "drill into" another view pushes a new page instead of opening a
//! separate window, giving the palette and the property inspector a single
//! implementation and a consistent keyboard model. [`InspectorMode`]
//! controls whether the panel anchors to a point or centers in the window.
use std::ops::Range;
use std::sync::Arc;

use gpui::{
    AnyElement, AnyView, App, Bounds, Context, Corner, FocusHandle, Focusable, IntoElement,
    KeyDownEvent, Pixels, Point, Render, ScrollStrategy, SharedString, Size,
    UniformListScrollHandle, Window, anchored, canvas, deferred, div, prelude::*, px, uniform_list,
};

pub mod completion;
pub mod drag_paint;
pub mod edits;
#[cfg(test)]
mod motion_tests;
pub mod palette;
pub mod plot_preview;
pub mod reflect;
pub mod registry;
pub mod row_list;
pub mod rows;
pub mod trace_picker;

use crate::motion::{self, Fade};
use crate::theme::theme;
use rows::text_field::TextAlign;
use rows::{InspectorRow, PreviewSpec, RowAction, TextField, tag_pill};

const ROW_HEIGHT: f32 = 28.0;
/// Horizontal padding inside a row, both sides together.
const ROW_PADDING: Pixels = px(24.0);
/// What a row needs beyond its label: padding, and room for a chevron or a
/// short summary.
const ROW_CHROME: Pixels = px(72.0);
const ANCHORED_MIN_WIDTH: Pixels = px(280.0);
const ANCHORED_MAX_WIDTH: Pixels = px(480.0);
const CENTERED_WIDTH: Pixels = px(500.0);

/// gpui action that asks the root view to inspect `entity` anchored at
/// `position`. Dispatched from deep in the view tree so inspection can reach
/// across pane boundaries without threading callbacks.
#[derive(Clone, PartialEq, gpui::Action)]
#[action(no_json)]
pub struct InspectEntity {
    pub entity: gpui::AnyEntity,
    pub position: Point<Pixels>,
}

/// Graphical draft edits feed the same query field used by text completion.
#[derive(Clone, PartialEq, gpui::Action)]
#[action(no_json)]
pub struct EditInspectorQuery {
    pub text: String,
}

/// Where the inspector draws itself relative to the window.
#[derive(Clone, Copy)]
pub enum InspectorMode {
    /// Snap the panel to a window point, used for right-click menus.
    Anchored(Point<Pixels>),
    /// Float near the top-center, used for the command palette.
    Centered,
    /// Draw as a plain child of a host view rather than a floating overlay:
    /// no anchoring, no deferred layer, no dismiss-on-outside-click. The
    /// host sizes the surrounding column and polls
    /// [`dismissed`](Inspector::dismissed) the way the window root does.
    /// Lets a dialog embed a row form and inherit the whole keyboard model —
    /// page stack, breadcrumbs, search, inline edit — instead of
    /// reimplementing it.
    Inline,
}

/// Inputs needed to open an inspector from a callback.
pub struct InspectorRequest {
    pub rows: Vec<Box<dyn InspectorRow>>,
    pub mode: InspectorMode,
}

/// Callback installed by subsystems that want the ability to open an
/// inspector without holding a direct reference to the root view.
pub type OpenInspectorCallback = Arc<dyn Fn(InspectorRequest, &mut Window, &mut App) + 'static>;

/// App-wide handle to the current window's inspector opener.
///
/// Populated by [`AppRoot::new`] so leaf views can reach the inspector
/// without threading the callback through every constructor. Overwritten
/// by each new window; single-window apps see exactly one installation.
pub struct OpenInspectorGlobal(pub OpenInspectorCallback);

impl gpui::Global for OpenInspectorGlobal {}

/// Grab the registered callback, if any. `None` during early startup
/// before any window has been opened.
pub fn open_inspector(cx: &App) -> Option<OpenInspectorCallback> {
    cx.try_global::<OpenInspectorGlobal>().map(|g| g.0.clone())
}

/// What a single inspector page renders.
///
/// `Rows` is the standard fuzzy-searchable list. `View` embeds an arbitrary
/// gpui view (e.g. a transient plot), reusing the inspector's overlay chrome
/// — anchored/centered placement, click-outside dismiss, page stack, focus
/// restoration — without forcing the content into the row model.
enum InspectorPageKind {
    Rows(Vec<Box<dyn InspectorRow>>),
    View { view: AnyView, size: Size<Pixels> },
}

struct InspectorPage {
    kind: InspectorPageKind,
    label: Option<SharedString>,
}

/// Row-list panel with page-stack navigation, fuzzy search, and inline
/// editing. Rendered as either an anchored popup or a centered overlay
/// depending on [`InspectorMode`].
pub struct Inspector {
    pages: Vec<InspectorPage>,
    live_palette: Option<palette::LivePalette>,
    mode: InspectorMode,
    focus_handle: FocusHandle,
    parent_focus: Option<FocusHandle>,
    pub dismissed: bool,
    fade: Fade,
    exit_complete: bool,
    search: TextField,
    /// What a provider row made of the current query, when one did. Shown in
    /// place of the page's fuzzy-filtered rows and re-asked on every query
    /// change; owned here because the page's own rows stay untouched behind
    /// it. See [`InspectorRow::query_rows`].
    query_rows: Option<Vec<Box<dyn InspectorRow>>>,
    query_revision: u64,
    selected_index: usize,
    editing: Option<EditState>,
    panel_bounds: Option<Bounds<Pixels>>,
    accessory: Option<rows::AccessorySpec>,
    accessory_expanded: bool,
    scroll_handle: UniformListScrollHandle,
    /// `false` for hover-style previews. Suppresses both keyboard focus
    /// capture and the full-window occluding overlay used for
    /// click-outside dismissal — together those keep the underlying
    /// surface fully interactive while the preview is up.
    dismiss_on_outside_click: bool,
}

struct EditState {
    row_index: usize,
    field: TextField,
    on_commit: Option<Box<dyn FnOnce(String, &mut Window, &mut App)>>,
}

impl Inspector {
    pub fn new(
        rows: Vec<Box<dyn InspectorRow>>,
        mode: InspectorMode,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self::from_page(
            InspectorPage {
                kind: InspectorPageKind::Rows(rows),
                label: None,
            },
            mode,
            cx,
        );
        this.seed_query(cx);
        this.selected_index = this.first_selectable_index();
        this
    }

    /// Open the inspector on a view-hosting page rather than a row list.
    /// The spec's `size` is the preferred panel size; the inspector
    /// forwards it to the embedded view's container.
    pub fn with_view(spec: PreviewSpec, mode: InspectorMode, cx: &mut Context<Self>) -> Self {
        Self::from_page(
            InspectorPage {
                kind: InspectorPageKind::View {
                    view: spec.view,
                    size: spec.size,
                },
                label: Some(spec.label),
            },
            mode,
            cx,
        )
    }

    fn from_page(page: InspectorPage, mode: InspectorMode, cx: &mut Context<Self>) -> Self {
        Self {
            pages: vec![page],
            live_palette: None,
            mode,
            focus_handle: cx.focus_handle(),
            parent_focus: None,
            dismissed: false,
            fade: Fade::entrance(match mode {
                InspectorMode::Centered => motion::PALETTE_ENTER,
                InspectorMode::Anchored(_) => motion::MENU_ENTER,
                InspectorMode::Inline => std::time::Duration::ZERO,
            }),
            exit_complete: false,
            search: TextField::new("Search...", cx),
            query_rows: None,
            query_revision: 0,
            selected_index: 0,
            editing: None,
            panel_bounds: None,
            accessory: None,
            accessory_expanded: matches!(mode, InspectorMode::Centered),
            scroll_handle: UniformListScrollHandle::new(),
            dismiss_on_outside_click: true,
        }
    }

    pub(crate) fn follow_palette(
        &mut self,
        db: Arc<metor_db::DB>,
        tiles: &gpui::Entity<crate::tiles::TileGroup>,
    ) {
        self.live_palette = Some(palette::LivePalette::new(db, tiles));
    }

    fn refresh_palette(&mut self, cx: &mut Context<Self>) {
        if self.dismissed {
            return;
        }
        let Some(source) = &mut self.live_palette else {
            return;
        };
        let Some(rows) = source.refresh(cx) else {
            return;
        };
        let selected = self
            .filtered_indices()
            .get(self.selected_index)
            .and_then(|&index| self.current_rows()?.get(index))
            .map(|row| row.identity());
        self.pages[0].kind = InspectorPageKind::Rows(rows);
        if self.pages.len() == 1 {
            self.refresh_query_rows(cx);
            self.selected_index = selected
                .and_then(|key| {
                    self.filtered_indices()
                        .iter()
                        .position(|&index| self.current_rows().unwrap()[index].identity() == key)
                })
                .unwrap_or_else(|| self.first_selectable_index());
        }
    }

    pub fn set_parent_focus(&mut self, handle: FocusHandle) {
        self.parent_focus = Some(handle);
    }

    /// Make the inspector a passive overlay: no focus capture, no
    /// click-outside dismissal. The caller becomes responsible for the
    /// lifecycle (typically dropping the entity on some external signal).
    pub fn set_passive(&mut self) {
        self.dismiss_on_outside_click = false;
        self.fade = Fade::settled(1.0);
    }

    fn current_page(&self) -> &InspectorPage {
        self.pages.last().expect("page stack must never be empty")
    }

    pub(crate) fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dismissed {
            return;
        }
        self.dismissed = true;
        self.fade.exit(match self.mode {
            InspectorMode::Centered => motion::PALETTE_EXIT,
            _ => motion::MENU_EXIT,
        });
        let can_fade = self.dismiss_on_outside_click
            && !matches!(self.mode, InspectorMode::Inline)
            && self.accessory.is_none()
            && self
                .current_rows()
                .is_some_and(|rows| rows.iter().all(|row| row.supports_exit_fade()));
        if !can_fade || !motion::enabled(cx) {
            self.fade.finish();
        }
        if self.focus_handle.contains_focused(window, cx) {
            if let Some(parent) = &self.parent_focus {
                parent.focus(window);
            } else {
                window.blur();
            }
        }
        cx.notify();
    }

    fn push_page(
        &mut self,
        outgoing_label: Option<SharedString>,
        incoming_label: Option<SharedString>,
        kind: InspectorPageKind,
        cx: &mut Context<Self>,
    ) {
        // The outgoing page's label becomes a breadcrumb pill above the search box.
        if let Some(current) = self.pages.last_mut()
            && current.label.is_none()
        {
            current.label = outgoing_label;
        }
        self.pages.push(InspectorPage {
            kind,
            label: incoming_label,
        });
        self.search.clear();
        self.query_rows = None;
        self.seed_query(cx);
        self.selected_index = self.first_selectable_index();
        cx.notify();
    }

    fn pop_page(&mut self, cx: &mut Context<Self>) -> bool {
        if self.pages.len() > 1 {
            self.pages.pop();
            if let Some(current) = self.pages.last_mut() {
                current.label = None;
            }
            self.search.clear();
            self.query_rows = None;
            self.seed_query(cx);
            self.selected_index = self.first_selectable_index();
            cx.notify();
            true
        } else {
            false
        }
    }

    /// The rows the panel currently shows: a provider's take on the query
    /// when there is one, the page's own rows otherwise.
    fn current_rows(&self) -> Option<&[Box<dyn InspectorRow>]> {
        if let Some(rows) = &self.query_rows {
            return Some(rows.as_slice());
        }
        match &self.current_page().kind {
            InspectorPageKind::Rows(rows) => Some(rows.as_slice()),
            InspectorPageKind::View { .. } => None,
        }
    }

    /// Initialize the page's search placeholder and query, then compute its rows.
    fn seed_query(&mut self, cx: &mut App) {
        let placeholder = match &self.current_page().kind {
            InspectorPageKind::Rows(rows) => rows.iter().find_map(|r| r.query_placeholder()),
            _ => None,
        }
        .unwrap_or("Search...")
        .to_string();
        self.search.set_placeholder(&placeholder);
        if let InspectorPageKind::Rows(rows) = &self.current_page().kind
            && let Some(query) = rows.iter().find_map(|row| row.initial_query())
        {
            self.search.set_text(query);
            self.search.cursor = self.search.text.len();
            self.search.mark = self.search.cursor;
        }
        self.refresh_query_rows(cx);
    }

    fn query_edited(&mut self, cx: &mut App) {
        if let InspectorPageKind::Rows(rows) = &self.current_page().kind {
            for row in rows {
                row.query_edited(&self.search.text, cx);
            }
        }
        self.refresh_query_rows(cx);
    }

    /// The first provider that answers supplies the rows, including for an empty
    /// query. If no provider answers, the inspector uses the page's own rows.
    fn refresh_query_rows(&mut self, cx: &mut App) {
        self.query_rows = None;
        let query = self.search.text.clone();
        let cursor = self.search.cursor;
        let computed = match &self.current_page().kind {
            InspectorPageKind::Rows(rows) => rows
                .iter()
                .find_map(|row| row.query_rows(&query, cursor, cx)),
            InspectorPageKind::View { .. } => None,
        };
        self.query_rows = computed;
    }

    fn filtered_indices(&self) -> Vec<usize> {
        let Some(rows) = self.current_rows() else {
            return Vec::new();
        };
        let filter = &self.search.text;
        // A provider's rows are already the answer to the query — fuzzy
        // re-filtering them against expression text would drop candidates
        // whose labels don't happen to resemble the whole expression.
        if filter.is_empty() || self.query_rows.is_some() {
            return (0..rows.len()).collect();
        }

        use nucleo_matcher::{
            Matcher,
            pattern::{CaseMatching, Normalization, Pattern},
        };

        let mut matcher = Matcher::new(nucleo_matcher::Config::DEFAULT);
        let pattern = Pattern::parse(filter, CaseMatching::Ignore, Normalization::Smart);

        let mut scored: Vec<(usize, u32)> = rows
            .iter()
            .enumerate()
            .filter_map(|(i, row)| {
                // Section headers describe the unfiltered grouping; once a
                // query is active the results are already narrowed, so a
                // header (possibly now empty) only adds noise.
                if row.is_header() {
                    return None;
                }
                // Search-consuming rows (prompts / filter builders) stay
                // pinned to the bottom so the query-as-input affordance
                // is always reachable, even when the query doesn't
                // fuzzy-match the hint label.
                if row.consumes_search() {
                    return Some((i, 0));
                }
                let mut buf = Vec::new();
                let haystack = nucleo_matcher::Utf32Str::new(row.label(), &mut buf);
                let score = pattern.score(haystack, &mut matcher)?;
                Some((i, score))
            })
            .collect();

        scored.sort_by_key(|a| std::cmp::Reverse(a.1));
        scored.into_iter().map(|(i, _)| i).collect()
    }

    /// Whether the row at visible position `vis_ix` is a non-selectable
    /// section header. `filtered` is passed in so callers that already hold
    /// the index list don't recompute it.
    fn is_header_at(&self, filtered: &[usize], vis_ix: usize) -> bool {
        let Some(rows) = self.current_rows() else {
            return false;
        };
        filtered
            .get(vis_ix)
            .and_then(|&i| rows.get(i))
            .is_some_and(|r| r.is_header())
    }

    /// First visible position that isn't a section header, so the initial
    /// and post-filter selection lands on an activatable row rather than a
    /// group label.
    fn first_selectable_index(&self) -> usize {
        let filtered = self.filtered_indices();
        (0..filtered.len())
            .find(|&vis_ix| !self.is_header_at(&filtered, vis_ix))
            .unwrap_or(0)
    }

    fn current_rows_mut(&mut self) -> Option<&mut [Box<dyn InspectorRow>]> {
        if self.query_rows.is_some() {
            return self.query_rows.as_deref_mut();
        }
        match &mut self.pages.last_mut()?.kind {
            InspectorPageKind::Rows(rows) => Some(rows),
            InspectorPageKind::View { .. } => None,
        }
    }

    fn activate_row(&mut self, row_idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.dismissed {
            return;
        }
        let search = self.search.text.clone();
        let Some(rows) = self.current_rows_mut() else {
            return;
        };
        let Some(row) = rows.get_mut(row_idx) else {
            return;
        };
        let action = row.activate_with_search(&search, window, cx);
        self.handle_action(action, row_idx, window, cx);
    }

    /// Tab on the selected row: insert instead of commit.
    fn insert_row(&mut self, row_idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        let search = self.search.text.clone();
        let Some(rows) = self.current_rows_mut() else {
            return;
        };
        let Some(row) = rows.get_mut(row_idx) else {
            return;
        };
        let action = row.insert(&search, window, cx);
        self.handle_action(action, row_idx, window, cx);
    }

    fn handle_action(
        &mut self,
        action: RowAction,
        row_idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            RowAction::Handled => {
                cx.notify();
            }
            RowAction::Cascade(child_rows) => {
                let outgoing = self
                    .current_rows()
                    .and_then(|rows| rows.get(row_idx))
                    .map(|r| SharedString::from(r.label().to_string()));
                self.push_page(outgoing, None, InspectorPageKind::Rows(child_rows), cx);
            }
            RowAction::CascadeWith { rows, query } => {
                let outgoing = self
                    .current_rows()
                    .and_then(|rows| rows.get(row_idx))
                    .map(|r| SharedString::from(r.label().to_string()));
                self.push_page(outgoing, None, InspectorPageKind::Rows(rows), cx);
                self.search.set_text(query);
                self.refresh_query_rows(cx);
                self.selected_index = self.first_selectable_index();
                cx.notify();
            }
            RowAction::CascadeView(spec) => {
                let outgoing = self
                    .current_rows()
                    .and_then(|rows| rows.get(row_idx))
                    .map(|r| SharedString::from(r.label().to_string()));
                self.push_page(
                    outgoing,
                    Some(spec.label),
                    InspectorPageKind::View {
                        view: spec.view,
                        size: spec.size,
                    },
                    cx,
                );
            }
            RowAction::Pop => {
                self.pop_page(cx);
            }
            RowAction::Dismiss => {
                self.dismiss(window, cx);
            }
            RowAction::ReplaceQuery { text, cursor } => {
                self.search.text = text;
                self.search.cursor = cursor.min(self.search.text.len());
                self.search.mark = self.search.cursor;
                self.query_edited(cx);
                self.selected_index = self.first_selectable_index();
                cx.notify();
            }
            RowAction::StartEdit {
                current_text,
                on_commit,
            } => {
                let mut edit_field = TextField::new("", cx);
                edit_field.text = current_text;
                edit_field.cursor = edit_field.text.len();
                edit_field.mark = edit_field.cursor;
                edit_field.set_align(TextAlign::Right);
                self.editing = Some(EditState {
                    row_index: row_idx,
                    field: edit_field,
                    on_commit: Some(on_commit),
                });
                cx.notify();
            }
        }
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let indices = self.filtered_indices();
        let Some(&row_idx) = indices.get(self.selected_index) else {
            return;
        };
        if self.is_header_at(&indices, self.selected_index) {
            return;
        }
        self.activate_row(row_idx, window, cx);
    }

    fn commit_edit(&mut self, window: &mut Window, cx: &mut App) {
        if let Some(mut edit) = self.editing.take()
            && let Some(on_commit) = edit.on_commit.take()
        {
            on_commit(edit.field.text.clone(), window, cx);
        }
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = event.keystroke.key.as_str();
        if event.keystroke.modifiers.alt && key == "t" {
            if let Some(accessory) = &self.accessory {
                accessory.focus.focus(window);
            }
            cx.stop_propagation();
            return;
        }
        if event.keystroke.modifiers.alt && key == "e" && self.accessory.is_some() {
            self.accessory_expanded = !self.accessory_expanded;
            cx.stop_propagation();
            cx.notify();
            return;
        }

        // View pages only respond to navigation keys; everything else is
        // ignored so it can propagate to the embedded view's focus tree.
        if matches!(self.current_page().kind, InspectorPageKind::View { .. }) {
            match key {
                "escape" => {
                    if !self.pop_page(cx) {
                        self.dismiss(window, cx);
                    }
                    cx.notify();
                }
                "backspace" if self.pages.len() > 1 => {
                    self.pop_page(cx);
                }
                _ => {}
            }
            return;
        }

        if self.editing.is_some() {
            match key {
                "escape" => {
                    self.editing = None;
                    cx.notify();
                    return;
                }
                "enter" | "return" => {
                    self.commit_edit(window, cx);
                    cx.notify();
                    return;
                }
                _ => {
                    if let Some(edit) = &mut self.editing {
                        edit.field.handle_key_down(event, cx);
                    }
                    cx.notify();
                    return;
                }
            }
        }

        match key {
            "escape" => {
                if !self.pop_page(cx) {
                    self.dismiss(window, cx);
                }
                cx.notify();
                return;
            }
            "backspace" => {
                if self.search.text.is_empty() && self.pages.len() > 1 {
                    self.pop_page(cx);
                    return;
                }
            }
            "up" => {
                let filtered = self.filtered_indices();
                let mut i = self.selected_index;
                while i > 0 {
                    i -= 1;
                    if !self.is_header_at(&filtered, i) {
                        self.selected_index = i;
                        self.scroll_handle.scroll_to_item(i, ScrollStrategy::Top);
                        break;
                    }
                }
                cx.notify();
                return;
            }
            "down" => {
                let filtered = self.filtered_indices();
                let total = filtered.len();
                let mut i = self.selected_index;
                while i + 1 < total {
                    i += 1;
                    if !self.is_header_at(&filtered, i) {
                        self.selected_index = i;
                        self.scroll_handle.scroll_to_item(i, ScrollStrategy::Bottom);
                        break;
                    }
                }
                cx.notify();
                return;
            }
            "enter" | "return" => {
                self.confirm(window, cx);
                cx.notify();
                return;
            }
            "tab" => {
                let filtered = self.filtered_indices();
                if let Some(&row_idx) = filtered.get(self.selected_index)
                    && !self.is_header_at(&filtered, self.selected_index)
                {
                    self.insert_row(row_idx, window, cx);
                }
                cx.notify();
                return;
            }
            _ => {}
        }

        let previous = self.search.text.clone();
        if self.search.handle_key_down(event, cx) {
            if self.search.text != previous {
                self.query_edited(cx);
            } else {
                self.refresh_query_rows(cx);
            }
            self.selected_index = self.first_selectable_index();
        }
    }

    fn render_input_bar(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let mut bar = div()
            .flex()
            .flex_row()
            .items_center()
            .w_full()
            .min_w_0()
            .overflow_hidden()
            .px(px(8.0))
            .py(px(4.0))
            .border_b_1()
            .border_color(theme.border_primary)
            .text_size(px(12.0));

        // The trail is context, the field is the point: only the nearest two
        // crumbs show, each capped, and anything before them folds into one
        // ellipsis so the field always keeps its room.
        let crumbs: Vec<&SharedString> = self.pages[..self.pages.len().saturating_sub(1)]
            .iter()
            .filter_map(|page| page.label.as_ref())
            .collect();
        let shown = crumbs.len().min(2);
        let mut labels: Vec<SharedString> = Vec::with_capacity(shown + 1);
        if crumbs.len() > shown {
            labels.push(SharedString::new_static("…"));
        }
        labels.extend(crumbs[crumbs.len() - shown..].iter().map(|l| (*l).clone()));
        for label in labels {
            bar = bar.child(
                div()
                    .flex_none()
                    .max_w(px(140.0))
                    .truncate()
                    .px(px(6.0))
                    .py(px(1.0))
                    .mr(px(4.0))
                    .bg(theme.pill_bg)
                    .border_1()
                    .border_color(theme.pill_border)
                    .rounded(px(3.0))
                    .text_size(px(10.0))
                    .text_color(theme.text_secondary)
                    .child(label),
            );
        }

        bar = bar.child(div().flex_1().min_w(px(60.0)).child(self.search.element()));
        if self.accessory.is_some() {
            bar = bar.child(
                div()
                    .id("time-preview-size")
                    .px(px(4.0))
                    .cursor_pointer()
                    .child(
                        if self.accessory_expanded {
                            crate::icons::Icon::ChevronUp
                        } else {
                            crate::icons::Icon::ChevronDown
                        }
                        .svg_color(12.0, theme.text_secondary),
                    )
                    .tooltip(|_, cx| {
                        crate::views::TooltipText::build(
                            "Expand/collapse timeline (Alt-E); focus timeline (Alt-T)".into(),
                            cx,
                        )
                    })
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.accessory_expanded = !this.accessory_expanded;
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    ),
            );
        }

        bar
    }

    fn render_panel(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let theme = theme(cx);

        let view = cx.entity().clone();
        let bounds_tracker = canvas(
            move |bounds, _window, cx| {
                view.update(cx, |this, _| {
                    this.panel_bounds = Some(bounds);
                });
            },
            |_, _, _, _| {},
        )
        .size_full()
        .absolute();

        let frame = div()
            .relative()
            .flex()
            .flex_col()
            .id("inspector-panel")
            // Names this subtree so a leader keybinding gated on `!Inspector`
            // is suppressed while the search field has focus.
            .when(!self.dismissed, |frame| {
                frame
                    .key_context("Inspector TextInput")
                    .track_focus(&self.focus_handle)
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                        this.handle_key_down(event, window, cx);
                        cx.notify();
                    }))
            })
            .bg(theme.bg_elevated)
            .border_1()
            .border_color(theme.border_primary)
            .rounded(px(6.0))
            .when(!matches!(self.mode, InspectorMode::Inline), |frame| {
                frame.shadow_sm()
            })
            .child(bounds_tracker);

        let panel = match &self.current_page().kind {
            InspectorPageKind::Rows(_) => self.render_rows_panel(frame, window, cx),
            InspectorPageKind::View { view, size } => {
                let view = view.clone();
                let size = *size;
                self.render_view_panel(frame, view, size, cx)
            }
        };
        let mut group = div()
            .id("inspector-group")
            .flex()
            .flex_col()
            .when(!self.dismissed, |group| {
                group.on_action(cx.listener(|this, action: &EditInspectorQuery, _, cx| {
                    this.search.set_text(action.text.clone());
                    this.search.cursor = this.search.text.len();
                    this.search.mark = this.search.cursor;
                    this.query_edited(cx);
                    this.selected_index = this.first_selectable_index();
                    cx.stop_propagation();
                    cx.notify();
                }))
            })
            .child(panel);
        if matches!(self.mode, InspectorMode::Anchored(_)) && self.accessory.is_some() {
            group = group
                .child(div().h(px(6.0)))
                .child(self.render_accessory(cx));
        }
        group
    }

    fn render_accessory(&self, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let theme = theme(cx);
        let height = if self.accessory_expanded { 112.0 } else { 31.0 };
        div()
            .id("inspector-accessory")
            .relative()
            .w_full()
            .h(px(height))
            .flex_shrink_0()
            .bg(theme.bg_elevated)
            .border_1()
            .border_color(theme.border_primary)
            .rounded(px(4.0))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "tab" {
                    this.focus_handle.focus(window);
                    cx.stop_propagation();
                } else {
                    this.handle_key_down(event, window, cx);
                }
            }))
            .children(self.accessory.as_ref().map(|a| a.view.clone()))
    }

    fn render_rows_panel(
        &mut self,
        frame: gpui::Stateful<gpui::Div>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let theme = theme(cx);
        let indices = self.filtered_indices();

        // An anchored panel is sized to what it lists, within reason: a
        // component's path is often longer than a fixed menu, and cutting
        // it off hides exactly the part that names it. Rows are told the
        // room that leaves so what still does not fit elides from the front.
        let width = match self.mode {
            InspectorMode::Anchored(_) => {
                let rows = self.current_rows().unwrap_or_default();
                let widest = indices
                    .iter()
                    .take(200)
                    .map(|&i| rows::measure(rows[i].label(), rows::LABEL_SIZE, window))
                    .fold(px(0.0), Pixels::max);
                Some(if self.accessory.is_some() {
                    ANCHORED_MAX_WIDTH
                } else {
                    (widest + ROW_CHROME).clamp(ANCHORED_MIN_WIDTH, ANCHORED_MAX_WIDTH)
                })
            }
            InspectorMode::Centered => Some(CENTERED_WIDTH),
            InspectorMode::Inline => None,
        };
        let width = if self.dismissed {
            self.panel_bounds.map(|bounds| bounds.size.width).or(width)
        } else {
            width
        };
        cx.set_global(rows::LabelFit {
            row_width: width.map(|w| w - ROW_PADDING),
        });

        let items_element: AnyElement = if indices.is_empty() {
            div()
                .px(px(12.0))
                .py(px(6.0))
                .text_size(px(12.0))
                .text_color(theme.text_tertiary)
                .child(SharedString::new_static("No results"))
                .into_any_element()
        } else {
            let count = indices.len();
            let scroll_handle = self.scroll_handle.clone();
            let accessory_height = if self.accessory.is_some() {
                if self.accessory_expanded { 118.0 } else { 37.0 }
            } else {
                0.0
            };
            let max_items_h = (f32::from(window.viewport_size().height) - 120.0 - accessory_height)
                .clamp(56.0, 360.0);
            let items_h = if self.accessory.is_some() {
                max_items_h.min(280.0)
            } else {
                (count as f32 * ROW_HEIGHT).min(max_items_h)
            };
            if self.dismissed {
                let offset = self.scroll_handle.0.borrow().base_handle.offset().y;
                let first =
                    ((-f32::from(offset) / ROW_HEIGHT).floor().max(0.0) as usize).min(count);
                let end = (first + (items_h / ROW_HEIGHT).ceil() as usize + 1).min(count);
                let passive_rows = rows::with_passive(cx, |cx| {
                    let rows = self.current_rows().unwrap();
                    (first..end)
                        .map(|vis_ix| {
                            let row = rows[indices[vis_ix]].render_row(
                                vis_ix,
                                vis_ix == self.selected_index,
                                window,
                                cx,
                            );
                            div()
                                .absolute()
                                .top(offset + px(vis_ix as f32 * ROW_HEIGHT))
                                .w_full()
                                .h(px(ROW_HEIGHT))
                                .child(row)
                        })
                        .collect::<Vec<_>>()
                });
                div()
                    .relative()
                    .overflow_hidden()
                    .h(px(items_h))
                    .children(passive_rows)
                    .into_any_element()
            } else {
                uniform_list(
                    "inspector-items",
                    count,
                    cx.processor(
                        move |this: &mut Self,
                              range: Range<usize>,
                              window: &mut Window,
                              cx: &mut Context<Self>| {
                            let theme = crate::theme::theme(cx);
                            let indices = this.filtered_indices();
                            let mut out = Vec::with_capacity(range.len());
                            for vis_ix in range {
                                let row_idx = indices[vis_ix];
                                let selected = vis_ix == this.selected_index;
                                let is_editing = this
                                    .editing
                                    .as_ref()
                                    .is_some_and(|e| e.row_index == row_idx);

                                let Some(rows) = this.current_rows() else {
                                    unreachable!("render_rows_panel is only entered on Rows pages");
                                };

                                let element = if is_editing {
                                    let edit = this.editing.as_ref().unwrap();
                                    let label =
                                        SharedString::from(rows[row_idx].label().to_string());
                                    crate::inspector::rows::row_base(vis_ix, selected, cx)
                                        .child(
                                            div()
                                                .text_size(px(12.0))
                                                .text_color(theme.text_primary)
                                                .child(label),
                                        )
                                        .child(div().flex_1().min_w_0().child(edit.field.element()))
                                        .into_any_element()
                                } else if rows[row_idx].is_header() {
                                    // Headers are inert: no click-to-select, no
                                    // selection highlight.
                                    rows[row_idx].render_row(vis_ix, selected, window, cx)
                                } else {
                                    let row_element =
                                        rows[row_idx].render_row(vis_ix, selected, window, cx);
                                    let row_idx_click = row_idx;
                                    div()
                                        .on_mouse_down(
                                            gpui::MouseButton::Left,
                                            cx.listener(move |this, _, window, cx| {
                                                this.selected_index = vis_ix;
                                                this.activate_row(row_idx_click, window, cx);
                                            }),
                                        )
                                        .child(row_element)
                                        .into_any_element()
                                };

                                out.push(element);
                            }
                            out
                        },
                    ),
                )
                .track_scroll(scroll_handle)
                .h(px(items_h))
                .into_any_element()
            }
        };

        let frame = match (self.mode, width) {
            (InspectorMode::Anchored(_) | InspectorMode::Centered, Some(width)) => frame.w(width),
            // The host's column owns the width; a border and rounding would
            // draw a panel inside a panel, so inline drops both.
            _ => frame.w_full().border_0().rounded(px(0.0)),
        };

        let mut frame = frame.max_h(px(540.0)).child(self.render_input_bar(cx));
        if !matches!(self.mode, InspectorMode::Anchored(_)) && self.accessory.is_some() {
            frame = frame.child(self.render_accessory(cx));
        }
        frame.child(div().py(px(2.0)).child(items_element))
    }

    fn render_view_panel(
        &self,
        frame: gpui::Stateful<gpui::Div>,
        view: AnyView,
        size: Size<Pixels>,
        cx: &App,
    ) -> gpui::Stateful<gpui::Div> {
        let theme = theme(cx);

        // Breadcrumb pills for parent pages, plus the current page's label
        // as a header (the input bar slot is reused so the chrome matches
        // the rows path).
        let mut bar = div()
            .flex()
            .flex_row()
            .items_center()
            .px(px(8.0))
            .py(px(4.0))
            .border_b_1()
            .border_color(theme.border_primary)
            .text_size(px(12.0));
        let last_ix = self.pages.len().saturating_sub(1);
        for page in &self.pages[..last_ix] {
            if let Some(label) = &page.label {
                bar = bar.child(div().mr(px(4.0)).child(tag_pill(label.clone(), cx)));
            }
        }
        if let Some(label) = self.current_page().label.clone() {
            bar = bar.child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme.text_primary)
                    .child(label),
            );
        }

        let body = div().w(size.width).h(size.height).child(view);

        frame.w(size.width).child(bar).child(body)
    }
}

impl gpui::EventEmitter<motion::Closed> for Inspector {}

impl Focusable for Inspector {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Inspector {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let opacity = self.fade.opacity(window, cx);
        if self.dismissed && opacity == 0.0 {
            if !self.exit_complete {
                self.exit_complete = true;
                cx.emit(motion::Closed);
            }
            return div().into_any_element();
        }
        self.refresh_palette(cx);
        let revision = match &self.current_page().kind {
            InspectorPageKind::Rows(rows) => {
                rows.iter().map(|r| r.query_revision(cx)).max().unwrap_or(0)
            }
            _ => 0,
        };
        if !self.dismissed && revision != self.query_revision {
            let selected = self
                .current_rows()
                .and_then(|rows| rows.get(self.selected_index))
                .map(|r| r.label().to_string());
            self.refresh_query_rows(cx);
            self.query_revision = revision;
            self.selected_index = selected
                .and_then(|label| self.current_rows()?.iter().position(|r| r.label() == label))
                .unwrap_or_else(|| self.first_selectable_index());
        }
        if !self.dismissed {
            self.accessory = match &self.current_page().kind {
                InspectorPageKind::Rows(rows) => {
                    rows.iter().find_map(|r| r.accessory(&self.search.text, cx))
                }
                _ => None,
            };
        }
        let panel = self.render_panel(window, cx);
        if self.dismissed {
            let bounds = self.panel_bounds.unwrap_or_default();
            return deferred(
                div()
                    .absolute()
                    .left(bounds.origin.x)
                    .top(bounds.origin.y)
                    .opacity(opacity)
                    .child(panel),
            )
            .with_priority(1)
            .into_any_element();
        }
        let panel = panel.opacity(opacity);

        // The full-window occluder used for click-outside dismissal also
        // blocks moves to the surface beneath — incompatible with
        // hover-driven previews, which need the source to keep firing.
        let dismiss_on_outside = self.dismiss_on_outside_click;

        match self.mode {
            InspectorMode::Anchored(position) => {
                let element = if dismiss_on_outside {
                    let panel = panel.on_mouse_down_out(cx.listener(
                        |this, _: &gpui::MouseDownEvent, window, _cx| {
                            if !this.accessory.as_ref().is_some_and(|a| (a.dragging)(_cx)) {
                                this.dismiss(window, _cx);
                            }
                        },
                    ));
                    let anchored_panel = anchored()
                        .position(position)
                        .anchor(Corner::TopLeft)
                        .snap_to_window_with_margin(px(8.0))
                        .child(panel);
                    div()
                        .id("inspector-overlay")
                        .occlude()
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                        .child(anchored_panel)
                        .into_any_element()
                } else {
                    anchored()
                        .position(position)
                        .anchor(Corner::TopLeft)
                        .snap_to_window_with_margin(px(8.0))
                        .child(panel)
                        .into_any_element()
                };
                deferred(element).with_priority(1).into_any_element()
            }
            InspectorMode::Centered => {
                let panel = panel.on_mouse_down_out(cx.listener(
                    |this, _: &gpui::MouseDownEvent, window, _cx| {
                        if !this.accessory.as_ref().is_some_and(|a| (a.dragging)(_cx)) {
                            this.dismiss(window, _cx);
                        }
                    },
                ));
                let centered = div()
                    .id("inspector-centered")
                    .occlude()
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .flex()
                    .flex_col()
                    .items_center()
                    .pt(px(80.0))
                    .child(panel);

                deferred(centered).with_priority(1).into_any_element()
            }
            // No overlay at all: the host places the panel in its own tree.
            InspectorMode::Inline => panel.into_any_element(),
        }
    }
}

#[cfg(test)]
mod temporal_tests {
    use super::*;
    use crate::temporal::{self, picker::Target};

    #[gpui::test]
    fn timeline_accessory_draws_below_anchored_menu_and_applies_time_edits(
        cx: &mut gpui::TestAppContext,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let db = Arc::new(metor_db::DB::create(temp.path().join("db")).unwrap());
        cx.update(|cx| {
            crate::theme::set_theme(cx, Arc::new(crate::theme::DARK.clone()));
            temporal::TemporalController::init(db, cx);
            temporal::dispatch(
                temporal::TimeAction::Seek(temporal::TimeExpr::fixed(
                    metor_proto::types::Timestamp(50_000_000),
                )),
                cx,
            )
            .unwrap();
            temporal::dispatch(
                temporal::TimeAction::Range(temporal::TimeRangeSpec::fixed(
                    metor_proto::types::Timestamp(0)..metor_proto::types::Timestamp(100_000_000),
                )),
                cx,
            )
            .unwrap();
        });
        let (inspector, cx) = cx.add_window_view(|_, cx| {
            let rows = temporal::picker::editor(Target::Range, cx);
            Inspector::new(
                rows,
                InspectorMode::Anchored(gpui::point(px(40.0), px(40.0))),
                cx,
            )
        });
        cx.refresh().unwrap();
        cx.run_until_parked();
        let (area, before) = cx.update(|_, cx| {
            let i = inspector.read(cx);
            let accessory = i.accessory.as_ref().expect("time page has a companion");
            let timeline = accessory
                .view
                .clone()
                .downcast::<crate::views::Timeline>()
                .ok()
                .unwrap();
            let area = timeline
                .read(cx)
                .content_bounds()
                .expect("timeline painted");
            assert!(area.origin.y >= i.panel_bounds.unwrap().bottom());
            assert!(f32::from(area.size.height) <= 31.0);
            (area, temporal::config(cx))
        });
        // An immediate readout must also draw inside the inspector's deferred
        // layer, before a range drag takes ownership of the pointer.
        cx.simulate_mouse_move(
            area.origin + gpui::point(px(10.0), px(20.0)),
            None,
            gpui::Modifiers::default(),
        );
        cx.refresh().unwrap();
        cx.simulate_event(gpui::MouseDownEvent {
            button: gpui::MouseButton::Left,
            position: area.origin + gpui::point(px(2.0), px(4.0)),
            click_count: 1,
            ..Default::default()
        });
        cx.simulate_event(gpui::MouseMoveEvent {
            position: area.origin + gpui::point(px(80.0), px(4.0)),
            pressed_button: Some(gpui::MouseButton::Left),
            ..Default::default()
        });
        cx.simulate_event(gpui::MouseUpEvent {
            button: gpui::MouseButton::Left,
            position: area.origin + gpui::point(px(80.0), px(4.0)),
            ..Default::default()
        });
        cx.update(|_, cx| {
            let i = inspector.read(cx);
            assert!(
                !i.dismissed,
                "clicking the companion stays within the menu group"
            );
            assert_ne!(temporal::config(cx).range, before.range);
            assert_eq!(temporal::config(cx).view, before.view);
            assert!(!i.search.text.contains("+00:00"));
            assert_eq!(
                temporal::model::parse_range(&i.search.text, &temporal::model::ParseContext::utc())
                    .unwrap(),
                temporal::config(cx).range,
            );
        });
    }

    #[gpui::test]
    fn time_editor_keyboard_contract_is_identical_in_both_hosts(cx: &mut gpui::TestAppContext) {
        let temp = tempfile::tempdir().unwrap();
        let db = Arc::new(metor_db::DB::create(temp.path().join("db")).unwrap());
        let cx = cx.add_empty_window();
        cx.update(|window, cx| {
            crate::theme::set_theme(cx, Arc::new(crate::theme::DARK.clone()));
            temporal::TemporalController::init(db, cx);
            for mode in [
                InspectorMode::Centered,
                InspectorMode::Anchored(gpui::point(px(20.0), px(20.0))),
            ] {
                temporal::dispatch(
                    temporal::TimeAction::Range(temporal::TimeRangeSpec::fixed(
                        metor_proto::types::Timestamp(0)
                            ..metor_proto::types::Timestamp(100_000_000),
                    )),
                    cx,
                )
                .unwrap();
                let rows = temporal::picker::editor(Target::Range, cx);
                let inspector = cx.new(|cx| Inspector::new(rows, mode, cx));
                inspector.update(cx, |i, cx| {
                    assert_eq!(i.search.cursor, i.search.text.len());
                    let key = |key: &str| {
                        let mut keystroke = gpui::Keystroke::parse(key).unwrap();
                        if key.chars().count() == 1 {
                            keystroke.key_char = Some(key.to_string());
                        }
                        KeyDownEvent {
                            keystroke,
                            is_held: false,
                        }
                    };
                    i.search.set_text("last 2.5");
                    i.search.cursor = i.search.text.len();
                    i.search.mark = i.search.cursor;
                    i.handle_key_down(&key("m"), window, cx);
                    assert_eq!(temporal::config(cx).range.start.offset, -150_000_000);
                    let valid = temporal::config(cx);
                    i.handle_key_down(&key("backspace"), window, cx);
                    assert_eq!(
                        temporal::config(cx),
                        valid,
                        "incomplete duration keeps the last valid range"
                    );
                    i.handle_key_down(&key("tab"), window, cx);
                    assert_eq!(temporal::config(cx), valid);
                    let revision = cx.global::<temporal::TemporalRevision>().0;
                    i.handle_key_down(&key("enter"), window, cx);
                    assert_eq!(
                        cx.global::<temporal::TemporalRevision>().0,
                        revision,
                        "Enter must not reapply the live edit"
                    );
                    assert!(i.dismissed);
                    assert_eq!(temporal::config(cx).range.start.offset, -150_000_000);
                });
                temporal::dispatch(temporal::TimeAction::Live, cx).unwrap();
                let rows = temporal::picker::editor(Target::View, cx);
                let inspector = cx.new(|cx| Inspector::new(rows, mode, cx));
                inspector.update(cx, |i, cx| {
                    let before = temporal::config(cx);
                    i.search.set_text("2026-09-05 14:00:00 UTC");
                    i.query_edited(cx);
                    let edited = temporal::config(cx);
                    assert_ne!(edited.view, before.view);
                    i.handle_key_down(
                        &KeyDownEvent {
                            keystroke: gpui::Keystroke::parse("escape").unwrap(),
                            is_held: false,
                        },
                        window,
                        cx,
                    );
                    assert!(i.dismissed);
                    assert_eq!(
                        temporal::config(cx),
                        edited,
                        "Escape closes with the last applied value"
                    );
                });
            }
        });
    }
}
