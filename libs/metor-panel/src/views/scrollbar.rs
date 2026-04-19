use gpui::{div, prelude::*, px, Axis, IntoElement};

use crate::theme::theme;

const TRACK_SIZE: f32 = 8.0;
const THUMB_SIZE: f32 = 6.0;
const THUMB_RADIUS: f32 = 3.0;
const MIN_THUMB: f32 = 25.0;
const TRACK_MARGIN: f32 = 1.0;

/// A simple scrollbar thumb rendered as an absolutely-positioned div.
///
/// Place this as a child of a `relative()` container. It computes the thumb
/// position and size from the viewport size, content size, and current scroll
/// offset, then renders a rounded bar along the specified axis.
#[derive(IntoElement)]
pub struct Scrollbar {
    axis: Axis,
    viewport: f32,
    content: f32,
    offset: f32,
}

impl Scrollbar {
    /// Create a scrollbar for the given axis.
    ///
    /// - `viewport`: visible size along the axis (px)
    /// - `content`: total content size along the axis (px)
    /// - `offset`: current scroll position (positive = scrolled into content)
    pub fn new(axis: Axis, viewport: f32, content: f32, offset: f32) -> Self {
        Self {
            axis,
            viewport,
            content,
            offset,
        }
    }

    /// Whether the scrollbar should be visible (content overflows viewport).
    pub fn visible(&self) -> bool {
        self.content > self.viewport
    }
}

impl gpui::RenderOnce for Scrollbar {
    fn render(self, _window: &mut gpui::Window, cx: &mut gpui::App) -> impl IntoElement {
        if !self.visible() {
            return div().into_any_element();
        }

        let theme = theme(cx);
        let thumb_color = theme.text_tertiary.opacity(0.4);

        let thumb_frac = (self.viewport / self.content).min(1.0);
        let thumb_len = (self.viewport * thumb_frac).max(MIN_THUMB);
        let max_scroll = self.content - self.viewport;
        let scroll_frac = self.offset.clamp(0.0, max_scroll) / max_scroll;
        let thumb_offset = scroll_frac * (self.viewport - thumb_len);

        let thumb = div()
            .absolute()
            .bg(thumb_color)
            .rounded(px(THUMB_RADIUS));

        let (track, thumb) = match self.axis {
            Axis::Vertical => (
                div()
                    .absolute()
                    .top_0()
                    .right(px(TRACK_MARGIN))
                    .w(px(TRACK_SIZE))
                    .h_full(),
                thumb
                    .top(px(thumb_offset))
                    .right(px((TRACK_SIZE - THUMB_SIZE) / 2.0))
                    .w(px(THUMB_SIZE))
                    .h(px(thumb_len)),
            ),
            Axis::Horizontal => (
                div()
                    .absolute()
                    .bottom(px(TRACK_MARGIN))
                    .left_0()
                    .w_full()
                    .h(px(TRACK_SIZE)),
                thumb
                    .left(px(thumb_offset))
                    .bottom(px((TRACK_SIZE - THUMB_SIZE) / 2.0))
                    .w(px(thumb_len))
                    .h(px(THUMB_SIZE)),
            ),
        };

        track.child(thumb).into_any_element()
    }
}
