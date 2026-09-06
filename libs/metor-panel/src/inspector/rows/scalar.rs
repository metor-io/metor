use std::{cell::RefCell, rc::Rc, sync::Arc};

use gpui::{AnyElement, App, SharedString, Window, div, prelude::*, px};
use metor_proto::types::PrimType;

use super::{InspectorRow, RowAction, row_base};
use crate::dynamic::tensor::TypedScalar;
use crate::theme::theme;

/// Numeric editor that preserves the field's type and rejects out-of-range input.
pub struct ScalarRow {
    pub label: SharedString,
    pub value: Rc<RefCell<TypedScalar>>,
    pub on_change: Arc<dyn Fn(TypedScalar, &mut Window, &mut App)>,
}

impl ScalarRow {
    pub fn new(
        label: SharedString,
        value: f64,
        on_change: Arc<dyn Fn(f64, &mut Window, &mut App)>,
    ) -> Self {
        Self::typed(
            label,
            TypedScalar::F64(value),
            Arc::new(move |v, w, cx| {
                on_change(v.as_f64(), w, cx);
            }),
        )
    }

    pub fn typed(
        label: SharedString,
        value: TypedScalar,
        on_change: Arc<dyn Fn(TypedScalar, &mut Window, &mut App)>,
    ) -> Self {
        Self {
            label,
            value: Rc::new(RefCell::new(value)),
            on_change,
        }
    }

    fn format_value(&self) -> String {
        format_scalar(*self.value.borrow())
    }
}

fn format_scalar(value: TypedScalar) -> String {
    match value {
        TypedScalar::U8(v) => v.to_string(),
        TypedScalar::U16(v) => v.to_string(),
        TypedScalar::U32(v) => v.to_string(),
        TypedScalar::U64(v) => v.to_string(),
        TypedScalar::I8(v) => v.to_string(),
        TypedScalar::I16(v) => v.to_string(),
        TypedScalar::I32(v) => v.to_string(),
        TypedScalar::I64(v) => v.to_string(),
        TypedScalar::F32(v) => v.to_string(),
        TypedScalar::F64(v) => v.to_string(),
        TypedScalar::Bool(v) => v.to_string(),
    }
}

fn parse_for_dtype(text: &str, dtype: PrimType) -> Option<TypedScalar> {
    let text = text.trim();
    Some(match dtype {
        PrimType::U8 => TypedScalar::U8(text.parse().ok()?),
        PrimType::U16 => TypedScalar::U16(text.parse().ok()?),
        PrimType::U32 => TypedScalar::U32(text.parse().ok()?),
        PrimType::U64 => TypedScalar::U64(text.parse().ok()?),
        PrimType::I8 => TypedScalar::I8(text.parse().ok()?),
        PrimType::I16 => TypedScalar::I16(text.parse().ok()?),
        PrimType::I32 => TypedScalar::I32(text.parse().ok()?),
        PrimType::I64 => TypedScalar::I64(text.parse().ok()?),
        PrimType::F32 => TypedScalar::F32(text.parse().ok()?),
        PrimType::F64 => TypedScalar::F64(text.parse().ok()?),
        PrimType::Bool => TypedScalar::Bool(match text {
            "true" | "1" => true,
            "false" | "0" => false,
            _ => return None,
        }),
    })
}

impl InspectorRow for ScalarRow {
    fn supports_exit_fade(&self) -> bool {
        true
    }

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
        let dtype = self.value.borrow().dtype();
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
    fn integers_round_trip_without_float_conversion() {
        for v in [
            TypedScalar::U64(u64::MAX),
            TypedScalar::I64(i64::MIN),
            TypedScalar::U64(9_007_199_254_740_993),
            TypedScalar::I8(-128),
        ] {
            assert_eq!(parse_for_dtype(&format_scalar(v), v.dtype()), Some(v));
        }
    }

    #[test]
    fn integers_reject_fractional_and_out_of_range_input() {
        for text in ["1.5", "NaN", "inf", "-1", "256"] {
            assert_eq!(parse_for_dtype(text, PrimType::U8), None);
        }
        assert_eq!(parse_for_dtype("18446744073709551616", PrimType::U64), None);
        assert_eq!(parse_for_dtype("128", PrimType::I8), None);
    }

    #[test]
    fn floats_and_bools_keep_their_input_forms() {
        assert_eq!(
            parse_for_dtype("1.5", PrimType::F32),
            Some(TypedScalar::F32(1.5))
        );
        assert_eq!(
            parse_for_dtype("true", PrimType::Bool),
            Some(TypedScalar::Bool(true))
        );
        assert_eq!(
            parse_for_dtype("0", PrimType::Bool),
            Some(TypedScalar::Bool(false))
        );
    }
}
