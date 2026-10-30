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
        // Integers print exactly: a counter or an id is read digit by digit.
        ElementValue::U64(value) => pad_positive(&group_digits(&value.to_string())),
        ElementValue::I64(value) => pad_positive(&group_digits(&value.to_string())),
        other => pad_positive(&group_digits(&(other.as_f64() as i64).to_string())),
    }
}

/// [`format_exact`] with a leading space on non-negative values, so digits
/// stay column-aligned across a sign change.
pub(crate) fn format_number(v: f64) -> String {
    pad_positive(&format_exact(v))
}

/// One value as a readout shows it, where the number itself is what is
/// read: adaptive precision, digits grouped in threes once there are five
/// or more, and an exponent past what grouping keeps legible.
pub(crate) fn format_exact(v: f64) -> String {
    let abs = v.abs();
    if v == 0.0 || !v.is_finite() {
        v.to_string()
    } else if !(1e-4..1e15).contains(&abs) {
        short_exponent(&format!("{v:.5e}"))
    } else if abs >= 1000.0 {
        group_digits(&format!("{v:.0}"))
    } else if abs >= 100.0 {
        format!("{v:.1}")
    } else if abs >= 1.0 {
        format!("{v:.2}")
    } else {
        format!("{v:.4}")
    }
}

/// `1.23400e20` as `1.234e20`.
fn short_exponent(s: &str) -> String {
    match s.split_once('e') {
        Some((mantissa, exponent)) if mantissa.contains('.') => {
            let mantissa = mantissa.trim_end_matches('0').trim_end_matches('.');
            format!("{mantissa}e{exponent}")
        }
        _ => s.to_string(),
    }
}

/// Split a number's integer digits into threes with narrow spaces, once
/// there are five or more of them (`204 512 734`, but `1234`).
fn group_digits(s: &str) -> String {
    let (sign, rest) = s.split_at(usize::from(s.starts_with('-')));
    let (int, frac) = rest.split_at(rest.find('.').unwrap_or(rest.len()));
    if int.len() < 5 || !int.bytes().all(|b| b.is_ascii_digit()) {
        return s.to_string();
    }
    let mut out = String::from(sign);
    for (i, digit) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            out.push('\u{202F}');
        }
        out.push(digit);
    }
    out.push_str(frac);
    out
}

/// The tick labels of one value axis.
#[derive(Debug, PartialEq)]
pub(crate) struct AxisLabels {
    /// The value every label is an offset from, painted once on the axis,
    /// when the ticks sit too close together for their size to label
    /// whole (`204.51M +` over `+500`, `+1000`, …).
    pub base: Option<SharedString>,
    pub ticks: Vec<SharedString>,
}

/// Significant digits a tick label may need before the axis labels
/// offsets from a shared base instead.
const MAX_AXIS_DIGITS: i32 = 5;

/// Label an axis's ticks together, since what a tick needs to say depends
/// on its neighbours: every label shares one unit (`k` to `T`, or an
/// exponent in threes beyond them) and as many decimals as the spacing
/// needs to tell ticks apart.
pub(crate) fn axis_labels(ticks: &[f64]) -> AxisLabels {
    let largest = ticks.iter().fold(0.0f64, |m, t| m.max(t.abs()));
    let step = match ticks {
        [a, b, ..] => (b - a).abs(),
        _ => largest,
    };
    let (Some(&first), Some(&last)) = (ticks.first(), ticks.last()) else {
        return AxisLabels {
            base: None,
            ticks: Vec::new(),
        };
    };
    if largest > 0.0 && step > 0.0 && order(largest) - order(step) + 1 > MAX_AXIS_DIGITS {
        let unit = 10f64.powi(order((last - first).abs().max(step)) + 1);
        let base = (first.min(last) / unit).floor() * unit;
        let offsets: Vec<f64> = ticks.iter().map(|t| t - base).collect();
        let base = scaled(&[base], unit, false).remove(0);
        return AxisLabels {
            base: Some(format!("{base} +").into()),
            ticks: scaled(&offsets, step, true),
        };
    }
    AxisLabels {
        base: None,
        ticks: scaled(ticks, step, false),
    }
}

/// The power of ten `v` falls in.
fn order(v: f64) -> i32 {
    v.abs().log10().floor() as i32
}

/// Format `values` alike, in the unit their largest calls for, with the
/// decimals `step` needs. `signed` marks positive values with `+`.
fn scaled(values: &[f64], step: f64, signed: bool) -> Vec<SharedString> {
    let largest = values.iter().fold(0.0f64, |m, v| m.max(v.abs()));
    let (scale, suffix) = axis_unit(largest);
    let step = step / scale;
    // Enough decimals to land on the step exactly (`2.5` needs one), but
    // never more than two significant digits of it.
    let most = if step > 0.0 {
        (1 - order(step)).max(0)
    } else {
        0
    };
    let decimals = (0..most)
        .find(|&d| {
            let x = step * 10f64.powi(d);
            (x - x.round()).abs() < 1e-6 * x
        })
        .unwrap_or(most) as usize;
    values
        .iter()
        .map(|&v| {
            if v == 0.0 {
                return SharedString::new_static("0");
            }
            let sign = if signed && v > 0.0 { "+" } else { "" };
            format!("{sign}{:.*}{suffix}", decimals, v / scale).into()
        })
        .collect()
}

