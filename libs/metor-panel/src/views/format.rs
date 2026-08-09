use std::fmt::Write;

use gpui::SharedString;
use metor_db::DB;
use metor_proto::types::{ComponentId, ComponentView, ElementValue};
use smallvec::SmallVec;

/// Packed element indices into a multi-dimensional component.
///
/// Inline capacity covers a Vec3 (3) or small matrix (9) without spilling
/// to the heap; larger components allocate.
pub type ElementIndexes = SmallVec<[usize; 8]>;

/// Display payload for one element of a component.
#[derive(Clone, Debug, PartialEq)]
pub struct StripCell {
    pub label: Option<SharedString>,
    pub value: SharedString,
}

/// Render a component value respecting its metadata.
///
/// - String components (U8 marked `is_string`) decode as UTF-8 up to the
///   first NUL.
/// - Enum components map the integer value to a variant name.
/// - Everything else falls back to 4-decimal numeric formatting.
pub fn format_value(view: ComponentView<'_>, db: &DB, component_id: ComponentId) -> String {
    ValueFormatter::resolve(db, component_id).format(view)
}

/// String and enum rules resolved once per subscription.
#[derive(Clone, Default)]
pub(crate) struct ValueFormatter {
    is_string: bool,
    enum_variants: Option<Vec<String>>,
}

impl ValueFormatter {
    pub(crate) fn resolve(db: &DB, component_id: ComponentId) -> Self {
        db.with_state(|state| {
            let meta = state.get_component_metadata(component_id);
            Self {
                is_string: meta.map(|meta| meta.is_string()).unwrap_or(false),
                enum_variants: meta.and_then(|meta| {
                    meta.enum_variants()
                        .map(|variants| variants.map(str::to_string).collect())
                }),
            }
        })
    }

    pub(crate) fn new(is_string: bool, enum_variants: Option<Vec<String>>) -> Self {
        Self {
            is_string,
            enum_variants,
        }
    }

    fn special_value(&self, view: &ComponentView<'_>) -> Option<String> {
        if self.is_string
            && let ComponentView::U8(array) = view
        {
            let buf = array.buf();
            let len = buf.iter().position(|&byte| byte == 0).unwrap_or(buf.len());
            if let Ok(value) = std::str::from_utf8(&buf[..len]) {
                return Some(value.to_string());
            }
        }
        self.enum_variants
            .as_ref()?
            .get(view.to_f64() as usize)
            .cloned()
    }

    pub(crate) fn format(&self, view: ComponentView<'_>) -> String {
        if let Some(value) = self.special_value(&view) {
            return value;
        }
        let mut value = String::new();
        let _ = write!(value, "{view:.4}");
        value
    }

    pub(crate) fn format_cells(
        &self,
        view: &ComponentView<'_>,
        element_names: &[SharedString],
    ) -> Vec<StripCell> {
        if let Some(value) = self.special_value(view) {
            return vec![StripCell {
                label: None,
                value: SharedString::from(value),
            }];
        }

        let values: Vec<ElementValue> = view.iter().collect();
        if values.len() == 1 {
            return vec![StripCell {
                label: None,
                value: SharedString::from(format_element(values[0])),
            }];
        }
        values
            .into_iter()
            .enumerate()
            .map(|(index, value)| StripCell {
                label: element_names.get(index).cloned(),
                value: SharedString::from(format_element(value)),
            })
            .collect()
    }
}

/// Render one element, substituting a variant name when the component is an enum.
pub fn format_element_value(value: ElementValue, enum_variants: Option<&[&str]>) -> String {
    if let Some(variants) = enum_variants {
        let idx = value.as_usize();
        if let Some(name) = variants.get(idx) {
            return name.to_string();
        }
    }
    format_element(value)
}

pub(crate) fn format_element(value: ElementValue) -> String {
    match value {
        ElementValue::Bool(value) => (if value { "true" } else { "false" }).to_string(),
        ElementValue::F32(value) => format_number(value as f64),
        ElementValue::F64(value) => format_number(value),
        other => pad_positive(&other.as_f64().to_string()),
    }
}

/// Adaptive-precision number formatter. Non-negative values get a leading
/// space so digits stay column-aligned across a sign change.
pub(crate) fn format_number(v: f64) -> String {
    let body = if v == 0.0 {
        "0".to_string()
    } else if v.abs() >= 1000.0 {
        format!("{:.0}", v)
    } else if v.abs() >= 100.0 {
        format!("{:.1}", v)
    } else if v.abs() >= 1.0 {
        format!("{:.2}", v)
    } else {
        format!("{:.4}", v)
    };
    pad_positive(&body)
}

/// Pass-through for already-signed strings and non-finite floats.
pub(crate) fn pad_positive(s: &str) -> String {
    if s.starts_with('-') {
        s.to_string()
    } else if s
        .chars()
        .next()
        .map(|c| c.is_ascii_digit())
        .unwrap_or(false)
    {
        format!(" {s}")
    } else {
        s.to_string()
    }
}
