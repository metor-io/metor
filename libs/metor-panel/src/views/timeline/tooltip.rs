//! Immediate, mutually exclusive timestamp and event readouts, like the plot.
use super::*;
use gpui::{SharedString, point};

/// The timestamp and flag card are mutually exclusive for each hover position.
pub(super) enum Readout {
    Time(String),
    Events(Vec<Arc<TimelineEvent>>, usize),
}

impl Timeline {
    pub(super) fn clear_hover(&mut self, cx: &mut Context<Self>) {
        if self.hover.take().is_some() {
            cx.notify();
        }
    }
    pub(super) fn readout(&self, cx: &App) -> Option<Readout> {
        let pointer = self
            .hover
            .filter(|p| self.time_area().is_some_and(|a| a.contains(p)))?;
        if self.drag.is_some() {
            return None;
        }
        if let Some((events, count)) = self.hit_events(pointer, 8) {
            return Some(Readout::Events(events, count));
        }
        let time = self.viewport?.at(self.fraction(pointer.x));
        Some(Readout::Time(self.preview_error.clone().unwrap_or_else(
            || temporal::display::label(Timestamp(time), cx),
        )))
    }
    pub(super) fn show_readout(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(readout) = self.readout(cx) else {
            return;
        };
        let pointer = self.hover.unwrap();
        let target = cx.entity().downgrade();
        // Register immediately in prepaint, bypassing the native hover delay.
        // The tooltip layer escapes thin previews and works inside an inspector's
        // deferred surface, where nesting another deferred draw would panic.
        let view = cx.new(|_| TimelineTooltip { readout });
        window.set_tooltip(gpui::AnyTooltip {
            view: view.into(),
            mouse_position: pointer + point(px(11.0), px(11.0)),
            check_visible_and_update: Rc::new(move |_, _, cx| {
                target.upgrade().is_some_and(|t| {
                    let t = t.read(cx);
                    t.hover == Some(pointer) && t.drag.is_none()
                })
            }),
        });
    }
}

/// A frame's readout snapshot; no timer or subscription is needed.
struct TimelineTooltip {
    readout: Readout,
}
impl Render for TimelineTooltip {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let card = match &self.readout {
            Readout::Time(label) => crate::views::popover::readout_card(cx).child(label.clone()),
            Readout::Events(events, count) => {
                let first = &events[0].event;
                let header: SharedString = if *count > 1 {
                    format!("{} (and {} more)", first.label, count - 1).into()
                } else {
                    first.label.clone()
                };
                crate::plot_events::popover::event_card(header, events.iter().map(|e| &e.event), cx)
            }
        };
        card.max_w(px(480.0).min((window.viewport_size().width - px(8.0)).max(px(0.0))))
    }
}
