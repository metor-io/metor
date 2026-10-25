//! The image view's stretch rows: the levels with the histogram beneath
//! them, and presets for how levels follow the frame. They head the view's
//! inspector page, which is also its right-click menu.

use std::cell::RefCell;
use std::sync::Arc;

use gpui::{AnyElement, App, Entity, Focusable, SharedString, Window, prelude::*};

use super::lut::Levels;
use super::strip::HistogramStrip;
use super::view::ImageView;
use crate::inspector::rows::{AccessorySpec, CommandRow, InspectorRow, RowAction, value_row};

/// Stretch presets: a label and the levels it sets.
const PRESETS: [(&str, Levels); 3] = [
    (
        "Stretch: auto (0.5–99.95%)",
        Levels::Auto {
            low: 0.005,
            high: 0.9995,
        },
    ),
    (
        "Stretch: min–max",
        Levels::Auto {
            low: 0.0,
            high: 1.0,
        },
    ),
    (
        "Stretch: 1–99%",
        Levels::Auto {
            low: 0.01,
            high: 0.99,
        },
    ),
];

/// The levels row, carrying the histogram, then the presets.
pub fn stretch_rows(view: &Entity<ImageView>) -> Vec<Box<dyn InspectorRow>> {
    let mut rows: Vec<Box<dyn InspectorRow>> = vec![Box::new(LevelsRow {
        view: view.clone(),
        strip: RefCell::new(None),
    })];
    for (label, levels) in PRESETS {
        let view = view.clone();
        rows.push(Box::new(CommandRow::action(
            label,
            Arc::new(move |_, cx| {
                view.update(cx, |view, cx| view.set_stretch(levels, cx));
                // Stays open, so the histogram shows the new levels.
                RowAction::Handled
            }),
        )));
    }
    rows
}

/// The painted levels and how they are set; editing it types manual levels.
/// Its page shows the view's histogram, whose handles drag the levels.
struct LevelsRow {
    view: Entity<ImageView>,
    /// Made on first use: building rows has no `&mut App`.
    strip: RefCell<Option<Entity<HistogramStrip>>>,
}

impl InspectorRow for LevelsRow {
    fn label(&self) -> &str {
        "Levels"
    }

    fn accessory(&self, _: &str, cx: &mut App) -> Option<AccessorySpec> {
        let strip = self
            .strip
            .borrow_mut()
            .get_or_insert_with(|| ImageView::histogram_strip(&self.view, cx))
            .clone();
        let weak = strip.downgrade();
        Some(AccessorySpec {
            view: strip.clone().into(),
            focus: strip.focus_handle(cx),
            dragging: Arc::new(move |cx| weak.upgrade().is_some_and(|s| s.read(cx).is_dragging())),
            expanded: true,
        })
    }

    fn render_row(
        &self,
        row_ix: usize,
        selected: bool,
        _: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let view = self.view.read(cx);
        let summary = summary(view.display.levels, view.levels());
        value_row(row_ix, selected, "Levels".into(), summary, false, cx).into_any_element()
    }

    fn activate(&mut self, _: &mut Window, cx: &mut App) -> RowAction {
        let current = self.view.read(cx).levels();
        let view = self.view.clone();
        RowAction::StartEdit {
            current_text: current
                .map(|(black, white)| format!("{}, {}", number(black), number(white)))
                .unwrap_or_default(),
            on_commit: Box::new(move |text, _, cx| {
                if let Some(levels) = parse_levels(&text) {
                    view.update(cx, |view, cx| view.set_levels(levels, cx));
                }
            }),
        }
    }
}

/// How the levels are set, and the painted black and white.
fn summary(levels: Levels, painted: Option<(f64, f64)>) -> SharedString {
    let mode = match levels {
        Levels::Manual { .. } => "manual".to_string(),
        Levels::Auto { low, high } => {
            format!("auto {}–{}%", number(low * 100.0), number(high * 100.0))
        }
    };
    match painted {
        Some((black, white)) => format!("{mode} · {}–{}", number(black), number(white)).into(),
        None => mode.into(),
    }
}

/// `value` with at most four decimals and no trailing zeros.
fn number(value: f64) -> String {
    let text = format!("{value:.4}");
    let text = text.trim_end_matches('0').trim_end_matches('.');
    match text {
        "-0" => "0".to_string(),
        text => text.to_string(),
    }
}

/// Black and white from `"black, white"` or `"black white"`, white above
/// black.
fn parse_levels(text: &str) -> Option<(f64, f64)> {
    let mut parts = text
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|part| !part.is_empty())
        .map(str::parse::<f64>);
    let (black, white) = (parts.next()?.ok()?, parts.next()?.ok()?);
    (parts.next().is_none() && black.is_finite() && white.is_finite() && white > black)
        .then_some((black, white))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_parse_from_two_numbers() {
        assert_eq!(parse_levels("10, 400"), Some((10.0, 400.0)));
        assert_eq!(parse_levels(" 0.5 2.25 "), Some((0.5, 2.25)));
        assert_eq!(parse_levels("400, 10"), None, "white above black");
        assert_eq!(parse_levels("10"), None);
        assert_eq!(parse_levels("1, 2, 3"), None);
        assert_eq!(parse_levels("a, 2"), None);
    }

    #[test]
    fn the_summary_names_the_mode_and_the_levels() {
        assert_eq!(
            summary(Levels::default(), Some((12.0, 3400.0))).as_ref(),
            "auto 0.5–99.95% · 12–3400"
        );
        assert_eq!(
            summary(
                Levels::Manual {
                    black: 0.25,
                    white: 1.0
                },
                Some((0.25, 1.0))
            )
            .as_ref(),
            "manual · 0.25–1"
        );
    }
}
