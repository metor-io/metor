use std::rc::Rc;
use std::sync::Arc;

use gpui::{AnyElement, App, SharedString, Window};

use super::{BOX_HEIGHT, InspectorRow, LABEL_SIZE, RowAction, box_slot, boxed_row, edit_on_click};
use crate::theme::theme;
use crate::views::scrubber::{self, Scrubber};

/// Inspector row for a bounded numeric field, drawn as a scrubber: drag the
/// box to move the value, click or press Enter to type one, and Left/Right
/// to step it.
///
/// `on_change` runs on every drag tick and nudge, with the value snapped to
/// the step so the readout stays stable.
pub struct SliderRow {
    pub label: SharedString,
    pub read_value: Arc<dyn Fn(&App) -> f64>,
    pub min: f64,
    pub max: f64,
    /// Whole numbers only: the step is never below one.
    pub integer: bool,
    pub on_change: Arc<dyn Fn(f64, &mut Window, &mut App)>,
}

impl SliderRow {
    fn step(&self) -> f64 {
        let step = scrubber::default_step(self.min, self.max);
        match self.integer {
            true => step.max(1.0),
            false => step,
        }
    }

    fn scrubber(&self, row_ix: usize, cx: &App) -> Scrubber {
        Scrubber::new(
            ("slider", row_ix),
            (self.read_value)(cx),
            self.min,
            self.max,
            BOX_HEIGHT,
        )
        .step(self.step())
        .text_size(f32::from(LABEL_SIZE))
        .align_end()
    }

    fn row(&self, row_ix: usize, selected: bool, scrubber: Scrubber, cx: &App) -> AnyElement {
        let scrubber = scrubber.apply(box_slot(), &theme(cx));
        boxed_row(row_ix, selected, self.label.clone(), scrubber, cx)
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
        let mut scrubber = self.scrubber(row_ix, cx);
        if !super::passive(cx) {
            let on_change = self.on_change.clone();
            scrubber = scrubber
                .on_change(Rc::new(move |value, window, cx| {
                    on_change(value, window, cx)
                }))
                .on_edit(edit_on_click());
        }
        self.row(row_ix, selected, scrubber, cx)
    }

    fn render_editing(
        &self,
        row_ix: usize,
        selected: bool,
        field: AnyElement,
        _window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let scrubber = self.scrubber(row_ix, cx).editing(field);
        self.row(row_ix, selected, scrubber, cx)
    }

    fn nudge(&mut self, direction: f64, fine: bool, window: &mut Window, cx: &mut App) -> bool {
        let value = (self.read_value)(cx);
        let fine = fine && !self.integer;
        let next = scrubber::nudged(value, (self.min, self.max), self.step(), direction, fine);
        if next != value {
            (self.on_change)(next, window, cx);
        }
        true
    }

    fn activate(&mut self, _window: &mut Window, cx: &mut App) -> RowAction {
        let (min, max, integer) = (self.min, self.max, self.integer);
        let on_change = self.on_change.clone();
        RowAction::StartEdit {
            current_text: scrubber::format_value((self.read_value)(cx), self.step()),
            on_commit: Box::new(move |text, window, cx| {
                let Ok(value) = text.trim().parse::<f64>() else {
                    return;
                };
                if !value.is_finite() {
                    return;
                }
                let value = value.clamp(min, max);
                on_change(if integer { value.round() } else { value }, window, cx);
            }),
        }
    }
}
