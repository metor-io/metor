//! A Magit/emacs-transient "which-key" chord menu.
//!
//! Pressing the configured leader key opens this overlay: a compact rounded
//! popup in the lower-right corner of the window showing the keys
//! available at the current level. Each keystroke either descends into a
//! submenu or runs a command and dismisses. It deliberately parallels the [`Inspector`](crate::inspector)
//! palette — both are data-driven trees over the same command vocabulary — but
//! is key-driven (one keypress navigates) rather than fuzzy-search-driven, so
//! it is a separate, lighter entity rather than another inspector mode.
pub mod menu;
pub mod node;

use gpui::{
    Context, FocusHandle, Focusable, IntoElement, KeyDownEvent, Render, SharedString, Window,
    deferred, div, prelude::*, px, relative,
};

use crate::inspector::rows::tag_pill;
use crate::motion::{self, Fade};
use crate::theme::theme;
use node::{ChordAction, ChordNode};

/// One level of the navigation stack: the rows to show, plus the crumb label of
/// the submenu that opened it (`None` for the root level).
struct Level {
    crumb: Option<SharedString>,
    nodes: Vec<ChordNode>,
}

/// Rows in one column of keys before the level wraps into another column.
const MAX_COLUMN_ROWS: usize = 6;

/// The chord menu overlay. Owns a navigation stack over a [`ChordNode`] tree and
/// renders the current level as a small popup in the window's lower-right
/// corner, sized to its content.
pub struct Transient {
    /// Display string of the leader key, shown as the leading breadcrumb.
    leader: SharedString,
    stack: Vec<Level>,
    focus_handle: FocusHandle,
    parent_focus: Option<FocusHandle>,
    pub dismissed: bool,
    fade: Fade,
    exit_complete: bool,
}

impl Transient {
    pub fn new(leader: SharedString, nodes: Vec<ChordNode>, cx: &mut Context<Self>) -> Self {
        Self {
            leader,
            stack: vec![Level { crumb: None, nodes }],
            focus_handle: cx.focus_handle(),
            parent_focus: None,
            dismissed: false,
            fade: Fade::entrance(motion::TRANSIENT_ENTER),
            exit_complete: false,
        }
    }

    pub fn set_parent_focus(&mut self, handle: FocusHandle) {
        self.parent_focus = Some(handle);
    }

    fn current_nodes(&self) -> &[ChordNode] {
        &self.stack.last().expect("stack is never empty").nodes
    }

    fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dismissed {
            return;
        }
        self.dismissed = true;
        self.fade.exit(motion::PALETTE_EXIT);
        if self.focus_handle.contains_focused(window, cx) {
            if let Some(parent) = &self.parent_focus {
                parent.focus(window, cx);
            } else {
                window.blur(cx);
            }
        }
        cx.notify();
    }

    /// Pop one level; dismiss when already at the root.
    fn back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.stack.len() > 1 {
            self.stack.pop();
            cx.notify();
        } else {
            self.dismiss(window, cx);
            cx.notify();
        }
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.dismissed {
            return;
        }
        let key = event.keystroke.key.as_str();
        match key {
            "escape" | "backspace" => {
                self.back(window, cx);
                return;
            }
            _ => {}
        }

        let selected = self
            .current_nodes()
            .iter()
            .find(|node| node.key.as_ref() == key)
            .map(|node| (node.label.clone(), node.action.clone()));
        match selected {
            Some((label, ChordAction::SubMenu(build))) => {
                let nodes = build(cx);
                self.stack.push(Level {
                    crumb: Some(label),
                    nodes,
                });
                cx.notify();
            }
            Some((_, ChordAction::Command(callback))) => {
                callback(window, cx);
                self.dismiss(window, cx);
                cx.notify();
            }
            // Unknown key at this level: ignore so a stray press is harmless.
            None => {}
        }
    }
}

impl gpui::EventEmitter<motion::Closed> for Transient {}

impl Focusable for Transient {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Transient {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let opacity = self.fade.opacity(window, cx);
        if self.dismissed && opacity == 0.0 {
            if !self.exit_complete {
                self.exit_complete = true;
                cx.emit(motion::Closed);
            }
            return div().into_any_element();
        }
        let theme = theme(cx);

        // Breadcrumb: the leader, then one crumb per descended level.
        let mut crumbs = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .child(tag_pill(self.leader.clone(), cx));
        for level in &self.stack {
            if let Some(crumb) = &level.crumb {
                crumbs = crumbs.child(tag_pill(crumb.clone(), cx));
            }
        }

        // Keys flow down short columns that then wrap sideways, so a level
        // stays a small block in the corner whatever its key count.
        let nodes = self.current_nodes();
        let columns = nodes.len().div_ceil(MAX_COLUMN_ROWS).max(1);
        let rows_per_column = nodes.len().div_ceil(columns).max(1);
        let mut grid = div()
            .id("transient-grid")
            .flex()
            .flex_row()
            .gap_x(px(12.0))
            .min_h_0()
            .when(!self.dismissed, |grid| grid.overflow_y_scroll())
            .text_size(px(11.0));
        for column in nodes.chunks(rows_per_column) {
            let mut col = div().flex().flex_col().gap_y(px(1.0));
            for node in column {
                col = col.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(5.0))
                        .child(
                            div()
                                .min_w(px(10.0))
                                .text_color(theme.control_active)
                                .child(node.key.clone()),
                        )
                        .child(
                            div()
                                .whitespace_nowrap()
                                .text_color(theme.text_primary)
                                .child(node.label.clone()),
                        ),
                );
            }
            grid = grid.child(col);
        }

        let panel = div()
            .id("transient-panel")
            .when(!self.dismissed, |panel| {
                panel
                    .key_context("Transient")
                    .track_focus(&self.focus_handle)
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                        this.handle_key_down(event, window, cx);
                    }))
                    .on_mouse_down_out(cx.listener(|this, _: &gpui::MouseDownEvent, window, cx| {
                        this.dismiss(window, cx);
                    }))
                    .occlude()
            })
            .opacity(opacity)
            .absolute()
            .bottom(px(12.0))
            .right(px(12.0))
            // Hug the content; the caps only kick in for key-heavy levels,
            // which then scroll inside the grid.
            .max_w(relative(0.5))
            .max_h(relative(0.4))
            .flex()
            .flex_col()
            .gap(px(3.0))
            .bg(theme.bg_elevated)
            .border_1()
            .border_color(theme.border_primary)
            .rounded(px(6.0))
            .px(px(8.0))
            .py(px(5.0))
            .shadow_sm()
            .child(crumbs)
            .child(grid);

        deferred(panel).with_priority(1).into_any_element()
    }
}
