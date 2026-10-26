//! The image view's stretch rows: the levels with the histogram beneath
//! them, and presets for how levels follow the frame. They head the view's
//! inspector page, which is also its right-click menu.

use std::cell::RefCell;
use std::sync::Arc;

use gpui::{
    AnyElement, App, Bounds, Context, Entity, FocusHandle, Focusable, IntoElement, Pixels, Render,
    SharedString, Subscription, WeakEntity, Window, canvas, div, point, prelude::*, px, size,
};

use super::histogram::Histogram;
use super::lut::Levels;
use super::view::ImageView;
use crate::inspector::rows::{AccessorySpec, CommandRow, InspectorRow, RowAction, value_row};
use crate::theme::{Theme, theme};

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
/// Its page shows the view's histogram.
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
            .get_or_insert_with(|| cx.new(|cx| HistogramStrip::new(&self.view, cx)))
            .clone();
        Some(AccessorySpec {
            view: strip.clone().into(),
            focus: strip.focus_handle(cx),
            dragging: Arc::new(|_| false),
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

/// Log-count bars of the view's histogram, the painted levels, and a marker
/// at the top code.
struct HistogramStrip {
    view: WeakEntity<ImageView>,
    focus: FocusHandle,
    _observe: Subscription,
}

impl HistogramStrip {
    fn new(view: &Entity<ImageView>, cx: &mut Context<Self>) -> Self {
        Self {
            view: view.downgrade(),
            focus: cx.focus_handle(),
            _observe: cx.observe(view, |_, _, cx| cx.notify()),
        }
    }
}

impl Focusable for HistogramStrip {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for HistogramStrip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let data = self
            .view
            .upgrade()
            .and_then(|view| view.read(cx).strip_data());
        let theme = theme(cx);
        let painter = canvas(
            |_, _, _| {},
            move |bounds, (), window, _| match &data {
                Some((histogram, levels, top)) => {
                    paint(bounds, histogram, *levels, *top, &theme, window)
                }
                None => window.paint_quad(gpui::fill(bounds, theme.bg_secondary)),
            },
        )
        .size_full();
        div().track_focus(&self.focus).size_full().child(painter)
    }
}

/// The x of `value` on a strip `width` wide spanning `range`.
fn x_of(value: f64, range: (f64, f64), width: f32) -> f32 {
    let span = range.1 - range.0;
    if span <= 0.0 || !span.is_finite() {
        return 0.0;
    }
    (((value - range.0) / span).clamp(0.0, 1.0) * f64::from(width)) as f32
}

/// `n` column heights in `0..=1`: the log of the summed bins under each.
fn columns(bins: &[u32], n: usize) -> Vec<f32> {
    if bins.is_empty() || n == 0 {
        return vec![0.0; n];
    }
    let sums: Vec<f64> = (0..n)
        .map(|c| {
            let a = c * bins.len() / n;
            let b = ((c + 1) * bins.len() / n).max(a + 1).min(bins.len());
            bins[a..b].iter().map(|&count| f64::from(count)).sum()
        })
        .collect();
    let top = sums.iter().copied().fold(0.0, f64::max).ln_1p();
    sums.iter()
        .map(|s| match top > 0.0 {
            true => (s.ln_1p() / top) as f32,
            false => 0.0,
        })
        .collect()
}

fn paint(
    bounds: Bounds<Pixels>,
    histogram: &Histogram,
    levels: Option<(f64, f64)>,
    top_code: Option<f64>,
    theme: &Theme,
    window: &mut Window,
) {
    const LINE_PX: f32 = 2.0;
    window.paint_quad(gpui::fill(bounds, theme.bg_secondary));
    let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
    let n = w.max(0.0) as usize;
    if n == 0 || h <= 0.0 {
        return;
    }
    let range = histogram.range();
    let origin = bounds.origin;
    let at = |x: f32, y: f32| point(origin.x + px(x), origin.y + px(y));
    let mut bars = gpui::PathBuilder::fill();
    bars.move_to(at(0.0, h));
    for (c, height) in columns(histogram.bins(), n).iter().enumerate() {
        let top = h - height * h;
        bars.line_to(at(c as f32, top));
        bars.line_to(at(c as f32 + 1.0, top));
    }
    bars.line_to(at(n as f32, h));
    bars.close();
    if let Ok(path) = bars.build() {
        window.paint_path(path, theme.text_tertiary);
    }
    let line = |x: f32, color, window: &mut Window| {
        let x = x.clamp(0.0, w - LINE_PX);
        window.paint_quad(gpui::fill(
            Bounds::new(at(x, 0.0), size(px(LINE_PX), px(h))),
            color,
        ));
    };
    if let Some(top) = top_code {
        line(x_of(top, range, w), theme.image_saturated(), window);
    }
    if let Some((black, white)) = levels {
        line(x_of(black, range, w), theme.text_secondary, window);
        line(x_of(white, range, w), theme.text_primary, window);
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
    fn columns_are_log_scaled_and_cover_every_bin() {
        let cols = columns(&[0, 1, 0, 1000], 2);
        assert_eq!(cols[1], 1.0);
        assert!(cols[0] > 0.05 && cols[0] < 0.2, "{cols:?}");
        // More columns than bins repeat bins rather than dropping them.
        assert_eq!(columns(&[5], 3), [1.0, 1.0, 1.0]);
        assert!(columns(&[], 4).iter().all(|&c| c == 0.0));
    }

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
