use std::fmt::Write;

use metor_db::DB;
use metor_proto::types::{ComponentId, ComponentView, ElementValue};
use smallvec::SmallVec;

/// Element indices within a component (e.g. x=0, y=1, z=2 for a Vec3).
pub type ElementIndexes = SmallVec<[usize; 8]>;

/// Format a component value using metadata hints.
///
/// - `is_string`: interprets U8 bytes as UTF-8
/// - `enum_variants`: maps integer value to variant name
/// - Otherwise: numeric display with 4 decimal places
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

/// Format a single element value, respecting enum variant metadata.
pub fn format_element_value(value: ElementValue, enum_variants: Option<&[&str]>) -> String {
    if let Some(variants) = enum_variants {
        let idx = value.as_usize();
        if let Some(name) = variants.get(idx) {
            return name.to_string();
        }
    }
    super::value_strip::format_element(value)
}

/// Format a number with adaptive precision: drops unnecessary trailing
/// zeros for large magnitudes and keeps more digits for small ones.
pub(crate) fn format_number(v: f64) -> String {
    if v == 0.0 {
        "0".to_string()
    } else if v.abs() >= 1000.0 {
        format!("{:.0}", v)
    } else if v.abs() >= 100.0 {
        format!("{:.1}", v)
    } else if v.abs() >= 1.0 {
        format!("{:.2}", v)
    } else {
        format!("{:.4}", v)
    }
}
