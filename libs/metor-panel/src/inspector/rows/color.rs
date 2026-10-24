use std::sync::Arc;

use gpui::{AnyElement, App, Hsla, MouseButton, SharedString, Window, div, prelude::*, px};
use smallvec::SmallVec;

use super::slider::SliderRow;
use super::{InspectorRow, RowAction, row_base, row_label, row_value};
use crate::icons::Icon;
use crate::theme::{Theme, theme};

/// Row for colour fields.
///
/// Shows the colour's swatch and cascades into a page whose first row is a
/// strip of presets, then four slider rows (H, S, L, A) for anything the
/// presets don't cover. With [`AutoColor`], the field can also follow the
/// theme: the summary says "Auto" and the strip leads with an Auto swatch.
pub struct ColorRow {
    pub label: SharedString,
    pub read_color: Arc<dyn Fn(&App) -> Hsla>,
    pub on_change: Arc<dyn Fn(Hsla, &mut Window, &mut App)>,
    pub auto: Option<AutoColor>,
}

/// The "follow the theme" state of an optional colour field.
#[derive(Clone)]
pub struct AutoColor {
    pub is_auto: Arc<dyn Fn(&App) -> bool>,
    pub reset: Arc<dyn Fn(&mut Window, &mut App)>,
}

/// The four sliders' labels and the channel each one edits.
const CHANNELS: [(&str, fn(&mut Hsla) -> &mut f32); 4] = [
    ("Hue", |c| &mut c.h),
    ("Saturation", |c| &mut c.s),
    ("Lightness", |c| &mut c.l),
    ("Alpha", |c| &mut c.a),
];

const SWATCH: f32 = 16.0;

impl InspectorRow for ColorRow {
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
        let theme = theme(cx);
        let auto = self.auto.as_ref().is_some_and(|a| (a.is_auto)(cx));
        let summary = if auto {
            row_value("Auto", theme.text_secondary).into_any_element()
        } else {
            div()
                .w(px(14.0))
                .h(px(14.0))
                .rounded(px(3.0))
                .bg((self.read_color)(cx))
                .into_any_element()
        };

        row_base(row_ix, selected, cx)
            .child(row_label(self.label.clone(), theme.text_primary))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .child(summary)
                    .child(Icon::ChevronRight.svg(8.0, theme.text_secondary)),
            )
            .into_any_element()
    }

    fn activate(&mut self, _window: &mut Window, _cx: &mut App) -> RowAction {
        let mut rows: Vec<Box<dyn InspectorRow>> = vec![Box::new(SwatchRow {
            read_color: self.read_color.clone(),
            on_change: self.on_change.clone(),
            auto: self.auto.clone(),
        })];
        rows.extend(CHANNELS.into_iter().map(|(label, channel)| {
            let read = self.read_color.clone();
            let write = self.read_color.clone();
            let on_change = self.on_change.clone();
            Box::new(SliderRow {
                label: label.into(),
                read_value: Arc::new(move |cx| *channel(&mut read(cx)) as f64),
                min: 0.0,
                max: 1.0,
                on_change: Arc::new(move |v, w, cx| {
                    let mut color = write(cx);
                    *channel(&mut color) = v as f32;
                    on_change(color, w, cx);
                }),
            }) as Box<dyn InspectorRow>
        }));
        RowAction::Cascade(rows)
    }
}

/// One choice in the preset strip.
#[derive(Clone, Copy, PartialEq)]
enum Swatch {
    Auto,
    Color(Hsla),
}

/// The theme's plot palette, then a neutral light and grey: the colours
/// that already mean something in the panel, so a picked preset matches
/// the traces and text around it.
fn presets(theme: &Theme) -> SmallVec<[Hsla; 10]> {
    let mut colors: SmallVec<[Hsla; 10]> = theme.line_colors.iter().copied().collect();
    colors.push(theme.text_primary);
    colors.push(theme.text_tertiary);
    colors
}

/// Whether two colours are the same preset, allowing for the float drift a
/// round trip through a saved layout picks up.
fn same_color(a: Hsla, b: Hsla) -> bool {
    [a.h - b.h, a.s - b.s, a.l - b.l, a.a - b.a]
        .iter()
        .all(|d| d.abs() < 1e-3)
}

