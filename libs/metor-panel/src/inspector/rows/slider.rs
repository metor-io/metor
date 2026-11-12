use std::sync::Arc;

use gpui::{AnyElement, App, SharedString, Window};

use super::{InspectorRow, RowAction, boxed_row, passive, row_box};
use crate::views::scrubber::Scrubber;
use crate::views::value_box::ValueBox;

/// Inspector row for a bounded numeric field, drawn as a scrubber: drag the
/// box to move the value, click it or press Enter to type one, and press
/// Left or Right to step it.
///
/// `on_change` runs whenever a drag or a step changes the value, with the
/// value snapped to the step (a tenth of it while Shift is held), and when
/// a typed value is committed, kept within the range.
pub struct SliderRow {
    pub label: SharedString,
    pub read_value: Arc<dyn Fn(&App) -> f64>,
    pub min: f64,
    pub max: f64,
    /// Keep the value whole: the step is at least one, and Shift slows a
    /// drag without making the step finer.
    pub integer: bool,
    pub on_change: Arc<dyn Fn(f64, &mut Window, &mut App)>,
}

impl SliderRow {
    /// The row's scrubber; a drag changes the value only when `scrubs`.
    fn scrubber(&self, scrubs: bool, cx: &App) -> Scrubber {
        Scrubber::new(
            (self.read_value)(cx),
            (self.min, self.max),
            self.integer,
            scrubs.then(|| self.on_change.clone()),
        )
    }

    fn boxed(&self, row_ix: usize, cx: &App) -> ValueBox {
        let scrubber = self.scrubber(!passive(cx), cx);
        row_box(row_ix, scrubber.text(), cx).scrubber(scrubber)
    }
}

impl InspectorRow for SliderRow {
    fn label(&self) -> &str {
        &self.label
    }

    fn render_row(
        &self,
        row_ix: usize,
        selected: bool,
        _window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        boxed_row(row_ix, selected, &self.label, self.boxed(row_ix, cx), cx)
    }

    fn value_box(&self, row_ix: usize, cx: &App) -> Option<ValueBox> {
        Some(self.boxed(row_ix, cx))
    }

    fn nudge(&mut self, direction: f64, fine: bool, window: &mut Window, cx: &mut App) -> bool {
        let value = (self.read_value)(cx);
        let next = self.scrubber(false, cx).nudged(direction, fine);
        if next != value {
            (self.on_change)(next, window, cx);
        }
        true
    }

    fn activate(&mut self, _window: &mut Window, cx: &mut App) -> RowAction {
        let (min, max, integer) = (self.min, self.max, self.integer);
        let on_change = self.on_change.clone();
        RowAction::StartEdit {
            current_text: self.scrubber(false, cx).text(),
            on_commit: Box::new(move |text, window, cx| {
                let Ok(value) = text.trim().parse::<f64>() else {
                    return;
                };
                if !value.is_finite() {
                    return;
                }
                // Not `clamp`, which panics on an inverted or NaN range.
                let value = value.max(min).min(max);
                on_change(if integer { value.round() } else { value }, window, cx);
            }),
        }
    }
}
