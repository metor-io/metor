use gpui::{
    Bounds, ClickEvent, Context, EventEmitter, IntoElement, MouseButton, Pixels, Point, Render,
    SharedString, WeakEntity, Window, div, prelude::*, px,
};
use metor_proto::types::ComponentId;
use smallvec::SmallVec;

use super::drag::{DraggedPane, SplitDirection};
use super::item::PaneItemHandle;
use crate::icons::Icon;
use crate::theme::theme;

/// Dispatched by views inside a pane to open a new plot beside that pane.
///
/// The payload is deliberately minimal — `ComponentId` and element indices —
/// so the action can implement `PartialEq`. Color, label, and style derive
/// from theme and metadata via [`crate::views::time_series::traces_for_component`].
#[derive(Clone, PartialEq, gpui::Action)]
#[action(no_json)]
pub struct PlotComponentAction {
    pub component_id: ComponentId,
    pub indices: SmallVec<[usize; 4]>,
}

/// Opens an image view of an image component in the active pane.
#[derive(Clone, PartialEq, gpui::Action)]
#[action(no_json)]
pub struct OpenImageAction {
    pub component_id: ComponentId,
}

/// Dispatched by the outline to open another outline beside its pane,
/// rooted on `root` (a full component path).
#[derive(Clone, PartialEq, gpui::Action)]
#[action(no_json)]
pub struct OpenOutlineAction {
    pub root: SharedString,
}

/// Dispatched by the outline to open a pivot table beside its pane,
/// gathering whatever the right-clicked branch stood for.
#[derive(Clone, PartialEq, gpui::Action)]
#[action(no_json)]
pub struct OpenPivotTableAction {
    pub config: crate::views::PivotTableConfig,
}

/// Dispatched while shift is held over a component name to open a transient
/// plot preview anchored at the cursor. An empty `indices` means all
/// elements; surfaces with per-element granularity (value strips) pass a
/// single index. AppRoot owns the preview lifecycle and dismisses on
/// shift release.
#[derive(Clone, PartialEq, gpui::Action)]
#[action(no_json)]
pub struct PreviewPlotAction {
    pub component_id: ComponentId,
    pub indices: SmallVec<[usize; 4]>,
    pub anchor: Point<Pixels>,
}

/// Height of a pane's header row.
pub(super) const HEADER_HEIGHT: f32 = crate::theme::CONTROL_HEIGHT;

/// Messages a pane sends to the [`TileGroup`](super::TileGroup) that owns it.
///
/// Splits and removals happen at the tile-group level so the whole tree can
/// be mutated coherently; the pane only signals what the user wanted.
pub enum PaneEvent {
    /// Open a new pane beside this one along `direction`, showing `item` or,
    /// with none, the starter cards.
    Split {
        direction: SplitDirection,
        item: Option<Box<dyn PaneItemHandle>>,
    },
    /// The item was closed (or the empty pane dismissed); the tile group may
    /// drop the pane entirely.
    Empty,
    /// User requested an inspector anchored at `position` (typically from a
    /// header's right-click).
    Inspect {
        item: Box<dyn PaneItemHandle>,
        position: Point<Pixels>,
    },
    InspectPane {
        position: Point<Pixels>,
    },
    /// The header was double-clicked or its zoom button pressed: fill the
    /// tab with this pane, or put the layout back if it already does.
    ToggleZoom,
    /// The empty pane's Time Series button was pressed.
    NewTimeSeries,
}

impl EventEmitter<PaneEvent> for Pane {}

/// A leaf of the split tree, showing one [`PaneItem`](super::PaneItem)
/// under a header.
///
/// The header names the item, carries the split/zoom/close controls, and is
/// the handle a pane is dragged by. Tabs live only at the top of the window
/// (see [`TileGroup`](super::TileGroup)), so a pane never holds more than
/// one item; asking a full pane to [`add_item`](Pane::add_item) opens the
/// new item in a pane beside it instead.
pub struct Pane {
    item: Option<Box<dyn PaneItemHandle>>,
    /// Where the whole pane (header included) was last drawn: the drop
    /// region a dragged pane splits against, and the size a lock captures.
    bounds: Bounds<Pixels>,
    hide_header: bool,
    locked_size: Option<gpui::Size<f32>>,
    /// The group holding this pane, set when one adopts it. A pane dropped
    /// in another window is taken out of its old group through this.
    pub(super) group: Option<WeakEntity<super::TileGroup>>,
}

impl Pane {
    pub fn new(item: Option<Box<dyn PaneItemHandle>>, _cx: &mut Context<Self>) -> Self {
        Self {
            item,
            bounds: Bounds::default(),
            hide_header: false,
            locked_size: None,
            group: None,
        }
    }

    pub fn hide_header(&self) -> bool {
        self.hide_header
    }

    pub fn set_hide_header(&mut self, hide: bool, cx: &mut Context<Self>) {
        self.hide_header = hide;
        cx.notify();
    }

