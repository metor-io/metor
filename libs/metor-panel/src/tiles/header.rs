//! A pane's header as its own cached view.
//!
//! A pane redraws whenever the view in it does, and a live plot does every
//! frame. Rebuilt with it, the header (a title and up to four icon buttons
//! with tooltips) cost every pane its layout and paint each frame. Cached,
//! it replays its last frame until what it shows changes.
//!
//! The header can't read its title from the pane's view: that view notifies
//! with every frame of data, and reading it would invalidate the header as
//! often. The pane, which renders every frame anyway, works out a
//! [`HeaderState`] instead and hands over a changed one.

use gpui::{
    AnyElement, ClickEvent, Context, Entity, IntoElement, MouseButton, Render, SharedString,
    StyleRefinement, Styled, WeakEntity, Window, div, prelude::*, px,
};

use super::drag::{DraggedPane, SplitDirection};
use super::pane::{HEADER_HEIGHT, Pane, PaneEvent};
use crate::icons::Icon;
use crate::theme::theme;

/// Everything the header shows, as of the pane's last render.
#[derive(Clone, PartialEq)]
pub(super) struct HeaderState {
    pub(super) title: SharedString,
    pub(super) can_close: bool,
    /// Pinned in a dock: no zoom button.
    pub(super) docked: bool,
    /// Holds a view, so the header drags the pane.
    pub(super) has_item: bool,
    /// Laid over the top of the view rather than above it.
    pub(super) over: bool,
}

pub(super) struct PaneHeader {
    pane: WeakEntity<Pane>,
    state: HeaderState,
}

impl PaneHeader {
    pub(super) fn new(pane: WeakEntity<Pane>, state: HeaderState) -> Self {
        Self { pane, state }
    }

    /// The header as the pane's child, showing `state`. A changed state is
    /// handed over after this frame (a notify sent while drawing would land
    /// no sooner), so the header trails a change by one frame.
    pub(super) fn element(
        header: &Entity<Self>,
        state: HeaderState,
        cx: &mut Context<Pane>,
    ) -> AnyElement {
        let over = state.over;
        if header.read(cx).state != state {
            let header = header.clone();
            cx.defer(move |cx| {
                header.update(cx, |header, cx| {
                    if header.state != state {
                        header.state = state;
                        cx.notify();
                    }
                });
            });
        }
        let style = if over {
            StyleRefinement::default()
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .h(px(HEADER_HEIGHT))
        } else {
            StyleRefinement::default()
                .w_full()
                .h(px(HEADER_HEIGHT))
                .flex_shrink_0()
        };
        header.clone().cached(style).into_any_element()
    }
}

/// Run `f` on the pane, if it is still alive.
fn with_pane(
    pane: &WeakEntity<Pane>,
    cx: &mut gpui::App,
    f: impl FnOnce(&mut Pane, &mut Context<Pane>),
) {
    let _ = pane.update(cx, f);
}

impl Render for PaneHeader {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let HeaderState {
            title,
            can_close,
            docked,
            has_item,
            over,
        } = self.state.clone();

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
        let emit = |event: fn() -> PaneEvent| {
            let pane = self.pane.clone();
            move |_: &ClickEvent, _: &mut Window, cx: &mut gpui::App| {
                cx.stop_propagation();
                with_pane(&pane, cx, |_, cx| cx.emit(event()));
            }
        };

        let pane = self.pane.clone();
        let aux_pane = self.pane.clone();
        let inspect_pane = self.pane.clone();
        let close_pane = self.pane.clone();
        let mut header = div()
            .id("pane-header")
            .debug_selector(|| "pane-header".into())
            .flex()
            .flex_row()
            .items_center()
            .gap(px(2.0))
            .size_full()
            .pl(px(8.0))
            .pr(px(3.0))
            // Part of the panel rather than a strip above it, as in Rex: the
            // views' own background, which the tab bar shares too. Laid over
            // a plot, it frosts the data running beneath.
            .map(|header| {
                if over {
                    header
                        .bg(theme.plot_chrome_bg())
                        .backdrop_blur(crate::theme::CHROME_BLUR)
                } else {
                    header.bg(theme.bg_secondary)
                }
            })
            .text_size(px(12.0))
            .text_color(theme.text_secondary)
            .on_click(move |event: &ClickEvent, _, cx| {
                if event.click_count() == 2 {
                    with_pane(&pane, cx, |_, cx| cx.emit(PaneEvent::ToggleZoom));
                }
            })
            .on_aux_click(move |event: &ClickEvent, _, cx| {
                if event.is_middle_click() {
                    with_pane(&aux_pane, cx, |pane, cx| pane.close(cx));
                }
            })
            .on_mouse_down(MouseButton::Right, move |event, _, cx| {
                let position = event.position;
                with_pane(&inspect_pane, cx, |pane, cx| match pane.item() {
                    Some(item) => cx.emit(PaneEvent::Inspect {
                        item: item.clone_handle(),
                        position,
                    }),
                    None => cx.emit(PaneEvent::InspectPane { position }),
                });
            })
            .child(div().flex_1().min_w_0().truncate().child(title.clone()))
            .child(
                button("pane-split-right", Icon::SplitRight, "Split right").on_click(emit(|| {
                    PaneEvent::Split {
                        direction: SplitDirection::Right,
                        item: None,
                    }
                })),
            )
            .child(
                button("pane-split-down", Icon::SplitDown, "Split down").on_click(emit(|| {
                    PaneEvent::Split {
                        direction: SplitDirection::Down,
                        item: None,
                    }
                })),
            )
            .when(!docked, |header| {
                header.child(
                    button("pane-zoom", Icon::Maximize, "Zoom")
                        .on_click(emit(|| PaneEvent::ToggleZoom)),
                )
            })
            .when(can_close, |header| {
                header.child(button("pane-close", Icon::Close, "Close").on_click(
                    move |_, _, cx| {
                        cx.stop_propagation();
                        with_pane(&close_pane, cx, |pane, cx| pane.close(cx));
                    },
                ))
            });

        if has_item && let Some(pane) = self.pane.upgrade() {
            header = header.cursor_grab().on_drag(
                DraggedPane { pane },
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
