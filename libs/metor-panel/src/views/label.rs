//! Static text drawn in the value-cell box.
//!
//! A schematic needs words that aren't telemetry — a subsystem name over a
//! group of chips, a "PRIMARY BUS" caption beside a toggle. Drawn as a
//! state-chip cell (same rounded box, same dimmed-accent fill and edge), a
//! caption sits in a row of live chips without reading as a different
//! kind of thing. It binds nothing and streams nothing.

use std::sync::Arc;

use gpui::{
    App, Context, FontWeight, Hsla, IntoElement, SharedString, Window, div, prelude::*, px,
};
use metor_db::DB;
use serde::{Deserialize, Serialize};

use super::value_strip::StripAlign;
use crate::inspect;
use crate::theme::{Theme, theme, ui_px};

/// Persisted shape of a [`Label`], shared by the tile and dashboard
/// surfaces and the Python config API.
///
/// The colours are `Option` so an unset one follows the theme instead of
/// freezing the palette the label was made under.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct LabelConfig {
    pub text: String,
    pub text_color: Option<Hsla>,
    /// Tints the box and its edge, as a state's colour tints its chip.
    pub color: Option<Hsla>,
    pub text_size: f32,
    pub align: StripAlign,
    pub bold: bool,
    pub uppercase: bool,
    /// Off leaves bare text over whatever the dashboard draws beneath.
    pub background: bool,
    pub border: bool,
}

impl Default for LabelConfig {
    fn default() -> Self {
        Self {
            text: "Label".into(),
            text_color: None,
            color: None,
            // The value-cell size, so a caption matches the chips around it.
            text_size: 12.0,
            align: StripAlign::Center,
            bold: false,
            uppercase: false,
            background: true,
            border: false,
        }
    }
}

/// A caption in a value-cell box.
#[derive(facet::Facet)]
pub struct Label {
    pub text: SharedString,
    pub text_color: Option<Hsla>,
    pub color: Option<Hsla>,
    #[facet(inspect::range(min = "6.0", max = "96.0"))]
    pub text_size: f32,
    pub align: StripAlign,
    pub bold: bool,
    pub uppercase: bool,
    /// Off leaves bare text over whatever the dashboard draws beneath.
    pub background: bool,
    pub border: bool,
}

impl Label {
    pub fn from_config(cfg: LabelConfig, _db: Arc<DB>, _cx: &mut Context<Self>) -> Self {
        Self {
            text: cfg.text.into(),
            text_color: cfg.text_color,
            color: cfg.color,
            text_size: cfg.text_size,
            align: cfg.align,
            bold: cfg.bold,
            uppercase: cfg.uppercase,
            background: cfg.background,
            border: cfg.border,
        }
    }

    pub fn to_config(&self, _cx: &App) -> LabelConfig {
        LabelConfig {
            text: self.text.to_string(),
            text_color: self.text_color,
            color: self.color,
            text_size: self.text_size,
            align: self.align,
            bold: self.bold,
            uppercase: self.uppercase,
            background: self.background,
            border: self.border,
        }
    }

    pub fn text(&self) -> &SharedString {
        &self.text
    }

    fn display_text(&self) -> SharedString {
        if self.uppercase {
            self.text.to_uppercase().into()
        } else {
            self.text.clone()
        }
    }
}

impl Render for Label {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let (bg, edge) = match self.color {
            Some(accent) => (Theme::dim(accent, 0.18), accent),
            None => (theme.bg_secondary, theme.line_color),
        };
        let cell = div()
            .size_full()
            .flex()
            .flex_row()
            .items_center()
            .px(ui_px(6.0))
            .py(ui_px(3.0))
            .rounded(px(3.0))
            .overflow_hidden()
            .when(self.background, |cell| cell.bg(bg))
            .when(self.border, |cell| cell.border_1().border_color(edge))
            .text_size(ui_px(self.text_size.clamp(1.0, 512.0)))
            .text_color(self.text_color.unwrap_or(theme.text_primary))
            .when(self.bold, |cell| cell.font_weight(FontWeight::BOLD));
        let text = div().child(self.display_text());
        let (cell, text) = match self.align {
            StripAlign::Left => (cell.justify_start(), text.text_left()),
            StripAlign::Center => (cell.justify_center(), text.text_center()),
            StripAlign::Right => (cell.justify_end(), text.text_right()),
        };
        div().size_full().p(ui_px(4.0)).child(cell.child(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_label_config_round_trips_and_defaults_when_partial() {
        let cfg = LabelConfig {
            text: "PRIMARY BUS".into(),
            text_color: Some(Hsla::default()),
            color: Some(Hsla::default()),
            text_size: 18.0,
            align: StripAlign::Right,
            bold: true,
            uppercase: true,
            background: false,
            border: true,
        };
        let s = serde_json::to_string(&cfg).unwrap();
        assert_eq!(serde_json::from_str::<LabelConfig>(&s).unwrap(), cfg);

        let partial: LabelConfig = serde_json::from_str(r#"{"text":"EPS"}"#).unwrap();
        assert_eq!(partial.text, "EPS");
        assert_eq!(partial.text_size, 12.0);
        assert_eq!(partial.align, StripAlign::Center);
        assert!(partial.background);
    }
}
