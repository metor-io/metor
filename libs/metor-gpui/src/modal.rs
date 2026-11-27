//! A centered dialog over a dimmed page, and the confirm dialog built on it.

use gpui::{
    AnyElement, App, ClickEvent, IntoElement, MouseButton, RenderOnce, SharedString, Window, div,
    prelude::*, px,
};

use crate::{CONTROL_HEIGHT, CONTROL_RADIUS, StateTone, Theme, theme};

type Handler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

/// A dialog card centered over a scrim that covers its parent.
///
/// Render it as the last child of a `relative()` root so it overlays the page. Clicks on the
/// scrim call [`Modal::on_dismiss`]; clicks inside the card don't reach it. The app owns whether
/// the modal is shown and handles `escape` through its own key bindings.
#[derive(IntoElement)]
pub struct Modal {
    title: SharedString,
    body: Vec<AnyElement>,
    actions: Vec<AnyElement>,
    on_dismiss: Option<Handler>,
}

impl Modal {
    /// An empty dialog titled `title`.
    pub fn new(title: impl Into<SharedString>) -> Self {
        Self {
            title: title.into(),
            body: Vec::new(),
            actions: Vec::new(),
            on_dismiss: None,
        }
    }

    /// Content under the title.
    pub fn child(mut self, element: impl IntoElement) -> Self {
        self.body.push(element.into_any_element());
        self
    }

    /// A button in the trailing action row, in order.
    pub fn action(mut self, element: impl IntoElement) -> Self {
        self.actions.push(element.into_any_element());
        self
    }

    /// Called when the scrim outside the card is clicked.
    pub fn on_dismiss(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_dismiss = Some(Box::new(handler));
        self
    }

    /// A dialog asking to confirm `message`, with Cancel and a confirm button labelled
    /// `confirm`. A [`StateTone::Failure`] tone marks the confirm button as destructive.
    pub fn confirm(
        title: impl Into<SharedString>,
        message: impl Into<SharedString>,
        confirm: impl Into<SharedString>,
        tone: StateTone,
        on_confirm: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
        on_cancel: impl Fn(&ClickEvent, &mut Window, &mut App) + Clone + 'static,
    ) -> Self {
        Self::new(title)
            .child(message.into())
            .action(DialogButton::new("modal-cancel", "Cancel", None).on_click(on_cancel.clone()))
            .action(DialogButton::new("modal-confirm", confirm, Some(tone)).on_click(on_confirm))
            .on_dismiss(on_cancel)
    }
}

/// A dialog button. With a tone it is filled in that tone's color, for the primary action.
#[derive(IntoElement)]
pub struct DialogButton {
    id: gpui::ElementId,
    label: SharedString,
    tone: Option<StateTone>,
    on_click: Option<Handler>,
}

impl DialogButton {
    /// A button labelled `label`, filled in `tone` when it has one.
    pub fn new(
        id: impl Into<gpui::ElementId>,
        label: impl Into<SharedString>,
        tone: Option<StateTone>,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            tone,
            on_click: None,
        }
    }

    /// Calls `handler` when the button is clicked.
    pub fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Box::new(handler));
        self
    }
}

impl RenderOnce for DialogButton {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = theme(cx);
        let (bg, border, text) = match self.tone {
            Some(tone) => {
                let color = theme.state_color(tone);
                (Theme::dim(color, 0.2), color, theme.text_primary)
            }
            None => (
                theme.bg_elevated,
                theme.border_primary,
                theme.text_secondary,
            ),
        };
        let hover = theme.selection_bg;
        div()
            .id(self.id)
            .flex()
            .items_center()
            .h(px(CONTROL_HEIGHT))
            .px(px(12.0))
            .rounded(px(CONTROL_RADIUS))
            .border_1()
            .border_color(border)
            .bg(bg)
            .text_size(px(12.0))
            .text_color(text)
            .cursor_pointer()
            .hover(move |s| s.bg(hover))
            .child(self.label)
            .when_some(self.on_click, |button, on_click| button.on_click(on_click))
    }
}

impl RenderOnce for Modal {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = theme(cx);
        let card = div()
            .id("modal-card")
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(|_, _, cx| cx.stop_propagation())
            .flex()
            .flex_col()
            .gap(px(12.0))
            .min_w(px(320.0))
            .max_w(px(520.0))
            .p(px(16.0))
            .rounded(px(6.0))
            .bg(theme.surface_bg())
            .backdrop_blur(crate::SURFACE_BLUR)
            .border_1()
            .border_color(theme.border_primary)
            .shadow_lg()
            .text_size(px(12.0))
            .text_color(theme.text_secondary)
            .child(
                div()
                    .text_size(px(14.0))
                    .text_color(theme.text_primary)
                    .child(self.title),
            )
            .children(self.body)
            .when(!self.actions.is_empty(), |card| {
                card.child(
                    div()
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap(px(8.0))
                        .children(self.actions),
                )
            });
        div()
            .id("modal-scrim")
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .bg(theme.scrim())
            .when_some(self.on_dismiss, |scrim, on_dismiss| {
                scrim.on_click(on_dismiss)
            })
            .child(card)
    }
}
