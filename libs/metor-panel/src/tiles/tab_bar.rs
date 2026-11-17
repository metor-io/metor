//! The window's top-level tabs, drawn along the titlebar or down a sidebar.
//!
//! The bar is chrome the window root composes, not part of [`TileGroup`]'s
//! own render, because where it sits — in the titlebar beside the other
//! controls, or as a sidebar the full height of the window — is a global
//! preference ([`TabPlacement`]) that changes the whole window's frame.
use gpui::{
    AnyElement, App, ClickEvent, DragMoveEvent, Entity, MouseButton, Pixels, SharedString, Window,
    div, prelude::*, px,
};

use super::TileGroup;
use super::drag::{ActiveDrag, DragGhost, DragSource, DraggedPane, DraggedTab, set_active_drag};
use crate::config::TabPlacement;
use crate::icons::Icon;
use crate::theme::{Theme, theme};

/// Identity of a tab within its group, stable while tabs are added, closed
/// and reordered around it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TabId(pub(super) u64);

pub(crate) const TAB_HEIGHT: f32 = 26.0;
/// A tab along the titlebar sizes to its title up to this width, then
/// ellipsizes so one long title can't crowd every other tab off the bar.
pub(crate) const TAB_MAX_WIDTH: f32 = 220.0;
const TAB_MIN_WIDTH: f32 = 64.0;
const SIDEBAR_WIDTH: f32 = 200.0;
const MIN_SIDEBAR_WIDTH: f32 = 140.0;
const MAX_SIDEBAR_WIDTH: f32 = 420.0;

pub(crate) fn placement(cx: &App) -> TabPlacement {
    cx.try_global::<crate::theme::FontSettings>()
        .map(|settings| settings.config.tab_placement)
        .unwrap_or_default()
}

/// Move every window's tabs to `placement` and remember the choice.
pub(crate) fn set_placement(placement: TabPlacement, cx: &mut App) {
    let settings = cx.global_mut::<crate::theme::FontSettings>();
    settings.config.tab_placement = placement;
    if let Err(error) = crate::config::save(&settings.config) {
        tracing::error!(%error, "save tab placement failed");
    }
    cx.refresh_windows();
}

pub(crate) fn sidebar_width(cx: &App) -> Pixels {
    px(cx
        .try_global::<crate::theme::FontSettings>()
        .and_then(|settings| settings.config.tab_sidebar_width)
        .unwrap_or(SIDEBAR_WIDTH))
}

/// Resize the sidebar as its edge is dragged. Kept in memory only; the
/// drop that ends the drag saves it with [`save_sidebar_width`].
pub(crate) fn set_sidebar_width(width: Pixels, cx: &mut App) {
    let width = f32::from(width).clamp(MIN_SIDEBAR_WIDTH, MAX_SIDEBAR_WIDTH);
    let settings = cx.global_mut::<crate::theme::FontSettings>();
    if settings.config.tab_sidebar_width != Some(width) {
        settings.config.tab_sidebar_width = Some(width);
        cx.refresh_windows();
    }
}

pub(crate) fn save_sidebar_width(cx: &App) {
    if let Some(settings) = cx.try_global::<crate::theme::FontSettings>()
        && let Err(error) = crate::config::save(&settings.config)
    {
        tracing::error!(%error, "save tab sidebar width failed");
    }
}

