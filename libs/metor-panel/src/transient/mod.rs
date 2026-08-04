//! A Magit/emacs-transient "which-key" chord menu.
//!
//! Pressing the configured leader key opens this overlay: a compact rounded
//! sheet floating above the bottom edge of the window showing the keys
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
use crate::theme::theme;
use node::{ChordAction, ChordNode};

/// One level of the navigation stack: the rows to show, plus the crumb label of
/// the submenu that opened it (`None` for the root level).
struct Level {
    crumb: Option<SharedString>,
    nodes: Vec<ChordNode>,
}

/// The chord menu overlay. Owns a navigation stack over a [`ChordNode`] tree and
/// renders the current level as a compact sheet floating above the bottom edge
/// of the window, sized to its content.
pub struct Transient {
    /// Display string of the leader key, shown as the leading breadcrumb.
    leader: SharedString,
    stack: Vec<Level>,
    focus_handle: FocusHandle,
    parent_focus: Option<FocusHandle>,
    pub dismissed: bool,
}

impl Transient {
    pub fn new(leader: SharedString, nodes: Vec<ChordNode>, cx: &mut Context<Self>) -> Self {
        Self {
            leader,
            stack: vec![Level { crumb: None, nodes }],
            focus_handle: cx.focus_handle(),
            parent_focus: None,
            dismissed: false,
        }
    }

    pub fn set_parent_focus(&mut self, handle: FocusHandle) {
        self.parent_focus = Some(handle);
    }

    fn current_nodes(&self) -> &[ChordNode] {
        &self.stack.last().expect("stack is never empty").nodes
    }

    fn dismiss(&mut self, window: &mut Window) {
        self.dismissed = true;
        if let Some(parent) = &self.parent_focus {
            parent.focus(window);
        } else {
            window.blur();
        }
    }

    /// Pop one level; dismiss when already at the root.
    fn back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.stack.len() > 1 {
            self.stack.pop();
            cx.notify();
        } else {
            self.dismiss(window);
            cx.notify();
        }
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
                self.dismiss(window);
                cx.notify();
            }
            // Unknown key at this level: ignore so a stray press is harmless.
            None => {}
        }
    }
}

impl Focusable for Transient {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Transient {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.dismissed {
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

        // Key grid: each available key with its label, wrapping across rows.
        // Scrolls within the sheet's height cap when a level has many keys.
        let mut grid = div()
            .id("transient-grid")
            .flex()
            .flex_row()
            .flex_wrap()
            .gap_x(px(16.0))
            .gap_y(px(2.0))
            .min_h_0()
            .overflow_y_scroll()
            .text_size(px(12.0));
        for node in self.current_nodes() {
            grid = grid.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        div()
                            .min_w(px(14.0))
                            .text_color(theme.control_active)
                            .child(node.key.clone()),
                    )
                    .child(
                        div()
                            .text_color(theme.text_primary)
                            .child(node.label.clone()),
                    ),
            );
        }

        let panel = div()
            .id("transient-panel")
            .key_context("Transient")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.handle_key_down(event, window, cx);
            }))
            .on_mouse_down_out(cx.listener(|this, _: &gpui::MouseDownEvent, window, _cx| {
                this.dismiss(window);
            }))
            .occlude()
            .absolute()
            .bottom(px(12.0))
            .left(px(12.0))
            .right(px(12.0))
            // Hug the content; the cap only kicks in for key-heavy levels,
            // which then scroll inside the grid.
            .max_h(relative(0.4))
            .flex()
            .flex_col()
            .gap(px(4.0))
            .bg(theme.bg_elevated)
            .border_1()
            .border_color(theme.border_primary)
            .rounded(px(6.0))
            .px(px(6.0))
            .py(px(4.0))
            .shadow_sm()
            .child(crumbs)
            .child(grid);

        deferred(panel).with_priority(1).into_any_element()
    }
}
