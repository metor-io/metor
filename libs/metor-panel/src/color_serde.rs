//! Colours in saved layouts and configs, as `#rrggbbaa` hex: the format gpui
//! wrote before gpui-ce moved `Hsla` onto palette, whose own serde writes a
//! struct. Saved files stay readable both ways by naming this module on each
//! colour field: `#[serde(with = "crate::color_serde")]`, or the [`option`]
//! and [`overridden`] submodules for the wrapped forms.

use gpui::Hsla;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::views::time_series::Override;

/// A colour that serializes as hex. The wrapped forms go through it.
struct Hex(Hsla);

impl Serialize for Hex {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let c = gpui::hsla_to_rgba(self.0);
        let [r, g, b, a] = [c.red, c.green, c.blue, c.alpha].map(|v| (v * 255.0).round() as u8);
        serializer.serialize_str(&format!("#{r:02x}{g:02x}{b:02x}{a:02x}"))
    }
}

impl<'de> Deserialize<'de> for Hex {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        parse(&text)
            .map(Hex)
            .ok_or_else(|| de::Error::custom(format!("invalid colour {text:?}")))
    }
}

/// `#rgb`, `#rgba`, `#rrggbb` or `#rrggbbaa`, as gpui accepted them.
fn parse(text: &str) -> Option<Hsla> {
    let digits = text.strip_prefix('#')?;
    let nibble = |i: usize| {
        u8::from_str_radix(&digits[i..i + 1], 16)
            .ok()
            .map(|v| v * 17)
    };
    let byte = |i: usize| u8::from_str_radix(digits.get(i..i + 2)?, 16).ok();
    let [r, g, b, a] = match digits.len() {
        3 => [nibble(0)?, nibble(1)?, nibble(2)?, 255],
        4 => [nibble(0)?, nibble(1)?, nibble(2)?, nibble(3)?],
        6 => [byte(0)?, byte(2)?, byte(4)?, 255],
        8 => [byte(0)?, byte(2)?, byte(4)?, byte(6)?],
        _ => return None,
    }
    .map(|v| v as f32 / 255.0);
    Some(gpui::rgb_to_hsla(gpui::Rgba::new(r, g, b, a)))
}

pub(crate) fn serialize<S: Serializer>(color: &Hsla, serializer: S) -> Result<S::Ok, S::Error> {
    Hex(*color).serialize(serializer)
}

pub(crate) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Hsla, D::Error> {
    Ok(Hex::deserialize(deserializer)?.0)
}

/// For an `Option<Hsla>` field.
pub(crate) mod option {
    use super::*;

    pub(crate) fn serialize<S: Serializer>(
        color: &Option<Hsla>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        color.map(Hex).serialize(serializer)
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Hsla>, D::Error> {
        Ok(Option::<Hex>::deserialize(deserializer)?.map(|hex| hex.0))
    }
}

/// For an `Override<Hsla>` field.
pub(crate) mod overridden {
    use super::*;

    pub(crate) fn serialize<S: Serializer>(
        color: &Override<Hsla>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match color {
            Override::Auto => Override::<Hex>::Auto,
            Override::Custom(c) => Override::Custom(Hex(*c)),
        }
        .serialize(serializer)
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Override<Hsla>, D::Error> {
        Ok(match Override::<Hex>::deserialize(deserializer)? {
            Override::Auto => Override::Auto,
            Override::Custom(hex) => Override::Custom(hex.0),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips_and_reads_the_short_forms() {
        let color = parse("#f38ba8ff").unwrap();
        let mut out = Vec::new();
        serialize(&color, &mut serde_json::Serializer::new(&mut out)).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "\"#f38ba8ff\"");
        let short = gpui::hsla_to_rgba(parse("#f00").unwrap());
        assert_eq!(
            [short.red, short.green, short.blue, short.alpha],
            [1.0, 0.0, 0.0, 1.0]
        );
        assert!(parse("f38ba8").is_none() && parse("#f38ba").is_none());
    }
}
