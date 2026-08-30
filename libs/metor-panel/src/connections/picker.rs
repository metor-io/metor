//! The connection dialog: a spacious centered surface that explains what
//! connecting does, pins favorites front and center, and lists everything
//! else in a searchable table.
//!
//! Deliberately unlike the command palette: no selection pills, no
//! filter-first chrome. The shape follows hardware-discovery dialogs — a
//! compact caption bar that says what's streaming, favorite cards up
//! top, then a full-width table with hairline dividers where rows select
//! and explicit buttons act, and a connect-by-address footer. Opened from
//! the palette's "Connect…" command, the titlebar identity segment, or
//! automatically on first open when nothing is connected.

use gpui::{
    App, Context, Entity, FocusHandle, Focusable, IntoElement, KeyDownEvent, Render, SharedString,
    Window, deferred, div, prelude::*, px,
};

use super::options::option_rows;
use super::{ConnectionStatus, ConnectionTarget, ConnectionsStore, PickerEntry, TargetId, persist};
use crate::icons::Icon;
use crate::inspector::rows::text_field::TextField;
use crate::inspector::{Inspector, InspectorMode};
use crate::query::fuzzy_scores;
use crate::theme::{Theme, theme};

const DIALOG_WIDTH: f32 = 640.0;
const ROW_HEIGHT: f32 = 36.0;

/// What the dialog is currently doing: browsing, configuring one target,
/// typing an ad-hoc address, or answering the load-saved-layout question
/// shown right after connecting.
enum Phase {
    Browse,
    /// One target's knobs, rendered by an inline [`Inspector`] so the page
    /// inherits the palette's whole keyboard model instead of growing its
    /// own. Escape at the inspector's root dismisses it, which
    /// [`Render`] reads as "back to browsing".
    Options {
        target: ConnectionTarget,
        inspector: Entity<Inspector>,
    },
    ManualAddress {
        error: Option<SharedString>,
    },
    LayoutChoice {
        layout: String,
        choice: usize,
    },
}

/// Everything the keyboard can land on, in visual order: favorite cards,
/// then table rows, then the address footer.
enum Slot {
    Favorite(PickerEntry),
    Row(PickerEntry),
    ManualAddress,
}

pub struct ConnectionPicker {
    store: Entity<ConnectionsStore>,
    search: TextField,
    phase: Phase,
    selected: usize,
    /// Auto-shown at first open: dismissal is suppressed until something
    /// connects, so the picker can't be escaped into an empty, dead app.
    require_connection: bool,
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
            search: TextField::new("Search systems...", cx),
            phase: Phase::Browse,
            selected: 0,
            require_connection,
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

    /// Favorites (always shown, unfiltered) and the searchable remainder:
    /// recents first, then discovery order. Unavailable custom-backend
    /// recents ride along for continuity but aren't selectable.
    fn favorites(&self, cx: &App) -> Vec<PickerEntry> {
        self.store.read(cx).state().sections().favorites
    }

    fn table_entries(&self, cx: &App) -> Vec<PickerEntry> {
        let sections = self.store.read(cx).state().sections();
        let mut entries: Vec<PickerEntry> = sections
            .recents
            .into_iter()
            .chain(sections.discovered)
            .collect();
        let query = self.search.text.trim().to_string();
        if !query.is_empty() {
            let scored = fuzzy_scores(
                &query,
                entries.iter().map(|e| format!("{} {}", e.name, e.detail)),
            );
            entries = entries
                .into_iter()
                .zip(scored)
                .filter_map(|(e, score)| score.map(|_| e))
                .collect();
        }
        entries
    }

    /// The keyboard order over the whole dialog.
    fn slots(&self, cx: &App) -> Vec<Slot> {
        let mut slots: Vec<Slot> = self
            .favorites(cx)
            .into_iter()
            .filter(|e| e.target.is_some())
            .map(Slot::Favorite)
            .collect();
        slots.extend(
            self.table_entries(cx)
                .into_iter()
                .filter(|e| e.target.is_some())
                .map(Slot::Row),
        );
        slots.push(Slot::ManualAddress);
        slots
    }

