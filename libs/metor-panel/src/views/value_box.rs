//! The panel's one editable value box: a value on a box's surface that a
//! click asks to type into, and that shows the edit field in its place
//! while it is typed. The [`Scrubber`](super::scrubber::Scrubber) is this
//! box with a fill, a handle and a drag on top; inspector text and number
//! rows are the bare box. Like [`Switch`](super::switch::Switch) it is
//! sized by its height.

use std::rc::Rc;

use gpui::{
    AnyElement, App, ContentMask, CursorStyle, ElementId, MouseButton, SharedString, Stateful,
    TextRun, Window, canvas, div, point, prelude::*, px,
};

use crate::theme::Theme;
use crate::views::value_strip::BOX_RADIUS;

/// Called when a click (a press released without dragging) asks to type a
/// value.
pub(crate) type OnEdit = Rc<dyn Fn(&mut Window, &mut App)>;

/// The hover group every value box opens, so what is layered in it (a
/// scrubber's handle) can show on hover.
pub(crate) const HOVER_GROUP: &str = "value-box";

/// How a value box looks and what a click on it asks; the caller owns the
/// value and the edit, and styles its own element with [`ValueBox::apply`].
pub(crate) struct ValueBox {
    id: ElementId,
    text: SharedString,
    height: f32,
    text_size: Option<f32>,
    align_end: bool,
    on_edit: Option<OnEdit>,
    editing: Option<AnyElement>,
    layers: Vec<AnyElement>,
}

impl ValueBox {
    /// A box `height` pixels tall showing `text`.
    pub(crate) fn new(
        id: impl Into<ElementId>,
        text: impl Into<SharedString>,
        height: f32,
    ) -> Self {
        Self {
            id: id.into(),
            text: text.into(),
            height,
            text_size: None,
            align_end: false,
            on_edit: None,
            editing: None,
            layers: Vec::new(),
        }
    }

    /// The text size, where the host's text is a fixed size rather than
    /// the switch's 11/24 of the height.
    pub(crate) fn text_size(mut self, size: f32) -> Self {
        self.text_size = Some(size);
        self
    }

    /// Set the value against the right edge, where it lines up with other
    /// values in a column (an inspector's), rather than centred.
    pub(crate) fn align_end(mut self) -> Self {
        self.align_end = true;
        self
    }

    /// Let a click ask to type a value.
    pub(crate) fn on_edit(mut self, edit: OnEdit) -> Self {
        self.on_edit = Some(edit);
        self
    }

    /// Show `field` inside the box in place of the value while the caller
    /// edits it as text.
    pub(crate) fn editing(mut self, field: impl IntoElement) -> Self {
        self.editing = Some(field.into_any_element());
        self
    }

    /// Lay `layer` across the box beneath the value, positioned against
    /// the box: a scrubber's fill and handle.
    pub(crate) fn layer(mut self, layer: impl IntoElement) -> Self {
        self.layers.push(layer.into_any_element());
        self
    }

    /// Style `el` as this box. A press on it doesn't reach the elements
    /// behind it, so a host that acts on mouse-down (an inspector row)
    /// leaves the box to decide between a click and a drag.
    pub(crate) fn apply(self, el: gpui::Div, theme: &Theme) -> Stateful<gpui::Div> {
        let h = self.height;
        let editing = self.editing.is_some();
        // The value is shaped and painted the way the edit field paints its
        // text, against the same edge of the same padded area, so starting
        // an edit doesn't move it.
        let area = div()
            .relative()
            .size_full()
            .flex()
            .items_center()
            .px(px(h / 3.0));
        let body = match self.editing {
            Some(field) => area.child(field),
            None => area.child(painted_text(
                self.text,
                px(self.text_size.unwrap_or(h * 11.0 / 24.0)),
                theme.text_primary,
                self.align_end,
            )),
        };
        let el = el
            .id(self.id)
            .debug_selector(|| "value-box".into())
            .group(HOVER_GROUP)
            .relative()
            .flex()
            .items_center()
            .h(px(h))
            .rounded(px(BOX_RADIUS))
            .overflow_hidden()
            .bg(theme.bg_secondary)
            .children(self.layers)
            .child(body)
            // The edit ring is drawn over the box, not as its border, so the
            // value doesn't shift inward when editing starts.
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
        el.on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .when_some(self.on_edit, |el, edit| {
                el.cursor(CursorStyle::IBeam)
                    .on_click(move |_, window, cx| edit(window, cx))
            })
    }
}

/// `text` shaped and painted against the area's right edge or centre, on a
/// line as tall as the area, clipped to it.
fn painted_text(
    text: SharedString,
    font_size: gpui::Pixels,
    color: gpui::Hsla,
    align_end: bool,
) -> impl IntoElement {
    canvas(
        move |_, window, _| {
            let run = TextRun {
                len: text.len(),
                font: window.text_style().font(),
                color,
                background_color: None,
                underline: None,
                strikethrough: None,
                letter_spacing: None,
            };
            window
                .text_system()
                .shape_line(text.clone(), font_size, &[run])
        },
        move |bounds, shaped, window, cx| {
            let x = match align_end {
                true => bounds.right() - shaped.width,
                false => bounds.origin.x + (bounds.size.width - shaped.width) / 2.0,
            };
            let origin = point(x.max(bounds.origin.x), bounds.origin.y);
            window.with_content_mask(
                Some(ContentMask {
                    bounds,
                    fade_out: Default::default(),
                }),
                |window| {
                    let _ = shaped.paint(
                        origin,
                        bounds.size.height,
                        gpui::TextAlign::Left,
                        None,
                        window,
                        cx,
                    );
                },
            );
        },
    )
    .size_full()
}
