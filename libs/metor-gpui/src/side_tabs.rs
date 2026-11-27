//! A vertical column of navigation tabs, styled like the panel's tab bar.

use gpui::{
    AnyElement, App, ClickEvent, Div, ElementId, IntoElement, RenderOnce, SharedString, Stateful,
    Window, div, prelude::*, px,
};

use crate::{CONTROL_HEIGHT, CONTROL_RADIUS, Icon, theme};

/// Padding around a [`side_tabs`] column.
pub const SIDE_TABS_PADDING: f32 = 6.0;
/// Gap between neighbouring tabs.
pub const SIDE_TABS_GAP: f32 = 2.0;

type OnClick = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

/// One tab: a control-height pill with an optional leading icon.
///
/// The element is stateless; the app owns which tab is active. A host that needs handlers this
/// builder has no method for, such as the panel's drag and drop, takes the styled div from
/// [`SideTab::into_div`] and adds them there.
#[derive(IntoElement)]
pub struct SideTab {
    id: ElementId,
    label: SharedString,
    icon: Option<Icon>,
    active: bool,
    disabled: bool,
    end: Option<AnyElement>,
    on_click: Option<OnClick>,
}

/// A tab labelled `label`, like gpui's `div()`. See [`SideTab`].
pub fn side_tab(id: impl Into<ElementId>, label: impl Into<SharedString>) -> SideTab {
    SideTab {
        id: id.into(),
        label: label.into(),
        icon: None,
        active: false,
        disabled: false,
        end: None,
        on_click: None,
    }
}

/// The column tabs stack in, on the sidebar tint.
pub fn side_tabs(cx: &App) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(SIDE_TABS_GAP))
        .p(px(SIDE_TABS_PADDING))
        .bg(theme(cx).tab_sidebar_bg())
}

impl SideTab {
    /// Shows `icon` before the label.
    pub fn icon(mut self, icon: Icon) -> Self {
        self.icon = Some(icon);
        self
    }

    /// Marks the tab as the open page.
    pub fn active(mut self, active: bool) -> Self {
        self.active = active;
        self
    }

    /// Dims the tab and ignores clicks, for a page that exists but cannot open yet.
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// Places `element` after the label, at the trailing edge.
    pub fn end(mut self, element: impl IntoElement) -> Self {
        self.end = Some(element.into_any_element());
        self
    }

    /// Calls `handler` when the tab is clicked, unless it is disabled.
    pub fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Box::new(handler));
        self
    }

    /// The styled tab, full width, for hosts that add handlers of their own.
    pub fn into_div(self, cx: &App) -> Stateful<Div> {
        let theme = theme(cx);
        let bg_primary = theme.bg_primary;
        let text_primary = theme.text_primary;
        let text_color = if self.active {
            theme.text_primary
        } else if self.disabled {
            theme.text_tertiary
        } else {
            theme.text_secondary
        };
        div()
            .id(self.id)
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .w_full()
            .h(px(CONTROL_HEIGHT))
            .flex_shrink_0()
            .pl(px(8.0))
            .pr(px(4.0))
            .rounded(px(CONTROL_RADIUS))
            .border_1()
            .text_size(px(12.0))
            .text_color(text_color)
            .map(|tab| {
                if self.active {
                    tab.bg(bg_primary)
                        .border_color(theme.border_primary)
                        .shadow_xs()
                } else {
                    tab.border_color(gpui::transparent_black())
                }
            })
            .when(!self.active && !self.disabled, |tab| {
                tab.cursor_pointer()
                    .hover(move |s| s.bg(bg_primary).text_color(text_primary))
            })
            .when_some(self.icon, |tab, icon| tab.child(icon.svg(14.0, text_color)))
            .child(div().flex_1().min_w_0().truncate().child(self.label))
            .children(self.end)
            .when_some(self.on_click.filter(|_| !self.disabled), |tab, on_click| {
                tab.on_click(on_click)
            })
    }
}

impl RenderOnce for SideTab {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        self.into_div(cx)
    }
}