/// `group`'s tabs laid out for `placement`: a row that scrolls sideways in
/// the titlebar, or a column for the sidebar. Either ends in a "new tab"
/// button and a stretch of open bar that takes a dropped pane or tab as a
/// new last tab.
pub(crate) fn tab_list(
    group: &Entity<TileGroup>,
    placement: TabPlacement,
    window: &Window,
    cx: &mut App,
) -> AnyElement {
    let theme = theme(cx);
    let vertical = placement == TabPlacement::Side;
    let state = group.read(cx);
    let active = state.active_tab();
    let tabs: Vec<_> = state
        .tabs
        .iter()
        .map(|tab| (tab.id, tab.title(cx), tab.can_close(cx)))
        .collect();
    let lone = tabs.len() == 1;

    let mut list = div()
        .id("top-tabs")
        .debug_selector(|| "top-tabs".into())
        .flex()
        .min_w_0()
        .gap(px(2.0));
    list = if vertical {
        list.flex_col().w_full().flex_1().min_h_0().px(px(6.0)).overflow_y_scroll()
    } else {
        list.flex_row().items_center().flex_1().h_full().overflow_x_scroll()
    };
    list.style().restrict_scroll_to_axis = Some(true);

    for (ix, (id, title, can_close)) in tabs.into_iter().enumerate() {
        list = list.child(render_tab(
            group,
            TabView {
                id,
                ix,
                title,
                active: id == active,
                can_close: can_close && !lone,
                vertical,
            },
            &theme,
            window,
        ));
    }

    let bg_primary = theme.bg_primary;
    let new_tab = {
        let group = group.clone();
        div()
            .id("new-top-tab")
            .debug_selector(|| "new-top-tab".into())
            .occlude()
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap(px(6.0))
            .h(px(TAB_HEIGHT))
            .px(px(if vertical { 10.0 } else { 6.0 }))
            .rounded(px(6.0))
            .cursor_pointer()
            .text_size(px(12.0))
            .text_color(theme.text_tertiary)
            .hover(move |s| s.bg(bg_primary))
            .child(Icon::Add.svg(12.0, theme.text_tertiary))
            .when(vertical, |row| row.child("New tab"))
            .tooltip(|_, cx| crate::views::tooltip::TooltipText::build("New tab".into(), cx))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(move |_, _, cx| {
                group.update(cx, |group, cx| {
                    group.new_tab(cx);
                });
            })
    };
    list = list.child(new_tab);

    // Open bar past the last tab. Not occluded, so it stays part of the
    // titlebar's window-drag area.
    let drop_target = theme.drop_target;
    let tail = {
        let pane_group = group.clone();
        let tab_group = group.clone();
        div()
            .id("top-tabs-tail")
            .debug_selector(|| "top-tabs-tail".into())
            .flex_1()
            .min_w(px(24.0))
            .when(vertical, |tail| tail.min_h(px(24.0)).w_full())
            .when(!vertical, |tail| tail.h_full())
            .rounded(px(6.0))
            .drag_over::<DraggedPane>(move |s, _, _, _| s.bg(drop_target))
            .on_drop(move |dragged: &DraggedPane, _, cx| {
                pane_group.update(cx, |group, cx| {
                    group.move_pane_to_new_tab(dragged.pane.clone(), cx)
                });
            })
            .on_drop(move |dragged: &DraggedTab, _, cx| drop_tab(&tab_group, dragged, None, cx))
    };
    list.child(tail).into_any_element()
}

struct TabView {
    id: super::TabId,
    ix: usize,
    title: SharedString,
    active: bool,
    can_close: bool,
    vertical: bool,
}

