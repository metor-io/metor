//! Two-step wizard for constructing a single [`XyTrace`].
//!
//! Page stack: pick X component → pick X element → pick Y component →
//! pick Y element → commit. A trace's color comes from the theme's
//! categorical palette indexed by `color_basis` (the parent plot's
//! existing trace count, or zero for fresh plots).
//!
//! Either axis will take a `=` expression, independently of the other, and an
//! expression cascades into the same element page a picked component does —
//! an XY axis is one scalar, and a rank-1 expression has to say which of its
//! elements that is.

use std::sync::{Arc, Mutex};

use gpui::{App, SharedString, Window};
use metor_db::DB;

use crate::inspector::rows::{InspectorRow, RowAction};
use crate::inspector::trace_picker::{Channel, ColorBasis, OnChannel, channel_picker_rows};
use crate::theme::theme;

use super::XyTrace;

/// Invoked with the [`XyTrace`] the user built through the wizard.
pub type OnXyTraceSelected = Arc<dyn Fn(XyTrace, &mut Window, &mut App)>;

/// The X pick, carried from its page to the Y page's commit.
#[derive(Default)]
struct XyDraft {
    x: Option<Channel>,
}

/// Build the wizard's first page: the X axis channel.
pub fn select_xy_trace_wizard_rows(
    db: Arc<DB>,
    color_basis: ColorBasis,
    on_select: OnXyTraceSelected,
) -> Vec<Box<dyn InspectorRow>> {
    let draft = Arc::new(Mutex::new(XyDraft::default()));
    let db_for_y = db.clone();
    let on_x: OnChannel = Arc::new(move |x, _window, _cx| {
        draft.lock().unwrap().x = Some(x);
        let draft = draft.clone();
        let color_basis = color_basis.clone();
        let on_select = on_select.clone();
        let on_y: OnChannel = Arc::new(move |y, window, cx| {
            let Some(x) = draft.lock().unwrap().x.take() else {
                return RowAction::Pop;
            };
            let theme = theme(cx);
            let base = (color_basis)(cx);
            let color = theme.line_colors[base % theme.line_colors.len()];
            on_select(xy_trace(x, y, color), window, cx);
            RowAction::Dismiss
        });
        RowAction::Cascade(channel_picker_rows(
            db_for_y.clone(),
            "Pick Y axis component, or type an expression",
            on_y,
        ))
    });
    channel_picker_rows(db, "Pick X axis component, or type an expression", on_x)
}

/// One XY trace over two picked channels, labelled `x vs y`.
pub(crate) fn xy_trace(x: Channel, y: Channel, color: gpui::Hsla) -> XyTrace {
    let mut t = XyTrace::new(x.component, x.element, y.component, y.element, color);
    t.label = SharedString::from(format!("{} vs {}", x.label, y.label));
    t.x_expression = x.expression;
    t.y_expression = y.expression;
    t
}
