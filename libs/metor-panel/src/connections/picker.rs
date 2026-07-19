//! The connection picker: a centered modal listing every connectable
//! target with live status, plus a manual connect-by-address escape hatch.
//!
//! Deliberately not a command-palette page — connecting is a workspace-level
//! act with its own chrome (status dots, connect/disconnect affordances) —
//! but it borrows the palette's visual language: same overlay placement,
//! search field, row metrics, and fuzzy filter, so it reads as native.
//! Opened from the palette's "Connect…" command, the titlebar chip, or
//! automatically on first open when nothing is connected.

use gpui::{
    App, Context, Entity, FocusHandle, Focusable, IntoElement, KeyDownEvent, Render,
    ScrollStrategy, SharedString, UniformListScrollHandle, Window, deferred, div, prelude::*, px,
    uniform_list,
};

use super::{ConnectionStatus, ConnectionTarget, ConnectionsStore};
use crate::inspector::rows::text_field::TextField;
use crate::theme::{Theme, theme};

const ROW_HEIGHT: f32 = 28.0;
const MAX_LIST_HEIGHT: f32 = 360.0;

/// What the list is currently showing: the target list, or the inline
/// address form the footer row switches to.
enum Phase {
    Browse,
    ManualAddress { error: Option<SharedString> },
}

/// One visible row, rebuilt from the store on every render so discovery
/// upserts and status changes appear without bookkeeping.
enum PickerRow {
    Header(SharedString),
    Target(ConnectionTarget),
    ManualAddress,
}

impl PickerRow {
    fn selectable(&self) -> bool {
        !matches!(self, PickerRow::Header(_))
    }
}

pub struct ConnectionPicker {
    store: Entity<ConnectionsStore>,
    search: TextField,
    phase: Phase,
    selected: usize,
    /// Auto-shown at first open: dismissal is suppressed until something
    /// connects, so the picker can't be escaped into an empty, dead app.
    require_connection: bool,
    scroll_handle: UniformListScrollHandle,
    focus_handle: FocusHandle,
    parent_focus: Option<FocusHandle>,
    pub dismissed: bool,
}

impl ConnectionPicker {
    pub fn new(
        store: Entity<ConnectionsStore>,
        require_connection: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.observe(&store, |_, _, cx| cx.notify()).detach();
        Self {
            store,
            search: TextField::new("Search or filter...", cx),
            phase: Phase::Browse,
            selected: 0,
            require_connection,
            scroll_handle: UniformListScrollHandle::new(),
            focus_handle: cx.focus_handle(),
            parent_focus: None,
            dismissed: false,
        }
    }

    pub fn set_parent_focus(&mut self, handle: FocusHandle) {
        self.parent_focus = Some(handle);
    }

    fn dismissable(&self, cx: &App) -> bool {
        !self.require_connection || !self.store.read(cx).active().is_empty()
    }

    fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.dismissable(cx) {
            return;
        }
        self.dismissed = true;
        if let Some(parent) = &self.parent_focus {
            window.focus(parent);
        }
    }

    /// The visible rows for the current filter. Targets keep discovery
    /// order; the manual-address entry is always last so it never shuffles.
    fn rows(&self, cx: &App) -> Vec<PickerRow> {
        let store = self.store.read(cx);
        let query = self.search.text.trim().to_string();
        let mut targets: Vec<ConnectionTarget> = store.state().targets().to_vec();
        if !query.is_empty() {
            let scored = fuzzy_scores(
                &query,
                targets
                    .iter()
                    .map(|t| format!("{} {}", t.name, t.detail))
                    .collect(),
            );
            targets = targets
                .into_iter()
                .zip(scored)
                .filter_map(|(t, score)| score.map(|_| t))
                .collect();
        }
        let mut rows = Vec::with_capacity(targets.len() + 2);
        if !targets.is_empty() {
            rows.push(PickerRow::Header(SharedString::new_static("Systems")));
        }
        rows.extend(targets.into_iter().map(PickerRow::Target));
        rows.push(PickerRow::ManualAddress);
        rows
    }

    fn clamp_selection(&mut self, rows: &[PickerRow]) {
        if rows.is_empty() {
            self.selected = 0;
            return;
        }
        if self.selected >= rows.len() || !rows[self.selected].selectable() {
            self.selected = rows
                .iter()
                .position(PickerRow::selectable)
                .unwrap_or_default();
        }
    }

    fn move_selection(&mut self, delta: isize, cx: &App) {
        let rows = self.rows(cx);
        let mut i = self.selected as isize;
        loop {
            i += delta;
            if i < 0 || i as usize >= rows.len() {
                return;
            }
            if rows[i as usize].selectable() {
                self.selected = i as usize;
                self.scroll_handle.scroll_to_item(
                    self.selected,
                    if delta < 0 {
                        ScrollStrategy::Top
                    } else {
                        ScrollStrategy::Bottom
                    },
                );
                return;
            }
        }
    }

    fn activate_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rows = self.rows(cx);
        match rows.get(self.selected) {
            Some(PickerRow::Target(target)) => self.toggle_connection(target.clone(), window, cx),
            Some(PickerRow::ManualAddress) => {
                self.phase = Phase::ManualAddress { error: None };
                self.search.clear();
                self.search.set_placeholder("host:port");
            }
            _ => {}
        }
    }

    fn toggle_connection(
        &mut self,
        target: ConnectionTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let connected = self.store.read(cx).is_connected(&target.id);
        self.store.update(cx, |store, cx| {
            if connected {
                store.disconnect(&target.id.clone(), cx);
            } else {
                store.connect(target, cx);
            }
        });
        if !connected {
            self.require_connection = false;
            self.dismiss(window, cx);
        }
    }

    fn submit_manual_address(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.search.text.trim().to_string();
        match text.parse::<std::net::SocketAddr>() {
            Ok(addr) => {
                let target = ConnectionTarget::tcp(text, addr);
                self.store
                    .update(cx, |store, cx| store.upsert_target(target.clone(), cx));
                self.toggle_connection(target, window, cx);
            }
            Err(_) => {
                self.phase = Phase::ManualAddress {
                    error: Some(SharedString::new_static(
                        "expected host:port, e.g. 127.0.0.1:2240",
                    )),
                };
            }
        }
    }

    fn leave_manual_address(&mut self) {
        self.phase = Phase::Browse;
        self.search.clear();
        self.search.set_placeholder("Search or filter...");
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = event.keystroke.key.as_str();
        if let Phase::ManualAddress { .. } = &self.phase {
            match key {
                "escape" => self.leave_manual_address(),
                "enter" | "return" => self.submit_manual_address(window, cx),
                _ => {
                    self.search.handle_key_down(event, cx);
                }
            }
            cx.notify();
            return;
        }

        match key {
            "escape" => self.dismiss(window, cx),
            "up" => self.move_selection(-1, cx),
            "down" => self.move_selection(1, cx),
            "enter" | "return" => self.activate_selected(window, cx),
            _ => {
                if self.search.handle_key_down(event, cx) {
                    self.selected = 0;
                    let rows = self.rows(cx);
                    self.clamp_selection(&rows);
                }
            }
        }
        cx.notify();
    }

    fn status_dot(status: Option<ConnectionStatus>, theme: &Theme) -> gpui::Div {
        let color = match status {
            Some(ConnectionStatus::Connected) => theme.control_active,
            Some(ConnectionStatus::Connecting) | Some(ConnectionStatus::Reconnecting) => {
                theme.text_secondary
            }
            Some(ConnectionStatus::Failed(_)) => theme.error_accent,
            Some(ConnectionStatus::Disconnected) | None => theme.text_tertiary,
        };
        div()
            .w(px(14.0))
            .flex_shrink_0()
            .text_color(color)
            .child("\u{25cf}")
    }

    fn render_target_row(
        &self,
        target: &ConnectionTarget,
        row_ix: usize,
        selected: bool,
        cx: &App,
    ) -> gpui::AnyElement {
        let theme = theme(cx);
        let store = self.store.read(cx);
        let status = store
            .active()
            .iter()
            .find(|c| c.target.id == target.id)
            .map(|c| c.status());
        let connected = status.is_some();

        let mut row = crate::inspector::rows::row_base(row_ix, selected, cx)
            .child(Self::status_dot(status.clone(), &theme))
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme.text_primary)
                    .child(target.name.clone()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_size(px(11.0))
                    .text_color(theme.text_secondary)
                    .child(target.detail.clone()),
            );

        if let Some(ConnectionStatus::Failed(reason)) = &status {
            row = row.child(
                div()
                    .text_size(px(11.0))
                    .text_color(theme.error_accent)
                    .child(reason.clone()),
            );
        }
        if selected || connected {
            let label = if connected { "disconnect" } else { "connect" };
            row = row.child(
                div()
                    .text_size(px(11.0))
                    .text_color(if connected {
                        theme.text_secondary
                    } else {
                        theme.control_active
                    })
                    .child(SharedString::new_static(label)),
            );
        }
        row.into_any_element()
    }

    fn render_rows(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = theme(cx);
        let rows = self.rows(cx);
        self.clamp_selection(&rows);
        if rows.is_empty() {
            return div()
                .px(px(12.0))
                .py(px(6.0))
                .text_size(px(12.0))
                .text_color(theme.text_tertiary)
                .child(SharedString::new_static("No systems"))
                .into_any_element();
        }

        let count = rows.len();
        let items_h = (count as f32 * ROW_HEIGHT).min(MAX_LIST_HEIGHT);
        uniform_list(
            "connection-picker-items",
            count,
            cx.processor(
                move |this: &mut Self, range: std::ops::Range<usize>, _window, cx| {
                    let theme = crate::theme::theme(cx);
                    let rows = this.rows(cx);
                    let mut out = Vec::with_capacity(range.len());
                    for ix in range {
                        let selected = ix == this.selected;
                        let element: gpui::AnyElement = match &rows[ix] {
                            PickerRow::Header(label) => div()
                                .px(px(12.0))
                                .h(px(ROW_HEIGHT))
                                .flex()
                                .items_center()
                                .text_size(px(10.0))
                                .text_color(theme.text_tertiary)
                                .child(label.clone())
                                .into_any_element(),
                            PickerRow::Target(target) => {
                                let target_click = target.clone();
                                div()
                                    .on_mouse_down(
                                        gpui::MouseButton::Left,
                                        cx.listener(move |this, _, window, cx| {
                                            this.selected = ix;
                                            this.toggle_connection(
                                                target_click.clone(),
                                                window,
                                                cx,
                                            );
                                            cx.notify();
                                        }),
                                    )
                                    .child(this.render_target_row(target, ix, selected, cx))
                                    .into_any_element()
                            }
                            PickerRow::ManualAddress => div()
                                .on_mouse_down(
                                    gpui::MouseButton::Left,
                                    cx.listener(move |this, _, _window, cx| {
                                        this.selected = ix;
                                        this.phase = Phase::ManualAddress { error: None };
                                        this.search.clear();
                                        this.search.set_placeholder("host:port");
                                        cx.notify();
                                    }),
                                )
                                .child(
                                    crate::inspector::rows::row_base(ix, selected, cx)
                                        .child(
                                            div()
                                                .text_size(px(12.0))
                                                .text_color(theme.text_secondary)
                                                .child(SharedString::new_static(
                                                    "Connect to address\u{2026}",
                                                )),
                                        ),
                                )
                                .into_any_element(),
                        };
                        out.push(element);
                    }
                    out
                },
            ),
        )
        .track_scroll(self.scroll_handle.clone())
        .h(px(items_h))
        .into_any_element()
    }

    fn render_input_bar(&self, cx: &App) -> impl IntoElement {
        let theme = theme(cx);
        let mut bar = div()
            .flex()
            .flex_row()
            .items_center()
            .px(px(8.0))
            .py(px(4.0))
            .border_b_1()
            .border_color(theme.border_primary)
            .text_size(px(12.0));
        if matches!(self.phase, Phase::ManualAddress { .. }) {
            bar = bar.child(div().mr(px(4.0)).child(crate::inspector::rows::tag_pill(
                SharedString::new_static("address"),
                cx,
            )));
        }
        bar.child(div().flex_1().min_w(px(60.0)).child(self.search.element()))
    }

    fn render_header(&self, cx: &App) -> impl IntoElement {
        let theme = theme(cx);
        let active = self.store.read(cx).active().len();
        let mut header = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .px(px(12.0))
            .py(px(8.0))
            .border_b_1()
            .border_color(theme.border_primary)
            .child(
                div()
                    .text_size(px(13.0))
                    .text_color(theme.text_primary)
                    .child(SharedString::new_static("Connections")),
            );
        if active > 0 {
            header = header.child(crate::inspector::rows::tag_pill(
                SharedString::from(format!(
                    "{active} connected"
                )),
                cx,
            ));
        }
        header
    }
}