    pub fn locked_size(&self) -> Option<gpui::Size<f32>> {
        self.locked_size
    }

    /// The pane's drawn size, header included. Used by the inspector toggle
    /// to capture "lock at the current size".
    pub fn current_outer_size(&self) -> gpui::Size<f32> {
        gpui::Size {
            width: self.bounds.size.width.into(),
            height: self.bounds.size.height.into(),
        }
    }

    pub fn set_locked_size(&mut self, size: Option<gpui::Size<f32>>, cx: &mut Context<Self>) {
        self.locked_size = size;
        cx.notify();
    }

    pub(crate) fn bounds(&self) -> Bounds<Pixels> {
        self.bounds
    }

    /// The group holding this pane, if any still does.
    pub fn owner(&self) -> Option<gpui::Entity<super::TileGroup>> {
        self.group.as_ref()?.upgrade()
    }

    pub fn item(&self) -> Option<&dyn PaneItemHandle> {
        self.item.as_deref()
    }

    pub fn is_empty(&self) -> bool {
        self.item.is_none()
    }

    /// Show `item` here when the pane is empty, otherwise in a new pane
    /// split off along this pane's longer side.
    pub fn add_item(&mut self, item: Box<dyn PaneItemHandle>, cx: &mut Context<Self>) {
        if self.item.is_none() {
            self.item = Some(item);
            cx.notify();
        } else {
            cx.emit(PaneEvent::Split {
                direction: SplitDirection::along_longer(self.bounds.size),
                item: Some(item),
            });
        }
    }