    fn clamp_selection(&mut self, len: usize) {
        if len == 0 {
            self.selected = 0;
        } else if self.selected >= len {
            self.selected = len - 1;
        }
    }

    fn move_selection(&mut self, delta: isize, cx: &App) {
        let len = self.slots(cx).len();
        if len == 0 {
            return;
        }
        let next = self.selected as isize + delta;
        if next >= 0 && (next as usize) < len {
            self.selected = next as usize;
        }
    }

    fn activate_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let slots = self.slots(cx);
        match slots.get(self.selected) {
            Some(Slot::Favorite(entry)) | Some(Slot::Row(entry)) => {
                if let Some(target) = entry.target.clone() {
                    self.toggle_connection(target, window, cx);
                }
            }
            Some(Slot::ManualAddress) => self.enter_manual_address(cx),
            None => {}
        }
    }

    /// Whether `target` has anything to configure — its own declaration or
    /// the panel-wide fallback. Drives the gear affordance and the palette
    /// entry alike, so an unconfigurable target never advertises a page
    /// that would render empty.
    fn configurable(&self, target: &ConnectionTarget, cx: &App) -> bool {
        !self.store.read(cx).state().spec_for(target).is_empty()
    }

    fn enter_options(
        &mut self,
        target: ConnectionTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let rows = option_rows(self.store.clone(), target.clone(), cx);
        let parent_focus = self.focus_handle.clone();
        let inspector = cx.new(|cx| {
            let mut insp = Inspector::new(rows, InspectorMode::Inline, cx);
            insp.set_parent_focus(parent_focus);
            insp
        });
        inspector.focus_handle(cx).focus(window);
        self.phase = Phase::Options { target, inspector };
    }

    fn enter_manual_address(&mut self, cx: &App) {
        self.phase = Phase::ManualAddress { error: None };
        self.search.clear();
        let placeholder = self.store.read(cx).state().resolver().placeholder.clone();
        self.search.set_placeholder(placeholder.to_string());
    }

    fn toggle_connection(
        &mut self,
        target: ConnectionTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let connected = self.store.read(cx).is_connected(&target.id);
        let id = target.id.clone();
        self.store.update(cx, |store, cx| {
            if connected {
                store.disconnect(&id, cx);
            } else {
                store.connect(target, cx);
            }
        });
        if !connected {
            self.require_connection = false;
            self.offer_saved_layout(&id, window, cx);
        }
    }

    /// Right after connecting: restore the target's saved layout — silently
    /// into an untouched tile tree, or with consent when the user has
    /// something on screen the load would replace.
    fn offer_saved_layout(&mut self, id: &TargetId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(layout) = persist::load_layout(id) else {
            self.dismiss(window, cx);
            return;
        };
        if crate::workspace::any_window_has_items(cx) {
            self.phase = Phase::LayoutChoice { layout, choice: 0 };
        } else {
            self.load_layout(layout, window, cx);
        }
    }

    fn load_layout(&mut self, layout: String, window: &mut Window, cx: &mut Context<Self>) {
        let db = self.store.read(cx).db().clone();
        if crate::workspace::restore_workspace(&layout, window, cx, db) {
            self.store
                .update(cx, |store, _| store.note_loaded_layout(layout));
        }
        self.dismiss(window, cx);
    }

    fn submit_manual_address(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.search.text.trim().to_string();
        let resolved = (self.store.read(cx).state().resolver().resolve)(&text);
        match resolved {
            Ok(target) => {
                self.store
                    .update(cx, |store, cx| store.upsert_target(target.clone(), cx));
                self.leave_manual_address();
                self.toggle_connection(target, window, cx);
            }
            Err(error) => {
                self.phase = Phase::ManualAddress { error: Some(error) };
            }
        }
    }

    fn leave_manual_address(&mut self) {
        self.phase = Phase::Browse;
        self.search.clear();
        self.search.set_placeholder("Search systems...");
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = event.keystroke.key.as_str();
        match &self.phase {
            // The embedded inspector holds focus and handles its own keys;
            // events bubble up here afterwards, so the dialog stays out of
            // the way entirely rather than double-handling them.
            Phase::Options { .. } => return,
            Phase::ManualAddress { .. } => {
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
            Phase::LayoutChoice { layout, choice } => {
                let layout = layout.clone();
                let choice = *choice;
                match key {
                    "up" | "down" | "left" | "right" | "tab" => {
                        self.phase = Phase::LayoutChoice {
                            layout,
                            choice: 1 - choice,
                        };
                    }
                    "enter" | "return" => {
                        if choice == 0 {
                            self.load_layout(layout, window, cx);
                        } else {
                            self.dismiss(window, cx);
                        }
                    }
                    "escape" => self.dismiss(window, cx),
                    _ => {}
                }
                cx.notify();
                return;
            }
            Phase::Browse => {}
        }

        match key {
            "escape" => self.dismiss(window, cx),
            "up" | "left" => self.move_selection(-1, cx),
            "down" | "right" => self.move_selection(1, cx),
            "enter" | "return" => self.activate_selected(window, cx),
            _ => {
                if self.search.handle_key_down(event, cx) {
                    self.selected = 0;
                    let len = self.slots(cx).len();
                    self.clamp_selection(len);
                }
            }
        }
        cx.notify();
    }

    fn status_of(&self, id: &TargetId, cx: &App) -> Option<ConnectionStatus> {
        self.store
            .read(cx)
            .active()
            .iter()
            .find(|c| &c.target.id == id)
            .map(|c| c.status())
    }

    fn status_color(status: &Option<ConnectionStatus>, theme: &Theme) -> gpui::Hsla {
        match status {
            Some(ConnectionStatus::Connected) => theme.control_active,
            Some(ConnectionStatus::Connecting) | Some(ConnectionStatus::Reconnecting) => {
                theme.text_secondary
            }
            Some(ConnectionStatus::Failed(_)) => theme.error_accent,
            Some(ConnectionStatus::Disconnected) | None => theme.text_tertiary,
        }
    }

    /// The star that pins a target: a real toggle with its own hit area,
    /// visibly separate from the row so it never reads as part of a
    /// row-wide click. Clicking never connects.
    fn star(
        &self,
        id: &TargetId,
        favorite: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let id = id.clone();
        div()
            .id(SharedString::from(format!("star-{id}")))
            .flex_shrink_0()
            .w(px(26.0))
            .h(px(24.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(4.0))
            .text_color(if favorite {
                theme.alarm_color(1)
            } else {
                theme.text_tertiary
            })
            .cursor_pointer()
            .hover(|s| s.bg(theme.bg_primary).text_color(theme.alarm_color(1)))
            .child(if favorite {
                Icon::Favorite.svg_color(8.0, theme.alarm_color(1))
            } else {
                Icon::Favorite.svg(8.0)
            })
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _, _window, cx| {
                    cx.stop_propagation();
                    this.store
                        .update(cx, |store, cx| store.toggle_favorite(&id, cx));
                    cx.notify();
                }),
            )
    }

    /// The gear that opens a target's knobs: its own hit area beside the
    /// star, so clicking it configures rather than connects. Absent when
    /// the target has nothing to configure.
    fn gear(
        &self,
        target: &ConnectionTarget,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let target = target.clone();
        div()
            .id(SharedString::from(format!("gear-{}", target.id)))
            .flex_shrink_0()
            .w(px(26.0))
            .h(px(24.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(4.0))
            .cursor_pointer()
            .hover(|s| s.bg(theme.bg_primary))
            .child(Icon::Setting.svg_color(11.0, theme.text_tertiary))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.enter_options(target.clone(), window, cx);
                    cx.notify();
                }),
            )
    }

    /// The connect/disconnect action: a full-height segment at the row's
    /// right edge — `| info | button |` — split off by a dimmed green/red
    /// left border, with a faint matching tint and accent text. Width
    /// follows the label. The row itself only selects.
    fn action_button(
        &self,
        target: ConnectionTarget,
        connected: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let (label, accent) = if connected {
            (SharedString::new_static("Disconnect"), theme.error_accent)
        } else {
            (SharedString::new_static("Connect"), theme.control_active)
        };
        div()
            .id(SharedString::from(format!("action-{}", target.id)))
            .flex_shrink_0()
            .h_full()
            .px(px(8.0))
            .flex()
            .min_w(px(75.))
            .items_center()
            .justify_center()
            .border_l_2()
            .bg(Theme::dim(accent, 0.11))
            .text_size(px(12.0))
            .text_color(accent)
            .cursor_pointer()
            .hover(|s| s.bg(Theme::dim(accent, 0.18)))
            .child(label)
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.toggle_connection(target.clone(), window, cx);
                    cx.notify();
                }),
            )
    }

    /// One favorite card: a bordered tile with the status dot, name, and
    /// detail stacked. The keyboard highlight is an accent border.
    fn render_favorite_card(
        &self,
        entry: &PickerEntry,
        selected: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let status = self.status_of(&entry.id, cx);
        let connected = status.is_some();
        let target = entry.target.clone();
        let border = if selected {
            theme.control_active
        } else {
            theme.border_primary
        };
        div()
            .id(SharedString::from(format!("fav-{}", entry.id)))
            .w(px(200.0))
            .p(px(8.0))
            .flex()
            .flex_col()
            .gap(px(4.0))
            .bg(theme.bg_secondary)
            .border_1()
            .border_color(border)
            .rounded(px(6.0))
            .cursor_pointer()
            .hover(|s| s.bg(theme.bg_primary))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    if let Some(target) = target.clone() {
                        this.toggle_connection(target, window, cx);
                    }
                    cx.notify();
                }),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        div()
                            .text_size(px(10.0))
                            .text_color(Self::status_color(&status, theme))
                            .child("\u{25cf}"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_size(px(13.0))
                            .text_color(theme.text_primary)
                            .child(entry.name.clone()),
                    )
                    .when_some(
                        entry
                            .target
                            .as_ref()
                            .filter(|t| self.configurable(t, cx))
                            .cloned(),
                        |row, target| row.child(self.gear(&target, theme, cx)),
                    )
                    .child(self.star(&entry.id, entry.favorite, theme, cx)),
            )
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(theme.text_secondary)
                    .overflow_hidden()
                    .child(if connected {
                        SharedString::new_static("connected")
                    } else {
                        entry.detail.clone()
                    }),
            )
            .into_any_element()
    }

    /// One table row: star toggle, status dot, then name with its detail
    /// right beside it, and an explicit action button on the right. The
    /// row itself only selects — the button and star each own their click.
    fn render_table_row(
        &self,
        entry: &PickerEntry,
        row_ix: usize,
        selected: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let available = entry.target.is_some();
        let status = self.status_of(&entry.id, cx);
        let connected = status.is_some();

        // Every cell is a full-height flex box centering its content, so
        // the star, dot, and text all sit on the same visual centerline
        // regardless of glyph metrics.
        let cell = || div().h_full().flex().flex_row().items_center();

        let mut row = div()
            .id(SharedString::from(format!("row-{}", entry.id)))
            .h(px(ROW_HEIGHT))
            .pl(px(10.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .border_b_1()
            .border_color(theme.border_primary)
            .when(selected, |r| r.bg(theme.bg_secondary));

        if available {
            row = row.child(cell().child(self.star(&entry.id, entry.favorite, theme, cx)));
        } else {
            row = row.child(div().w(px(26.0)).flex_shrink_0());
        }

        row = row
            .child(
                cell()
                    .w(px(10.0))
                    .flex_shrink_0()
                    .text_size(px(10.0))
                    .text_color(Self::status_color(&status, theme))
                    .child("\u{25cf}"),
            )
            .child(
                cell()
                    .flex_shrink_0()
                    .text_size(px(13.0))
                    .text_color(if available {
                        theme.text_primary
                    } else {
                        theme.text_tertiary
                    })
                    .child(entry.name.clone()),
            )
            .child(
                cell()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_size(px(12.0))
                    .text_color(theme.text_secondary)
                    .child(entry.detail.clone()),
            );

        if let Some(ConnectionStatus::Failed(reason)) = &status {
            row = row.child(
                cell()
                    .flex_shrink_0()
                    .items_center()
                    .text_size(px(11.0))
                    .text_color(theme.error_accent)
                    .child(reason.clone()),
            );
        }
        if let Some(target) = entry.target.clone() {
            if self.configurable(&target, cx) {
                row = row.child(cell().child(self.gear(&target, theme, cx)));
            }
            row = row.child(self.action_button(target, connected, theme, cx));
        } else {
            row = row.child(
                cell()
                    .flex_shrink_0()
                    .pr(px(10.0))
                    .text_size(px(11.0))
                    .text_color(theme.text_tertiary)
                    .child(SharedString::new_static("not available")),
            );
        }
        if available {
            row = row.on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _, _window, cx| {
                    this.selected = row_ix;
                    cx.notify();
                }),
            );
        }
        row.into_any_element()
    }

    fn render_browse(&mut self, theme: &Theme, cx: &mut Context<Self>) -> gpui::AnyElement {
        let favorites = self.favorites(cx);
        let table = self.table_entries(cx);
        let slots = self.slots(cx);
        self.clamp_selection(slots.len());
        let selectable_favorites = favorites.iter().filter(|e| e.target.is_some()).count();

        let mut body = div().flex().flex_col().min_h_0();

        // Pinned systems first — the fast path sits right under the caption.
        if !favorites.is_empty() {
            let mut card_ix = 0usize;
            let mut cards = div()
                .flex()
                .flex_row()
                .flex_wrap()
                .justify_center()
                .gap(px(10.0))
                .px(px(16.0))
                .pt(px(14.0))
                .pb(px(14.0));
            for entry in &favorites {
                if entry.target.is_some() {
                    let selected = card_ix == self.selected;
                    cards = cards.child(self.render_favorite_card(entry, selected, theme, cx));
                    card_ix += 1;
                }
            }
            body = body.child(cards);
        }

        // The searchable remainder, Revel-table style; the search field
        // lives up in the caption bar.
        let mut list = div()
            .id("connection-table")
            .flex()
            .flex_col()
            .min_h_0()
            .overflow_y_scroll()
            .max_h(px(288.0))
            // The caption bar already draws a bottom border; only a card
            // strip above needs its own separator.
            .when(!favorites.is_empty(), |l| {
                l.border_t_1().border_color(theme.border_primary)
            });
        if table.is_empty() {
            list = list.child(
                div()
                    .px(px(16.0))
                    .py(px(10.0))
                    .text_size(px(12.0))
                    .text_color(theme.text_tertiary)
                    .child(SharedString::new_static("No other systems")),
            );
        }
        let mut row_ix = selectable_favorites;
        for entry in &table {
            let selected = entry.target.is_some() && row_ix == self.selected;
            list = list.child(self.render_table_row(entry, row_ix, selected, theme, cx));
            if entry.target.is_some() {
                row_ix += 1;
            }
        }
        body = body.child(list);

        // Footer: the ad-hoc escape hatch.
        let footer_selected = self.selected == slots.len().saturating_sub(1);
        body = body.child(
            div()
                .id("manual-address")
                .h(px(ROW_HEIGHT))
                .px(px(16.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .when(footer_selected, |r| r.bg(theme.bg_secondary))
                .cursor_pointer()
                .hover(|s| s.bg(theme.bg_secondary))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|this, _, _window, cx| {
                        this.enter_manual_address(cx);
                        cx.notify();
                    }),
                )
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(theme.text_secondary)
                        .child(SharedString::new_static("+ Connect to address\u{2026}")),
                ),
        );

        body.into_any_element()
    }

    /// One target's knobs: a back affordance and title, the inline
    /// inspector holding the rows, and the same connect/disconnect button
    /// the browse rows use — so a knob can be set and the connection made
    /// without stepping back out.
    fn render_options(
        &self,
        target: &ConnectionTarget,
        inspector: &Entity<Inspector>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let connected = self.store.read(cx).is_connected(&target.id);
        let title = SharedString::from(format!("{} \u{00b7} options", target.name));

        div()
            .flex()
            .flex_col()
            .min_h_0()
            .child(
                div()
                    .h(px(ROW_HEIGHT))
                    .px(px(16.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.0))
                    .border_b_1()
                    .border_color(theme.border_primary)
                    .child(
                        div()
                            .id("options-back")
                            .text_size(px(11.0))
                            .text_color(theme.text_tertiary)
                            .cursor_pointer()
                            .hover(|s| s.text_color(theme.text_primary))
                            .child(SharedString::new_static("\u{2190} back to systems"))
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                cx.listener(|this, _, window, cx| {
                                    this.leave_options(window, cx);
                                    cx.notify();
                                }),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_size(px(12.0))
                            .text_color(theme.text_primary)
                            .child(title),
                    ),
            )
            .child(inspector.clone())
            .child(
                div()
                    .h(px(ROW_HEIGHT))
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_end()
                    .border_t_1()
                    .border_color(theme.border_primary)
                    .child(self.action_button(target.clone(), connected, theme, cx)),
            )
            .into_any_element()
    }

    fn leave_options(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.phase = Phase::Browse;
        window.focus(&self.focus_handle);
        cx.notify();
    }

    fn render_manual_address(
        &self,
        error: &Option<SharedString>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let mut body = div()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(10.0))
            .pt(px(24.0))
            .pb(px(28.0))
            .child(
                div()
                    .text_size(px(18.0))
                    .text_color(theme.text_primary)
                    .child(SharedString::new_static("Connect to an address")),
            )
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme.text_secondary)
                    .child(SharedString::new_static(
                        "Type an address and press enter to connect.",
                    )),
            )
            .child(
                div()
                    .w(px(300.0))
                    .px(px(10.0))
                    .py(px(5.0))
                    .bg(theme.bg_secondary)
                    .border_1()
                    .border_color(theme.border_primary)
                    .rounded(px(4.0))
                    .text_size(px(13.0))
                    .child(self.search.element()),
            );
        if let Some(error) = error {
            body = body.child(
                div()
                    .text_size(px(11.0))
                    .text_color(theme.error_accent)
                    .child(error.clone()),
            );
        }
        body = body.child(
            div()
                .id("manual-back")
                .text_size(px(11.0))
                .text_color(theme.text_tertiary)
                .cursor_pointer()
                .hover(|s| s.text_color(theme.text_primary))
                .child(SharedString::new_static("\u{2190} back to systems"))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|this, _, _window, cx| {
                        this.leave_manual_address();
                        cx.notify();
                    }),
                ),
        );
        body.into_any_element()
    }

    fn render_layout_choice(
        &self,
        layout: &str,
        choice: usize,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let layout = layout.to_string();
        let mut body = div()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(10.0))
            .pt(px(24.0))
            .pb(px(28.0))
            .child(
                div()
                    .text_size(px(18.0))
                    .text_color(theme.text_primary)
                    .child(SharedString::new_static("Restore saved layout?")),
            )
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme.text_secondary)
                    .child(SharedString::new_static(
                        "This system has a layout from your last session.",
                    )),
            );
        let mut buttons = div().flex().flex_row().gap(px(12.0)).pt(px(8.0));
        for (ix, label) in ["Load saved layout", "Keep current layout"]
            .into_iter()
            .enumerate()
        {
            let layout = layout.clone();
            let selected = ix == choice;
            buttons = buttons.child(
                div()
                    .id(SharedString::from(format!("layout-choice-{ix}")))
                    .px(px(16.0))
                    .py(px(6.0))
                    .bg(theme.bg_secondary)
                    .border_1()
                    .border_color(if selected {
                        theme.control_active
                    } else {
                        theme.border_primary
                    })
                    .rounded(px(5.0))
                    .text_size(px(12.0))
                    .text_color(theme.text_primary)
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.bg_primary))
                    .child(SharedString::new_static(label))
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            if ix == 0 {
                                this.load_layout(layout.clone(), window, cx);
                            } else {
                                this.dismiss(window, cx);
                            }
                            cx.notify();
                        }),
                    ),
            );
        }
        body = body.child(buttons);
        body.into_any_element()
    }
}

