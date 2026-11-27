//! The window's top-level tabs, drawn along the titlebar or down a sidebar.
//!
//! The bar is chrome the window root composes, not part of [`TileGroup`]'s
//! own render, because where it sits — in the titlebar beside the other
//! controls, or as a sidebar the full height of the window — is a global
//! preference ([`TabPlacement`]) that changes the whole window's frame.
use crate::theme::PanelThemeExt;
use std::rc::Rc;

use gpui::{
    AnyElement, App, ClickEvent, DragMoveEvent, Entity, MouseButton, Pixels, ScrollHandle,
    SharedString, div, prelude::*, px,
};

use super::TileGroup;
use super::drag::{ActiveDrag, DragGhost, DragSource, DraggedPane, DraggedTab, set_active_drag};
use super::motion::{MovingTab, Rail, TabSizes};
use crate::config::TabPlacement;
use metor_gpui::Icon;
use metor_gpui::{CONTROL_HEIGHT, CONTROL_RADIUS, Theme, theme};

/// Identity of a tab, stable while tabs are added, closed and reordered
/// around it, and unique across windows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TabId(pub(super) u64);

/// The motion slot of the "new tab" button, which slides along behind the
/// last tab. Tabs are numbered from 1, so this never names one.
const NEW_TAB_SLOT: TabId = TabId(0);

/// A tab along the titlebar sizes to its title up to this width, then
/// ellipsizes so one long title can't crowd every other tab off the bar.
pub(crate) const TAB_MAX_WIDTH: f32 = 220.0;
const TAB_MIN_WIDTH: f32 = 80.0;
/// Space between neighbouring tabs.
const TAB_GAP: f32 = 2.0;
const SIDEBAR_WIDTH: f32 = 200.0;
const MIN_SIDEBAR_WIDTH: f32 = 140.0;
const MAX_SIDEBAR_WIDTH: f32 = 420.0;

/// What a group's tab bar keeps between frames.
#[derive(Default)]
pub(super) struct BarState {
    pub(super) sizes: TabSizes,
    pub(super) scroll: ScrollHandle,
    /// Where a tab dragged over this bar would land, shown as a gap.
    pub(super) preview: Option<DragPreview>,
    /// The tab being dragged out of this bar, whose slot closes at once.
    pub(super) lifted: Option<TabId>,
    /// The active tab last scrolled into view.
    pub(super) revealed: Option<TabId>,
}

#[derive(Clone, Copy, PartialEq)]
pub(super) struct DragPreview {
    pub tab: TabId,
    pub before: Option<TabId>,
    pub extent: Pixels,
}

/// The tab the dragged one lands before, judged from slot centres along
/// the bar as it shows while the tab is lifted — its own slot closed, no
/// gap open yet — rather than from animated hitboxes, so a still pointer
/// cannot keep swapping the target as neighbours slide past it.
fn insertion_before(
    slots: impl IntoIterator<Item = (TabId, Pixels)>,
    dragged: TabId,
    pointer: Pixels,
) -> Option<TabId> {
    let mut start = px(0.0);
    for (id, extent) in slots {
        if id == dragged {
            continue;
        }
        if pointer < start + extent / 2.0 {
            return Some(id);
        }
        start += extent;
    }
    None
}

pub(crate) fn placement(cx: &App) -> TabPlacement {
    cx.try_global::<crate::config::PanelConfig>()
        .map(|settings| settings.tab_placement)
        .unwrap_or_default()
}

/// Move every window's tabs to `placement` and remember the choice.
pub(crate) fn set_placement(placement: TabPlacement, cx: &mut App) {
    let settings = cx.global_mut::<crate::config::PanelConfig>();
    settings.tab_placement = placement;
    if let Err(error) = crate::config::save(&settings) {
        tracing::error!(%error, "save tab placement failed");
    }
    cx.refresh_windows();
}

pub(crate) fn sidebar_width(cx: &App) -> Pixels {
    px(cx
        .try_global::<crate::config::PanelConfig>()
        .and_then(|settings| settings.tab_sidebar_width)
        .unwrap_or(SIDEBAR_WIDTH))
}

/// Resize the sidebar as its edge is dragged. Kept in memory only; the
/// drop that ends the drag saves it with [`save_sidebar_width`].
pub(crate) fn set_sidebar_width(width: Pixels, cx: &mut App) {
    let width = f32::from(width).clamp(MIN_SIDEBAR_WIDTH, MAX_SIDEBAR_WIDTH);
    let settings = cx.global_mut::<crate::config::PanelConfig>();
    if settings.tab_sidebar_width != Some(width) {
        settings.tab_sidebar_width = Some(width);
        cx.refresh_windows();
    }
}