    /// Close the item, honouring [`PaneItemHandle::can_close`], and ask the
    /// group to drop the pane. An empty pane closes unconditionally.
    pub fn close(&mut self, cx: &mut Context<Self>) {
        if self.item.as_ref().is_some_and(|item| !item.can_close(cx)) {
            return;
        }
        self.item = None;
        cx.emit(PaneEvent::Empty);
        cx.notify();
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let title = self
            .item
            .as_ref()
            .map(|item| item.tab_title(cx))
            .unwrap_or_else(|| SharedString::new_static("New panel"));
        let can_close = self.item.as_ref().is_none_or(|item| item.can_close(cx));

        let button = |id: &'static str, icon: Icon, tooltip: &'static str| {
            let hover_bg = theme.bg_primary;
            div()
                .id(id)
                .debug_selector(move || id.into())
                .flex()
                .items_center()
                .justify_center()
                .size(px(22.0))
                .rounded(px(crate::theme::CONTROL_RADIUS))
                .cursor_pointer()
                .hover(move |s| s.bg(hover_bg))
                .child(icon.svg(12.0, theme.text_tertiary))
                .tooltip(move |_, cx| crate::views::tooltip::TooltipText::build(tooltip.into(), cx))
                // A press on a control must not start a header drag.
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        };

        let mut header = div()
            .id("pane-header")
            .debug_selector(|| "pane-header".into())
            .flex()
            .flex_row()
            .items_center()
            .gap(px(2.0))
            .h(px(HEADER_HEIGHT))
            .flex_shrink_0()
            .pl(px(8.0))
            .pr(px(3.0))
            // Part of the panel rather than a strip above it, as in Rex: the
            // views' own background, which the tab bar shares too.
            .bg(theme.bg_secondary)
            .text_size(px(12.0))
            .text_color(theme.text_secondary)
            .on_click(cx.listener(|_, event: &ClickEvent, _, cx| {
                if event.click_count() == 2 {
                    cx.emit(PaneEvent::ToggleZoom);
                }
            }))
            .on_aux_click(cx.listener(|this, event: &ClickEvent, _, cx| {
                if event.is_middle_click() {
                    this.close(cx);
                }
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(
                    |this, event: &gpui::MouseDownEvent, _, cx| match &this.item {
                        Some(item) => cx.emit(PaneEvent::Inspect {
                            item: item.clone_handle(),
                            position: event.position,
                        }),
                        None => cx.emit(PaneEvent::InspectPane {
                            position: event.position,
                        }),
                    },
                ),
            )
            .child(div().flex_1().min_w_0().truncate().child(title.clone()))
            .child(
                button("pane-split-right", Icon::SplitRight, "Split right").on_click(cx.listener(
                    |_, _, _, cx| {
                        cx.stop_propagation();
                        cx.emit(PaneEvent::Split {
                            direction: SplitDirection::Right,
                            item: None,
                        });
                    },
                )),
            )
            .child(
                button("pane-split-down", Icon::SplitDown, "Split down").on_click(cx.listener(
                    |_, _, _, cx| {
                        cx.stop_propagation();
                        cx.emit(PaneEvent::Split {
                            direction: SplitDirection::Down,
                            item: None,
                        });
                    },
                )),
            )
            .child(
                button("pane-zoom", Icon::Maximize, "Zoom").on_click(cx.listener(|_, _, _, cx| {
                    cx.stop_propagation();
                    cx.emit(PaneEvent::ToggleZoom);
                })),
            )
            .when(can_close, |header| {
                header.child(
                    button("pane-close", Icon::Close, "Close").on_click(cx.listener(
                        |this, _, _, cx| {
                            cx.stop_propagation();
                            this.close(cx);
                        },
                    )),
                )
            });

        if self.item.is_some() {
            header = header.cursor_grab().on_drag(
                DraggedPane { pane: cx.entity() },
                move |dragged, grab, window, cx| {
                    // Mirror the drag app-side so a mouse-up outside the
                    // window (which gpui otherwise swallows) can tear the
                    // pane out.
                    super::drag::set_active_drag(
                        super::drag::ActiveDrag {
                            source: super::drag::DragSource::Pane(dragged.pane.clone()),
                            source_window: window.window_handle(),
                            grab,
                            closable: can_close,
                        },
                        cx,
                    );
                    crate::drag_chip::begin(title.clone(), None, window, cx);
                    cx.new(|_| super::drag::DragGhost {
                        title: title.clone(),
                        width: None,
                        closable: can_close,
                    })
                },
            );
        }
        header
    }
}

impl Render for Pane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let view = cx.entity().clone();
        let bounds_tracker = gpui::canvas(
            move |bounds, _window, cx| {
                view.update(cx, |pane, _| pane.bounds = bounds);
            },
            |_, _, _, _| {},
        )
        .size_full()
        .absolute();

        let mut content = div()
            .id("pane-content")
            .relative()
            .flex_1()
            .min_h_0()
            .w_full()
            .overflow_hidden()
            .bg(theme.bg_secondary);
        content = match &self.item {
            Some(item) => content.child(item.view()),
            None => content.child(self.render_starters(window, cx)),
        };

        div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .child(bounds_tracker)
            .when(!self.hide_header, |pane| pane.child(self.render_header(cx)))
            .child(content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tiles::{PaneItem, TileGroup};
    use gpui::{App, Entity};

    #[derive(serde::Serialize, serde::Deserialize, Default)]
    struct TestItemConfig {}

    struct TestItem(bool);

    impl Render for TestItem {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    impl PaneItem for TestItem {
        type Config = TestItemConfig;

        fn tab_title(&self, _: &App) -> SharedString {
            SharedString::new_static("test")
        }

        fn serialization_key() -> &'static str {
            "test_item"
        }

        fn to_config(&self, _: &App) -> TestItemConfig {
            TestItemConfig {}
        }

        fn can_close(&self, _: &App) -> bool {
            self.0
        }
    }

    fn group_with(closable: bool, cx: &mut gpui::TestAppContext) -> Entity<TileGroup> {
        cx.update(|cx| {
            cx.new(|cx| {
                crate::theme::set_theme(cx, std::sync::Arc::new(crate::theme::DARK.clone()));
                let item = Box::new(cx.new(|_| TestItem(closable))) as Box<dyn PaneItemHandle>;
                TileGroup::new(vec![item], cx)
            })
        })
    }

    fn press(cx: &mut gpui::VisualTestContext, position: Point<Pixels>, button: MouseButton) {
        cx.simulate_event(gpui::MouseDownEvent {
            button,
            position,
            modifiers: gpui::Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        });
        cx.simulate_event(gpui::MouseUpEvent {
            button,
            position,
            modifiers: gpui::Modifiers::default(),
            click_count: 1,
        });
    }

    #[gpui::test]
    fn header_buttons_split_and_middle_click_closes_only_closable_items(
        cx: &mut gpui::TestAppContext,
    ) {
        for closable in [true, false] {
            let group = group_with(closable, cx);
            let (_, cx) = cx.add_window_view({
                let group = group.clone();
                move |_, _| Host(group)
            });
            cx.refresh().unwrap();
            let split = cx.debug_bounds("pane-split-right").unwrap().center();
            cx.simulate_click(split, Default::default());
            cx.refresh().unwrap();
            let panes = cx.update(|_, cx| group.read(cx).panes().to_vec());
            assert_eq!(panes.len(), 2);
            assert!(cx.update(|_, cx| panes[1].read(cx).is_empty()));
            assert!(
                cx.debug_bounds("pane-close").is_some(),
                "an empty pane can always be dismissed"
            );

            let header = cx.update(|_, cx| panes[0].read(cx).bounds().origin)
                + gpui::point(px(20.0), px(HEADER_HEIGHT / 2.0));
            press(cx, header, MouseButton::Middle);
            let remaining = cx.update(|_, cx| group.read(cx).panes().len());
            assert_eq!(remaining, if closable { 1 } else { 2 });
        }
    }

    struct Host(Entity<TileGroup>);

    impl Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(self.0.clone())
        }
    }
}
