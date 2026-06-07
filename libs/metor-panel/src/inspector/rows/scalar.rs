use std::{cell::RefCell, rc::Rc, sync::Arc};

use gpui::{AnyElement, App, SharedString, Window, div, prelude::*, px};
use metor_proto::types::PrimType;

use super::{InspectorRow, RowAction, row_base};
use crate::dynamic::tensor::TypedScalar;
use crate::theme::theme;

/// Inspector row for a numeric field.
///
/// Shows the current value as text; activation starts inline editing.
/// Unparsable input is silently dropped rather than raising an error.
///
/// The current value is cached in an `Rc<RefCell<f64>>` so that on commit
/// the row redraws with the new value immediately, instead of waiting for
/// the underlying entity to refresh.
///
/// When constructed via [`ScalarRow::typed`], a `dtype` is stashed and
/// used to format the displayed text (no decimals for integer dtypes) and to
/// reject inputs that don't fit the dtype's lexical shape (e.g. a `1.5`
/// typed into an `i32` field).
pub struct ScalarRow {
    pub label: SharedString,
    pub value: Rc<RefCell<f64>>,
    pub on_change: Arc<dyn Fn(f64, &mut Window, &mut App)>,
    /// When `Some`, the row formats and parses according to this dtype.
    /// `None` keeps the historic float-only behavior.
    dtype: Option<PrimType>,
}

impl ScalarRow {
    pub fn new(
        label: SharedString,
        value: f64,
        on_change: Arc<dyn Fn(f64, &mut Window, &mut App)>,
    ) -> Self {
        Self {
            label,
            value: Rc::new(RefCell::new(value)),
            on_change,
            dtype: None,
        }
    }

    /// Dtype-aware constructor. The displayed value is formatted to the
    /// dtype, and on commit the parsed `f64` is cast back through
    /// `TypedScalar::from_f64(v, dtype)` before reaching `on_change`.
    pub fn typed(
        label: SharedString,
        value: TypedScalar,
        on_change: Arc<dyn Fn(TypedScalar, &mut Window, &mut App)>,
    ) -> Self {
        let dtype = value.dtype();
        let on_change = Arc::new(move |v: f64, w: &mut Window, cx: &mut App| {
            on_change(TypedScalar::from_f64(v, dtype), w, cx);
        });
        Self {
            label,
            value: Rc::new(RefCell::new(value.as_f64())),
            on_change,
            dtype: Some(dtype),
        }
    }

    fn format_value(&self) -> String {
        format_f64_for_dtype(*self.value.borrow(), self.dtype)
    }
}

fn format_f64_for_dtype(v: f64, dtype: Option<PrimType>) -> String {
    match dtype {
        None | Some(PrimType::F32) | Some(PrimType::F64) => format!("{v}"),
        Some(PrimType::Bool) => {
            if v != 0.0 {
                "true".into()
            } else {
                "false".into()
            }
        }
        Some(_) => format!("{}", v as i64),
    }
}

/// Parse text per dtype. Floats accept any `f64` literal; integer dtypes
/// require an integer-shaped string (no decimal point). Bool accepts
/// `true`/`false`/`0`/`1`.
fn parse_for_dtype(text: &str, dtype: Option<PrimType>) -> Option<f64> {
    let trimmed = text.trim();
    match dtype {
        None | Some(PrimType::F32) | Some(PrimType::F64) => trimmed.parse::<f64>().ok(),
        Some(PrimType::Bool) => match trimmed {
            "true" | "1" => Some(1.0),
            "false" | "0" => Some(0.0),
            _ => None,
        },
        Some(_) => trimmed.parse::<i64>().ok().map(|n| n as f64),
    }
}

impl InspectorRow for ScalarRow {
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
        let value_text = SharedString::from(self.format_value());

        row_base(row_ix, selected, cx)
            .gap(px(2.))
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme.text_primary)
                    .child(self.label.clone()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(12.0))
                    .text_color(theme.text_secondary)
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_right()
                    .child(value_text),
            )
            .into_any_element()
    }

    fn activate(&mut self, _window: &mut Window, _cx: &mut App) -> RowAction {
        let on_change = self.on_change.clone();
        let cached = self.value.clone();
        let dtype = self.dtype;
        RowAction::StartEdit {
            current_text: self.format_value(),
            on_commit: Box::new(move |text, window, cx| {
                if let Some(v) = parse_for_dtype(&text, dtype) {
                    *cached.borrow_mut() = v;
                    on_change(v, window, cx);
                }
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_int_drops_decimals() {
        assert_eq!(format_f64_for_dtype(2.5, Some(PrimType::I32)), "2");
        assert_eq!(format_f64_for_dtype(-7.9, Some(PrimType::I32)), "-7");
        assert_eq!(format_f64_for_dtype(255.0, Some(PrimType::U8)), "255");
    }

    #[test]
    fn format_bool_uses_words() {
        assert_eq!(format_f64_for_dtype(0.0, Some(PrimType::Bool)), "false");
        assert_eq!(format_f64_for_dtype(1.0, Some(PrimType::Bool)), "true");
    }

    #[test]
    fn format_float_round_trips() {
        assert_eq!(format_f64_for_dtype(1.5, Some(PrimType::F64)), "1.5");
        assert_eq!(format_f64_for_dtype(1.5, None), "1.5");
    }

    #[test]
    fn parse_int_rejects_decimal() {
        assert!(parse_for_dtype("1.5", Some(PrimType::I32)).is_none());
        assert_eq!(parse_for_dtype("42", Some(PrimType::I32)), Some(42.0));
    }

    #[test]
    fn parse_bool_accepts_word_or_number() {
        assert_eq!(parse_for_dtype("true", Some(PrimType::Bool)), Some(1.0));
        assert_eq!(parse_for_dtype("0", Some(PrimType::Bool)), Some(0.0));
        assert!(parse_for_dtype("maybe", Some(PrimType::Bool)).is_none());
    }
}