pub(crate) fn save_sidebar_width(cx: &App) {
    if let Some(settings) = cx.try_global::<crate::config::PanelConfig>()
        && let Err(error) = crate::config::save(&settings)
    {
        tracing::error!(%error, "save tab sidebar width failed");
    }
}

/// The extent a tab takes along the bar: its measured size plus the gap
/// after it, or a typical tab's before it has been drawn.
fn slot_extent(sizes: &TabSizes, tab: TabId, vertical: bool) -> Pixels {
    let size = sizes.borrow().get(&tab).copied();
    let extent = match size {
        Some(size) if vertical => size.height,
        Some(size) => size.width,
        None if vertical => px(CONTROL_HEIGHT),
        None => px(TAB_MAX_WIDTH / 2.0),
    };
    extent + px(TAB_GAP)
}

/// `group`'s tabs laid out for `placement`: a row that scrolls sideways in
/// the titlebar, or a column for the sidebar. Either ends in a "new tab"
/// button and a stretch of open bar that takes a dropped pane as a new
/// last tab. Tabs slide when the order changes, and a tab dragged over the
/// bar opens a gap where it would land.
pub(crate) fn tab_list(
    group: &Entity<TileGroup>,
    placement: TabPlacement,
    cx: &mut App,
) -> AnyElement {
    let theme = theme(cx);
    let vertical = placement == TabPlacement::Side;
    let dragging = cx.has_active_drag();
    let (active, tabs, preview, lifted, sizes, scroll, reveal) = group.update(cx, |group, cx| {
        if !dragging {
            group.bar.preview = None;
            group.bar.lifted = None;
        }
        let active = group.active_tab();
        let tabs: Vec<_> = group
            .tabs
            .iter()
            .map(|tab| (tab.id, tab.title(cx), tab.can_close(cx)))
            .collect();
        let reveal = group.bar.revealed != Some(active);
        group.bar.revealed = Some(active);
        (
            active,
            tabs,
            group.bar.preview,
            group.bar.lifted,
            group.bar.sizes.clone(),
            group.bar.scroll.clone(),
            reveal,
        )
    });
    let lone = tabs.len() == 1;

    let rail = Rc::new(Rail::new(sizes.clone()));
    let tracker = rail.clone();
    let mut list = div()
        .id("top-tabs")
        .debug_selector(|| "top-tabs".into())
        .relative()
        .flex()
        .min_w_0()
        .gap(px(TAB_GAP))
        .track_scroll(&scroll)
        .child(
            gpui::canvas(
                move |bounds, _, _| tracker.bounds.set(bounds),
                |_, _, _, _| {},
            )
            .absolute()
            .size_full(),
        );
    list = if vertical {
        list.flex_col()
            .w_full()
            .flex_1()
            .min_h_0()
            .px(px(6.0))
            .overflow_y_scroll()
    } else {
        list.flex_row()
            .items_center()
            .flex_1()
            .h_full()
            .overflow_x_scroll()
    };
    // Along the titlebar a plain wheel scrolls the row sideways.
    list.style().restrict_scroll_to_axis = Some(vertical);

    let mut order: Vec<Option<usize>> = (0..tabs.len())
        .filter(|ix| {
            let id = tabs[*ix].0;
            Some(id) != lifted && preview.is_none_or(|preview| preview.tab != id)
        })
        .map(Some)
        .collect();
    // A lifted tab leaves its slot open until the pointer, over the bar,
    // picks another; the gap stays put while the pointer is off the bar.
    let landing = preview.or_else(|| {
        let tab = lifted?;
        let ix = tabs.iter().position(|(id, ..)| *id == tab)?;
        Some(DragPreview {
            tab,
            before: tabs.get(ix + 1).map(|(id, ..)| *id),
            extent: slot_extent(&sizes, tab, vertical),
        })
    });
    if let Some(landing) = landing {
        let slot = order
            .iter()
            .position(|ix| Some(tabs[ix.unwrap()].0) == landing.before)
            .unwrap_or(order.len());
        order.insert(slot, None);
    }
    if reveal
        && let Some(slot) = order
            .iter()
            .position(|ix| ix.is_some_and(|ix| tabs[ix].0 == active))
    {
        // The measuring canvas is the bar's first child.
        scroll.scroll_to_item(slot + 1);
    }

    let animate = !dragging || preview.is_some() || lifted.is_some();
    for ix in order {
        match ix {
            Some(ix) => {
                let (id, title, can_close) = tabs[ix].clone();
                let tab = render_tab(
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
                );
                list = list.child(MovingTab::new(id, tab, rail.clone(), animate, vertical));
            }
            None => {
                let extent = landing.map(|landing| landing.extent - px(TAB_GAP));
                let gap = div().flex_shrink_0();
                list = list.child(match (vertical, extent) {
                    (true, Some(extent)) => gap.w_full().h(extent),
                    (false, Some(extent)) => gap.h(px(CONTROL_HEIGHT)).w(extent),
                    (_, None) => gap,
                });
            }
        }
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
            .h(px(CONTROL_HEIGHT))
            .px(px(8.0))
            .rounded(px(CONTROL_RADIUS))
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
    list = list.child(MovingTab::new(
        NEW_TAB_SLOT,
        new_tab,
        rail.clone(),
        animate,
        vertical,
    ));

    // Open bar past the last tab. Not occluded, so it stays part of the
    // titlebar's window-drag area.
    let drop_fill = theme.drop_fill();
    let tail = {
        let group = group.clone();
        div()
            .id("top-tabs-tail")
            .debug_selector(|| "top-tabs-tail".into())
            .flex_1()
            .min_w(px(24.0))
            .when(vertical, |tail| tail.min_h(px(24.0)).w_full())
            .when(!vertical, |tail| tail.h(px(CONTROL_HEIGHT)))
            .rounded(px(CONTROL_RADIUS))
            .drag_over::<DraggedPane>(move |s, _, _, _| s.bg(drop_fill))
            .on_drop(move |dragged: &DraggedPane, _, cx| {
                group.update(cx, |group, cx| {
                    group.move_pane_to_new_tab(dragged.pane.clone(), cx)
                });
            })
    };

    let preview_group = group.clone();
    let drop_group = group.clone();
    list.child(tail)
        .on_drag_move(move |event: &DragMoveEvent<DraggedTab>, _, cx| {
            // Off the bar the gap stays where the pointer last left it,
            // for a tab of this bar; another window's tab takes its gap away.
            if !event.bounds.contains(&event.event.position) {
                if event.drag(cx).group != preview_group {
                    preview_group.update(cx, |group, cx| {
                        if group.bar.preview.take().is_some() {
                            cx.notify();
                        }
                    });
                }
                return;
            }
            let next = Some({
                let dragged = event.drag(cx);
                let extent = slot_extent(&dragged.group.read(cx).bar.sizes, dragged.tab, vertical);
                let bar = &preview_group.read(cx).bar;
                let relative = event.event.position - event.bounds.origin - bar.scroll.offset();
                let pointer = if vertical { relative.y } else { relative.x };
                let before = insertion_before(
                    preview_group.read(cx).tabs.iter().map(|tab| {
                        let extent = if tab.id == dragged.tab {
                            extent
                        } else {
                            slot_extent(&bar.sizes, tab.id, vertical)
                        };
                        (tab.id, extent)
                    }),
                    dragged.tab,
                    pointer,
                );
                DragPreview {
                    tab: dragged.tab,
                    before,
                    extent,
                }
            });
            preview_group.update(cx, |group, cx| {
                if group.bar.preview != next {
                    group.bar.preview = next;
                    cx.notify();
                }
            });
        })
        .on_drop(move |dragged: &DraggedTab, _, cx| drop_tab(&drop_group, dragged, cx))
        .into_any_element()
}

struct TabView {
    id: TabId,
    ix: usize,
    title: SharedString,
    active: bool,
    can_close: bool,
    vertical: bool,
}

fn render_tab(group: &Entity<TileGroup>, tab: TabView, theme: &Theme) -> impl IntoElement {
    let TabView {
        id,
        ix,
        title,
        active,
        can_close,
        vertical,
    } = tab;
    let hover_group: SharedString = format!("top-tab-{}", id.0).into();
    let bg_primary = theme.bg_primary;
    let text_primary = theme.text_primary;
    let border_primary = theme.border_primary;
    let drop_accent = theme.drop_accent();
    let drop_fill = theme.drop_fill();

    let mut element = div()
        .id(("top-tab", id.0 as usize))
        .debug_selector(move || format!("top-tab-{ix}"))
        .group(hover_group.clone())
        .occlude()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(4.0))
        .h(px(CONTROL_HEIGHT))
        .flex_shrink_0()
        .pl(px(8.0))
        .pr(px(4.0))
        .rounded(px(CONTROL_RADIUS))
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
            .border_color(border_primary)
            .text_color(text_primary)
            .shadow_xs()
    } else {
        element
            .border_color(gpui::transparent_black())
            .text_color(theme.text_secondary)
            .hover(move |s| s.bg(bg_primary).text_color(text_primary))
    };

    let close = close_icon(theme)
        .id(("top-tab-close", id.0 as usize))
        .when(!active, |close| {
            close
                .opacity(0.0)
                .group_hover(hover_group.clone(), |s| s.opacity(1.0))
        })
        .hover(move |s| s.bg(border_primary))
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
    let click_group = group.clone();
    let middle_group = group.clone();
    let tab_group = group.clone();
    let menu_group = group.clone();
    element
        .drag_over::<DraggedPane>(move |s, _, _, _| s.border_color(drop_accent).bg(drop_fill))
        // Keep the press from arming the titlebar's window move, which would
        // take the pointer before the tab's own drag could start. The tab's
        // click and drag tracking have already seen it.
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(move |_, _, cx| {
            click_group.update(cx, |group, cx| group.activate_tab(id, cx));
        })
        .on_mouse_down(MouseButton::Right, move |event, _, cx| {
            cx.stop_propagation();
            let position = event.position;
            menu_group.update(cx, |_, cx| {
                cx.emit(super::TileGroupEvent::InspectTab { tab: id, position })
            });
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
            move |dragged, grab, window, cx| {
                set_active_drag(
                    ActiveDrag {
                        source: DragSource::Tab(dragged.group.clone(), dragged.tab),
                        source_window: window.window_handle(),
                        grab,
                        closable: can_close,
                    },
                    cx,
                );
                let (title, size) = dragged.group.update(cx, |group, cx| {
                    group.bar.lifted = Some(dragged.tab);
                    cx.notify();
                    let title = group
                        .tab_index(dragged.tab)
                        .map(|ix| group.tabs[ix].title(cx))
                        .unwrap_or_default();
                    (title, group.bar.sizes.borrow().get(&dragged.tab).copied())
                });
                let width = size.map(|size| size.width);
                crate::drag_chip::begin(title.clone(), width, window, cx);
                cx.new(|_| DragGhost {
                    title,
                    width,
                    closable: can_close,
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
        // Tabs occlude the bar beneath them, so they take a tab drop too.
        .on_drop(move |dragged: &DraggedTab, _, cx| drop_tab(&tab_group, dragged, cx))
        .child(div().flex_1().min_w_0().truncate().child(title))
        .when(can_close, |tab| tab.child(close))
}

/// A tab's close button, shared with the ghost a dragged tab lifts as.
pub(super) fn close_icon(theme: &Theme) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .justify_center()
        .size(px(16.0))
        .flex_shrink_0()
        .rounded(px(3.0))
        .child(Icon::Close.svg(9.0, theme.text_secondary))
}

/// Land a dragged tab in `group` where its preview gap was (last without
/// one): a reorder within its own window, or a move from another.
fn drop_tab(group: &Entity<TileGroup>, dragged: &DraggedTab, cx: &mut App) {
    let before = group.update(cx, |group, _| {
        group
            .bar
            .preview
            .take()
            .filter(|preview| preview.tab == dragged.tab)
            .and_then(|preview| preview.before)
    });
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insertion_follows_the_centres_of_the_bar_without_the_lifted_tab() {
        let slots = [
            (TabId(1), px(100.0)),
            (TabId(2), px(100.0)),
            (TabId(3), px(100.0)),
        ];
        assert_eq!(insertion_before(slots, TabId(9), px(40.0)), Some(TabId(1)));
        assert_eq!(insertion_before(slots, TabId(9), px(60.0)), Some(TabId(2)));
        assert_eq!(insertion_before(slots, TabId(2), px(140.0)), Some(TabId(3)));
        assert_eq!(insertion_before(slots, TabId(9), px(290.0)), None);
        // With 1 lifted, 3 shows at 100..200: past its centre is the end.
        assert_eq!(insertion_before(slots, TabId(1), px(160.0)), None);
        assert_eq!(insertion_before(slots, TabId(1), px(40.0)), Some(TabId(2)));
    }
}
