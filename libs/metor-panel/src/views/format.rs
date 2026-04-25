use std::fmt::Write;

use metor_db::DB;
use metor_proto::types::{ComponentId, ComponentView, ElementValue};
use smallvec::SmallVec;

/// Packed element indices into a multi-dimensional component.
///
/// Inline capacity covers a Vec3 (3) or small matrix (9) without spilling
/// to the heap; larger components allocate.
pub type ElementIndexes = SmallVec<[usize; 8]>;

/// Render a component value respecting its metadata.
///
/// - String components (U8 marked `is_string`) decode as UTF-8 up to the
///   first NUL.
/// - Enum components map the integer value to a variant name.
/// - Everything else falls back to 4-decimal numeric formatting.
pub fn format_value(view: ComponentView<'_>, db: &DB, component_id: ComponentId) -> String {
    let meta = db.with_state(|s| s.get_component_metadata(component_id).cloned());
    if let Some(meta) = &meta {
        if meta.is_string() {
            if let ComponentView::U8(array) = &view {
                let buf = array.buf();
                let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
                if let Ok(s) = std::str::from_utf8(&buf[..len]) {
                    return s.to_string();
                }
            }
        }
        if let Some(variants) = meta.enum_variants() {
            let variants: Vec<&str> = variants.collect();
            let idx = view.to_f64() as usize;
            if let Some(name) = variants.get(idx) {
                return name.to_string();
            }
        }
    }
    let mut s = String::new();
    let _ = write!(s, "{:.4}", view);
    s
}

/// Render one element, substituting a variant name when the component is an enum.
pub fn format_element_value(value: ElementValue, enum_variants: Option<&[&str]>) -> String {
    if let Some(variants) = enum_variants {
        let idx = value.as_usize();
        if let Some(name) = variants.get(idx) {
            return name.to_string();
        }
    }
    super::value_strip::format_element(value)
}

/// Adaptive-precision number formatter.
///
/// Keeps large magnitudes compact (no trailing decimals above 1000) and
/// preserves resolution below unity (4 fractional digits under 1.0).
/// Non-negative values are prefixed with a space so that a value
/// oscillating across zero doesn't flicker its digits one column to the
/// right when the sign appears.
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

/// Prefix a digit-width space to non-negative numerics so the digit
/// columns stay put when a value crosses zero. Pass-through for already-
/// signed strings and non-finite floats (`NaN`, `inf`).
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