impl Focusable for ConnectionPicker {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ConnectionPicker {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.dismissed {
            return div().into_any_element();
        }
        let theme = theme(cx);

        let body: gpui::AnyElement = match &self.phase {
            Phase::Browse => self.render_rows(cx),
            Phase::ManualAddress { error } => {
                let mut body = div().py(px(6.0)).px(px(12.0)).flex().flex_col();
                body = body.child(
                    div()
                        .text_size(px(11.0))
                        .text_color(theme.text_tertiary)
                        .child(SharedString::new_static(
                            "Enter a metor-db address and press enter",
                        )),
                );
                if let Some(error) = error {
                    body = body.child(
                        div()
                            .text_size(px(11.0))
                            .text_color(theme.error_accent)
                            .child(error.clone()),
                    );
                }
                body.into_any_element()
            }
        };

        let mut panel = div()
            .id("connection-picker-panel")
            .key_context("ConnectionPicker")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.handle_key_down(event, window, cx);
            }))
            .flex()
            .flex_col()
            .w(px(560.0))
            .max_h(px(440.0))
            .bg(theme.bg_elevated)
            .border_1()
            .border_color(theme.border_primary)
            .rounded(px(6.0))
            .child(self.render_header(cx))
            .child(self.render_input_bar(cx))
            .child(div().py(px(2.0)).child(body));

        if self.dismissable(cx) {
            panel = panel.on_mouse_down_out(cx.listener(|this, _: &gpui::MouseDownEvent, window, cx| {
                this.dismiss(window, cx);
                cx.notify();
            }));
        }

        let centered = div()
            .id("connection-picker-overlay")
            .occlude()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .pt(px(80.0))
            .child(panel)
            .shadow_sm();

        deferred(centered).with_priority(1).into_any_element()
    }
}

/// Nucleo fuzzy scores for `query` against each haystack; `None` filters
/// the row out. Mirrors the inspector's matcher setup so both lists feel
/// identical to type into.
fn fuzzy_scores(query: &str, haystacks: Vec<String>) -> Vec<Option<u32>> {
    use nucleo_matcher::{
        Matcher,
        pattern::{CaseMatching, Normalization, Pattern},
    };
    let mut matcher = Matcher::new(nucleo_matcher::Config::DEFAULT);
    let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
    let mut buf = Vec::new();
    haystacks
        .iter()
        .map(|haystack| {
            let haystack = nucleo_matcher::Utf32Str::new(haystack, &mut buf);
            pattern.score(haystack, &mut matcher)
        })
        .collect()
}
