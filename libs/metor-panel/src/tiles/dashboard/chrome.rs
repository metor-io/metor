//! Canvas chrome rendered atop dashboard widgets in edit mode.
use gpui::{Bounds, Context, IntoElement, canvas, fill, point, prelude::*, px, size};

use crate::theme::theme;

use super::{DashboardPanel, SNAP_GRID_PX};

impl DashboardPanel {
    /// Paint a subtle grid aligned to the snap step, panning with scroll.
    pub(super) fn render_grid_overlay(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let grid_color = theme.border_primary.opacity(0.3);
        let bounds = self.container_bounds;
        let offset = self.scroll_offset;

        let grid_step = SNAP_GRID_PX;
        canvas(
            move |_, _, _| {},
            move |_, _, window, _| {
                let Some(bounds) = bounds else { return };
                let w = f32::from(bounds.size.width);
                let h = f32::from(bounds.size.height);

                let start_x = offset.x.rem_euclid(grid_step);
                let mut x_off = start_x;
                while x_off < w {
                    let origin = point(bounds.origin.x + px(x_off), bounds.origin.y);
                    let sz = size(px(1.0), bounds.size.height);
                    window.paint_quad(fill(Bounds { origin, size: sz }, grid_color));
                    x_off += grid_step;
                }

                let start_y = offset.y.rem_euclid(grid_step);
                let mut y_off = start_y;
                while y_off < h {
                    let origin = point(bounds.origin.x, bounds.origin.y + px(y_off));
                    let sz = size(bounds.size.width, px(1.0));
                    window.paint_quad(fill(Bounds { origin, size: sz }, grid_color));
                    y_off += grid_step;
                }
            },
        )
        .size_full()
        .absolute()
    }
}
