use std::{cell::RefCell, rc::Rc, sync::Arc};

use gpui::{AnyElement, App, SharedString, Window, prelude::*};

use super::{InspectorRow, RowAction, value_row};
use crate::dynamic::tensor::TypedScalar;

/// Numeric editor that preserves the field's type and rejects out-of-range input.
pub struct ScalarRow {
    pub label: SharedString,
    pub value: Rc<RefCell<TypedScalar>>,
    source: Option<Arc<dyn Fn(&App) -> Option<TypedScalar>>>,
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
            source: None,
            on_change,
        }
    }

    /// Read the value through `source` on every render, falling back to the
    /// cached copy when it answers `None`. A docked inspector outlives the
    /// edit that built it, so a row there has to track changes made
    /// elsewhere — by the popup, or by the view itself.
    pub fn live(mut self, source: Arc<dyn Fn(&App) -> Option<TypedScalar>>) -> Self {
        self.source = Some(source);
        self
    }

    fn current(&self, cx: &App) -> TypedScalar {
        self.source
            .as_ref()
            .and_then(|source| source(cx))
            .unwrap_or(*self.value.borrow())
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
        value_row(
            row_ix,
            selected,
            self.label.clone(),
            format_scalar(self.current(cx)).into(),
            false,
            cx,
        )
        .into_any_element()
    }

    fn activate(&mut self, _window: &mut Window, cx: &mut App) -> RowAction {
        let on_change = self.on_change.clone();
        let cached = self.value.clone();
        let current = self.current(cx);
        let dtype = current.dtype();
        RowAction::StartEdit {
            current_text: format_scalar(current),
            on_commit: Box::new(move |text, window, cx| {
                if let Some(v) = TypedScalar::parse(&text, dtype) {
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
    use metor_proto::types::PrimType;

    #[test]
    fn integers_round_trip_without_float_conversion() {
        for v in [
            TypedScalar::U64(u64::MAX),
            TypedScalar::I64(i64::MIN),
            TypedScalar::U64(9_007_199_254_740_993),
            TypedScalar::I8(-128),
        ] {
            assert_eq!(TypedScalar::parse(&format_scalar(v), v.dtype()), Some(v));
        }
    }

    #[test]
    fn integers_reject_fractional_and_out_of_range_input() {
        for text in ["1.5", "NaN", "inf", "-1", "256"] {
            assert_eq!(TypedScalar::parse(text, PrimType::U8), None);
        }
        assert_eq!(
            TypedScalar::parse("18446744073709551616", PrimType::U64),
            None
        );
        assert_eq!(TypedScalar::parse("128", PrimType::I8), None);
    }

    #[test]
    fn floats_and_bools_keep_their_input_forms() {
        assert_eq!(
            TypedScalar::parse("1.5", PrimType::F32),
            Some(TypedScalar::F32(1.5))
        );
        assert_eq!(
            TypedScalar::parse("true", PrimType::Bool),
            Some(TypedScalar::Bool(true))
        );
        assert_eq!(
            TypedScalar::parse("0", PrimType::Bool),
            Some(TypedScalar::Bool(false))
        );
    }
}