/// The scale and suffix an axis whose largest tick is `largest` labels in.
fn axis_unit(largest: f64) -> (f64, String) {
    const SUFFIXES: [(f64, &str); 4] = [(1e12, "T"), (1e9, "G"), (1e6, "M"), (1e3, "k")];
    if largest == 0.0 || (1e-3..1e4).contains(&largest) {
        return (1.0, String::new());
    }
    if (1e4..1e15).contains(&largest) {
        let (scale, suffix) = SUFFIXES
            .into_iter()
            .find(|(scale, _)| largest >= *scale)
            .expect("at least 1e4");
        return (scale, suffix.to_string());
    }
    let exponent = order(largest).div_euclid(3) * 3;
    (10f64.powi(exponent), format!("e{exponent}"))
}

/// `HH:MM:SS.mmm` local time from a payload timestamp (microseconds).
/// How long before `now` `at` was, in its largest whole unit: "5s ago".
pub(crate) fn format_age(
    at: metor_proto::types::Timestamp,
    now: metor_proto::types::Timestamp,
) -> SharedString {
    let secs = (now.0 - at.0).max(0) as f64 / 1e6;
    if secs < 60.0 {
        format!("{secs:.0}s ago")
    } else if secs < 3600.0 {
        format!("{:.0}m ago", secs / 60.0)
    } else {
        format!("{:.1}h ago", secs / 3600.0)
    }
    .into()
}

pub(crate) fn format_time(t_us: i64) -> String {
    let Ok(ts) = jiff::Timestamp::from_microsecond(t_us) else {
        return t_us.to_string();
    };
    let hms = ts
        .to_zoned(jiff::tz::TimeZone::system())
        .strftime("%H:%M:%S")
        .to_string();
    format!("{hms}.{:03}", t_us.rem_euclid(1_000_000) / 1000)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(ticks: &[f64]) -> (Option<String>, Vec<String>) {
        let labels = axis_labels(ticks);
        (
            labels.base.map(|b| b.to_string()),
            labels.ticks.iter().map(|t| t.to_string()).collect(),
        )
    }

    #[test]
    fn an_axis_shares_one_unit_and_the_decimals_its_spacing_needs() {
        assert_eq!(
            labels(&[0.0, 25e3, 50e3, 75e3]),
            (
                None,
                vec!["0".into(), "25k".into(), "50k".into(), "75k".into()]
            )
        );
        assert_eq!(
            labels(&[1.2e6, 1.4e6, 1.6e6]),
            (None, vec!["1.2M".into(), "1.4M".into(), "1.6M".into()])
        );
        assert_eq!(
            labels(&[0.0, 2.5, 5.0, 7.5]),
            (
                None,
                vec!["0".into(), "2.5".into(), "5.0".into(), "7.5".into()]
            )
        );
        assert_eq!(
            labels(&[0.0, 200.0, 400.0, 1200.0]).1,
            ["0", "200", "400", "1200"]
        );
        assert_eq!(labels(&[1e-4, 2e-4]).1, ["100e-6", "200e-6"]);
        assert_eq!(labels(&[0.0, 5e17, 1e18]).1, ["0", "0.5e18", "1.0e18"]);
        assert_eq!(
            labels(&[-1.0, -0.5, 0.0, 0.5]).1,
            ["-1.0", "-0.5", "0", "0.5"]
        );
    }

    #[test]
    fn ticks_close_together_for_their_size_are_offsets_from_a_base() {
        let ticks: Vec<f64> = (1..=6).map(|i| 204_510_000.0 + 500.0 * i as f64).collect();
        assert_eq!(
            labels(&ticks),
            (
                Some("204.51M +".into()),
                ["+500", "+1000", "+1500", "+2000", "+2500", "+3000"]
                    .map(String::from)
                    .to_vec()
            )
        );
        let negative: Vec<f64> = ticks.iter().rev().map(|t| -t).collect();
        let (base, offsets) = labels(&negative);
        assert_eq!(base.as_deref(), Some("-204.52M +"));
        assert_eq!(offsets.first().map(String::as_str), Some("+7000"));
    }

    #[test]
    fn readouts_stay_exact_and_group_their_digits() {
        assert_eq!(format_exact(204_512_734.4), "204\u{202F}512\u{202F}734");
        assert_eq!(format_exact(1234.5), "1234");
        assert_eq!(format_exact(-98_765.0), "-98\u{202F}765");
        assert_eq!(format_exact(1.5), "1.50");
        assert_eq!(format_exact(2.5e20), "2.5e20");
        assert_eq!(format_exact(-1.23456789e-7), "-1.23457e-7");
        assert_eq!(format_exact(0.0), "0");
        assert_eq!(
            format_element(ElementValue::U64(18_446_744_073_709_551_615)),
            " 18\u{202F}446\u{202F}744\u{202F}073\u{202F}709\u{202F}551\u{202F}615"
        );
        assert_eq!(format_element(ElementValue::I32(-42)), "-42");
    }
}