fn render_tab(
    group: &Entity<TileGroup>,
    tab: TabView,
    theme: &Theme,
    window: &Window,
) -> impl IntoElement {
    let TabView {
        id,
        ix,
        title,
        active,
        can_close,
        vertical,
    } = tab;
    let hover_group: SharedString = format!("top-tab-{}", id.0).into();
    let text_style = window.text_style();
    let bg_primary = theme.bg_primary;
    let text_primary = theme.text_primary;
    let border_primary = theme.border_primary;
    let control_active = theme.control_active;

    let mut element = div()
        .id(("top-tab", id.0 as usize))
        .debug_selector(move || format!("top-tab-{ix}"))
        .group(hover_group.clone())
        .occlude()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(4.0))
        .h(px(TAB_HEIGHT))
        .flex_shrink_0()
        .pl(px(10.0))
        .pr(px(4.0))
        .rounded(px(6.0))
        .border_1()
        .cursor_pointer()
        .text_size(px(12.0));
    element = if vertical {
        element.w_full()
    } else {
        element.min_w(px(TAB_MIN_WIDTH)).max_w(px(TAB_MAX_WIDTH))
    };
    element = if active {
        element
            .bg(bg_primary)
            .border_color(theme.border_primary)
            .text_color(text_primary)
            .shadow_xs()
    } else {
        element
            .border_color(gpui::transparent_black())
            .text_color(theme.text_secondary)
            .hover(move |s| s.bg(bg_primary).text_color(text_primary))
    };

    let close = div()
        .id(("top-tab-close", id.0 as usize))
        .flex()
        .items_center()
        .justify_center()
        .size(px(16.0))
        .flex_shrink_0()
        .rounded(px(3.0))
        .when(!active, |close| {
            close
                .opacity(0.0)
                .group_hover(hover_group.clone(), |s| s.opacity(1.0))
        })
        .hover(move |s| s.bg(border_primary))
        .child(Icon::Close.svg(9.0, theme.text_secondary))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click({
            let group = group.clone();
            move |_, _, cx| {
                cx.stop_propagation();
                group.update(cx, |group, cx| group.close_tab(id, cx));
            }
        });

    let drag_group = group.clone();
    let spring_group = group.clone();
    let pane_group = group.clone();
    let tab_group = group.clone();
    let click_group = group.clone();
    let middle_group = group.clone();
    element
        .drag_over::<DraggedPane>(move |s, _, _, _| s.border_color(control_active))
        .drag_over::<DraggedTab>(move |s, _, _, _| s.border_color(control_active))
        // Keep the press from arming the titlebar's window move, which would
        // take the pointer before the tab's own drag could start. The tab's
        // click and drag tracking have already seen it.
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(move |_, _, cx| {
            click_group.update(cx, |group, cx| group.activate_tab(id, cx));
        })
        .on_aux_click(move |event: &ClickEvent, _, cx| {
            if event.is_middle_click() && can_close {
                middle_group.update(cx, |group, cx| group.close_tab(id, cx));
            }
        })
        .on_drag(
            DraggedTab {
                group: drag_group,
                tab: id,
            },
            move |dragged, _, window, cx| {
                set_active_drag(
                    ActiveDrag {
                        source: DragSource::Tab(dragged.group.clone(), dragged.tab),
                        source_window: window.window_handle(),
                    },
                    cx,
                );
                let title = title_for_ghost(&dragged.group, dragged.tab, cx);
                cx.new(|_| DragGhost {
                    title,
                    text_style: text_style.clone(),
                })
            },
        )
        .on_drag_move(move |event: &DragMoveEvent<DraggedPane>, _, cx| {
            let over = event.bounds.contains(&event.event.position);
            spring_group.update(cx, |group, cx| {
                if over {
                    group.spring_load(Some(id), cx);
                } else if group.spring.as_ref().is_some_and(|(tab, _)| *tab == id) {
                    group.spring_load(None, cx);
                }
            });
        })
        .on_drop(move |dragged: &DraggedPane, _, cx| {
            pane_group.update(cx, |group, cx| {
                group.move_pane_to_tab(dragged.pane.clone(), id, cx)
            });
        })
        .on_drop(move |dragged: &DraggedTab, _, cx| {
            // Dropped on a later tab, the dragged one lands after it; on an
            // earlier one, before it — the way the pointer was travelling.
            let before = tab_group.read(cx).tab_index(id).and_then(|target| {
                let from = (dragged.group == tab_group)
                    .then(|| tab_group.read(cx).tab_index(dragged.tab))
                    .flatten();
                if from.is_some_and(|from| from < target) {
                    tab_group.read(cx).tabs.get(target + 1).map(|tab| tab.id)
                } else {
                    Some(id)
                }
            });
            drop_tab(&tab_group, dragged, before, cx);
        })
        .child(div().flex_1().min_w_0().truncate().child(title))
        .when(can_close, |tab| tab.child(close))
}

fn title_for_ghost(group: &Entity<TileGroup>, id: super::TabId, cx: &App) -> SharedString {
    let group = group.read(cx);
    group
        .tab_index(id)
        .map(|ix| group.tabs[ix].title(cx))
        .unwrap_or_default()
}

/// Land a dragged tab in `group` before `before` (last without one):
/// a reorder within its own window, or a move from another.
fn drop_tab(
    group: &Entity<TileGroup>,
    dragged: &DraggedTab,
    before: Option<super::TabId>,
    cx: &mut App,
) {
    if &dragged.group == group {
        group.update(cx, |group, cx| {
            group.move_tab(dragged.tab, before, cx);
            group.activate_tab(dragged.tab, cx);
        });
    } else {
        let source = dragged.group.clone();
        let tab = dragged.tab;
        group.update(cx, |group, cx| group.adopt_tab(&source, tab, before, cx));
    }
}