/// The preset strip: one row of swatches, clicked to apply. Enter cycles
/// through them, so the keyboard reaches every preset too.
struct SwatchRow {
    read_color: Arc<dyn Fn(&App) -> Hsla>,
    on_change: Arc<dyn Fn(Hsla, &mut Window, &mut App)>,
    auto: Option<AutoColor>,
}

impl SwatchRow {
    fn swatches(&self, cx: &App) -> SmallVec<[Swatch; 11]> {
        let mut out = SmallVec::new();
        if self.auto.is_some() {
            out.push(Swatch::Auto);
        }
        out.extend(presets(&theme(cx)).into_iter().map(Swatch::Color));
        out
    }

    fn current(&self, cx: &App) -> Option<Swatch> {
        if self.auto.as_ref().is_some_and(|a| (a.is_auto)(cx)) {
            return Some(Swatch::Auto);
        }
        let color = (self.read_color)(cx);
        self.swatches(cx)
            .into_iter()
            .find(|s| matches!(s, Swatch::Color(c) if same_color(*c, color)))
    }

    fn apply(&self, swatch: Swatch, window: &mut Window, cx: &mut App) {
        match (swatch, &self.auto) {
            (Swatch::Auto, Some(auto)) => (auto.reset)(window, cx),
            (Swatch::Color(color), _) => (self.on_change)(color, window, cx),
            (Swatch::Auto, None) => {}
        }
    }
}

impl InspectorRow for SwatchRow {
    fn label(&self) -> &str {
        "Presets"
    }

    fn render_row(
        &self,
        row_ix: usize,
        selected: bool,
        _window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let theme = theme(cx);
        let interactive = !super::passive(cx);
        let current = self.current(cx);
        let mut strip = div().flex().flex_row().items_center().gap(px(4.0));
        for (i, swatch) in self.swatches(cx).into_iter().enumerate() {
            let chosen = current == Some(swatch);
            let cell = div()
                .id(("color-swatch", i))
                .size(px(SWATCH))
                .flex_none()
                .rounded(px(3.0))
                .border_1()
                .border_color(if chosen {
                    theme.text_primary
                } else {
                    Hsla::transparent_black()
                });
            let cell = match swatch {
                Swatch::Color(color) => cell.bg(color),
                Swatch::Auto => cell
                    .border_color(if chosen {
                        theme.text_primary
                    } else {
                        theme.line_color
                    })
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(9.0))
                    .text_color(theme.text_secondary)
                    .child("A"),
            };
            let cell = if interactive {
                let read_color = self.read_color.clone();
                let on_change = self.on_change.clone();
                let auto = self.auto.clone();
                cell.cursor_pointer()
                    .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                        // The row's own click would cycle; a swatch picks.
                        cx.stop_propagation();
                        let row = SwatchRow {
                            read_color: read_color.clone(),
                            on_change: on_change.clone(),
                            auto: auto.clone(),
                        };
                        row.apply(swatch, window, cx);
                        window.refresh();
                    })
            } else {
                cell
            };
            strip = strip.child(cell);
        }
        row_base(row_ix, selected, cx)
            .child(strip)
            .into_any_element()
    }

    fn activate(&mut self, window: &mut Window, cx: &mut App) -> RowAction {
        let swatches = self.swatches(cx);
        let next = match self.current(cx) {
            Some(now) => swatches
                .iter()
                .position(|s| *s == now)
                .map_or(0, |i| (i + 1) % swatches.len()),
            None => 0,
        };
        if let Some(&swatch) = swatches.get(next) {
            self.apply(swatch, window, cx);
        }
        RowAction::Handled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_preset_survives_float_drift() {
        let a = gpui::hsla(0.5, 0.6, 0.7, 1.0);
        let drifted = gpui::hsla(0.500_01, 0.6, 0.699_99, 1.0);
        assert!(same_color(a, drifted));
        assert!(!same_color(a, gpui::hsla(0.52, 0.6, 0.7, 1.0)));
    }
}
