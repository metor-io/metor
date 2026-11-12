//! An inspector row's value drawn in a box. A click on the box asks the
//! inspector to edit the row, and while the row is edited the edit field
//! takes the value's place inside the box. A [`Scrubber`] adds a fill, a
//! hover handle and a drag.

use gpui::{
    AnyElement, CursorStyle, ElementId, MouseButton, SharedString, Stateful, TextRun, TruncateFrom,
    canvas, div, point, prelude::*, px,
};

use crate::inspector::ActivateSelectedRow;
use crate::inspector::rows::LABEL_SIZE;
use crate::theme::Theme;
use crate::views::scrubber::Scrubber;
use crate::views::value_strip::BOX_RADIUS;

/// Height of a value box: it fits inside an inspector row's selection pill
/// and matches the edit field's 20px line.
pub(crate) const HEIGHT: f32 = 20.0;
/// Width of every value box, so the values of different rows line up in
/// one column.
const WIDTH: f32 = 140.0;
/// Horizontal padding inside the box, the same as the edit field's, so the
/// text doesn't move when an edit starts.
const PADDING: f32 = HEIGHT / 3.0;

/// Hover-group name set on every value box, so the scrubber's handle inside
/// it can appear on hover.
pub(crate) const HOVER_GROUP: &str = "value-box";

pub struct ValueBox {
    id: ElementId,
    text: SharedString,
    editable: bool,
    editing: Option<AnyElement>,
    scrubber: Option<Scrubber>,
}

impl ValueBox {
    /// A box showing `text`. When `editable`, a click on it dispatches
    /// [`ActivateSelectedRow`].
    pub(crate) fn new(
        id: impl Into<ElementId>,
        text: impl Into<SharedString>,
        editable: bool,
    ) -> Self {
        Self {
            id: id.into(),
            text: text.into(),
            editable,
            editing: None,
            scrubber: None,
        }
    }

    /// Show the edit field `field` in place of the text.
    pub(crate) fn editing(mut self, field: AnyElement) -> Self {
        self.editing = Some(field);
        self
    }

    /// Make the box a slider over the scrubber's range.
    pub(crate) fn scrubber(mut self, scrubber: Scrubber) -> Self {
        self.scrubber = Some(scrubber);
        self
    }

    /// The box as an element. It stops left mouse-downs from reaching the
    /// row behind it, which acts on mouse-down, so a press here can still
    /// become either a click or a drag.
    pub(crate) fn render(self, theme: &Theme) -> Stateful<gpui::Div> {
        let editing = self.editing.is_some();
        let scrubs = !editing && self.scrubber.as_ref().is_some_and(Scrubber::scrubs);
        let body = div()
            .relative()
            .size_full()
            .flex()
            .items_center()
            .px(px(PADDING));
        let body = match self.editing {
            Some(field) => body.child(field),
            None => body.child(painted_text(self.text, theme.text_primary)),
        };
        let el = div()
            .id(self.id)
            .debug_selector(|| "value-box".into())
            .group(HOVER_GROUP)
            .relative()
            .flex_none()
            .w(px(WIDTH))
            .h(px(HEIGHT))
            .flex()
            .items_center()
            .rounded(px(BOX_RADIUS))
            .overflow_hidden()
            .bg(theme.bg_secondary)
            .children(self.scrubber.as_ref().map(|s| s.fill(theme)))
            .when_some(self.scrubber.as_ref().filter(|_| scrubs), |el, s| {
                el.child(s.handle(theme))
            })
            .child(body)
            // Drawn over the box rather than as its border, so the text
            // doesn't shift inward when an edit starts.
            .when(editing, |el| {
                el.child(
                    div()
                        .absolute()
                        .inset_0()
                        .rounded(px(BOX_RADIUS))
                        .border_1()
                        .border_color(theme.line_color),
                )
            });
        if editing {
            return el;
        }
        let el = el
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .when(self.editable, |el| {
                el.cursor(CursorStyle::IBeam).on_click(|_, window, cx| {
                    window.dispatch_action(Box::new(ActivateSelectedRow), cx)
                })
            });
        match self.scrubber {
            Some(scrubber) if scrubs => scrubber.attach(el),
            _ => el,
        }
    }
}

/// `text` against the right edge of its area, ending in an ellipsis when it
/// doesn't fit. It is shaped and painted the way the edit field paints its
/// text, so starting an edit doesn't move it.
fn painted_text(text: SharedString, color: gpui::Hsla) -> impl IntoElement {
    canvas(
        move |bounds, window, _| {
            let font = window.text_style().font();
            let run = TextRun {
                len: text.len(),
                font: font.clone(),
                color,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let runs = [run];
            let text_system = window.text_system();
            let (text, runs) = text_system.line_wrapper(font, LABEL_SIZE).truncate_line(
                text.clone(),
                bounds.size.width,
                "…",
                &runs,
                TruncateFrom::End,
            );
            text_system.shape_line(text, LABEL_SIZE, &runs, None)
        },
        move |bounds, shaped, window, cx| {
            let x = (bounds.right() - shaped.width).max(bounds.origin.x);
            let _ = shaped.paint(
                point(x, bounds.origin.y),
                bounds.size.height,
                gpui::TextAlign::Left,
                None,
                window,
                cx,
            );
        },
    )
    .size_full()
}