impl Focusable for ConnectionPicker {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ConnectionPicker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.dismissed {
            return div().into_any_element();
        }
        let theme = theme(cx);

        // Caption bar: sized like the app's other bars — title on the left,
        // search living in the bar itself, close affordance only when
        // leaving is allowed.
        let mut caption_right = div().flex().flex_row().items_center().gap(px(8.0));
        if matches!(self.phase, Phase::Browse) {
            caption_right = caption_right.child(
                div()
                    .w(px(220.0))
                    .px(px(8.0))
                    .py(px(2.0))
                    .bg(theme.bg_secondary)
                    .border_1()
                    .border_color(theme.border_primary)
                    .rounded(px(4.0))
                    .text_size(px(12.0))
                    .child(self.search.element()),
            );
        }
        if self.dismissable(cx) {
            caption_right = caption_right.child(
                div()
                    .id("picker-close")
                    .px(px(6.0))
                    .py(px(4.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.bg_secondary))
                    .child(crate::icons::Icon::Close.svg_color(12.0, theme.text_secondary))
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            this.dismiss(window, cx);
                            cx.notify();
                        }),
                    ),
            );
        }
        let caption = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .px(px(16.0))
            .py(px(5.0))
            .border_b_1()
            .border_color(theme.border_primary)
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme.text_primary)
                    .child(SharedString::new_static("Connections")),
            )
            .child(caption_right);

        // The inline inspector reports Escape-at-root as a dismissal; for
        // this host that means "back to browsing", not "close the dialog".
        if let Phase::Options { inspector, .. } = &self.phase
            && inspector.read(cx).dismissed
        {
            self.leave_options(window, cx);
        }

        let body = match &self.phase {
            Phase::Browse => self.render_browse(&theme.clone(), cx),
            Phase::Options { target, inspector } => {
                self.render_options(&target.clone(), &inspector.clone(), &theme.clone(), cx)
            }
            Phase::ManualAddress { error } => {
                self.render_manual_address(&error.clone(), &theme.clone(), cx)
            }
            Phase::LayoutChoice { layout, choice } => {
                self.render_layout_choice(&layout.clone(), *choice, &theme.clone(), cx)
            }
        };

        let mut panel = div()
            .id("connection-picker-panel")
            .key_context("ConnectionPicker TextInput")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.handle_key_down(event, window, cx);
            }))
            .flex()
            .flex_col()
            .w(px(DIALOG_WIDTH))
            .max_h(px(560.0))
            .bg(theme.bg_elevated)
            .border_1()
            .border_color(theme.border_primary)
            .rounded(px(8.0))
            .child(caption)
            .child(body);

        if self.dismissable(cx) {
            panel = panel.on_mouse_down_out(cx.listener(
                |this, _: &gpui::MouseDownEvent, window, cx| {
                    this.dismiss(window, cx);
                    cx.notify();
                },
            ));
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
            .pt(px(70.0))
            .child(panel)
            .shadow_sm();

        deferred(centered).with_priority(1).into_any_element()
    }
}
