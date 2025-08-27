use std::sync::atomic::{self, AtomicPtr};

use egui::Color32;
use impeller2_wkt::Color;
use serde::{Deserialize, Serialize};

use crate::dirs;

pub const WHITE: Color32 = Color32::WHITE;
pub const TRANSPARENT: Color32 = Color32::TRANSPARENT;

pub const TURQUOISE_DEFAULT: Color32 = Color32::from_rgb(0x69, 0xB3, 0xBF);
pub const SLATE_DEFAULT: Color32 = Color32::from_rgb(0x7F, 0x70, 0xFF);
pub const PUMPKIN_DEFAULT: Color32 = Color32::from_rgb(0xFF, 0x6F, 0x1E);
pub const YOLK_DEFAULT: Color32 = Color32::from_rgb(0xFE, 0xC5, 0x04);
pub const PEACH_DEFAULT: Color32 = Color32::from_rgb(0xFF, 0xD7, 0xB3);
pub const REDDISH_DEFAULT: Color32 = Color32::from_rgb(0xE9, 0x4B, 0x14);
pub const HYPERBLUE_DEFAULT: Color32 = Color32::from_rgb(0x14, 0x5F, 0xCF);
pub const MINT_DEFAULT: Color32 = Color32::from_rgb(0x88, 0xDE, 0x9F);
pub const BONE_DEFAULT: Color32 = Color32::from_rgb(0xE4, 0xD9, 0xC3);

pub const TURQUOISE_40: Color32 = Color32::from_rgb(0x38, 0x55, 0x59);
pub const SLATE_40: Color32 = Color32::from_rgb(0x41, 0x3A, 0x73);
pub const PUMPKIN_40: Color32 = Color32::from_rgb(0x74, 0x3A, 0x1A);
pub const YOLK_40: Color32 = Color32::from_rgb(0x73, 0x5C, 0x0D);
pub const PEACH_40: Color32 = Color32::from_rgb(0x74, 0x63, 0x54);
pub const REDDISH_40: Color32 = Color32::from_rgb(0x6B, 0x2B, 0x15);
pub const HYPERBLUE_40: Color32 = Color32::from_rgb(0x16, 0x33, 0x60);
pub const MINT_40: Color32 = Color32::from_rgb(0x43, 0x66, 0x4C);

pub const SURFACE_PRIMARY: Color32 = Color32::from_rgb(0x1F, 0x1F, 0x1F);
pub const SURFACE_SECONDARY: Color32 = Color32::from_rgb(0x16, 0x16, 0x16);

pub fn get_color_by_index_solid(index: usize) -> Color32 {
    let colors = [
        TURQUOISE_DEFAULT,
        SLATE_DEFAULT,
        PUMPKIN_DEFAULT,
        YOLK_DEFAULT,
        PEACH_DEFAULT,
        REDDISH_DEFAULT,
        HYPERBLUE_DEFAULT,
        MINT_DEFAULT,
    ];
    colors[index % colors.len()]
}

pub const ALL_COLORS_DARK: &[Color32] = &[
    Color32::from_rgb(47, 23, 57),
    Color32::from_rgb(49, 23, 58),
    Color32::from_rgb(50, 24, 59),
    Color32::from_rgb(51, 24, 60),
    Color32::from_rgb(53, 24, 61),
    Color32::from_rgb(54, 25, 62),
    Color32::from_rgb(56, 25, 63),
    Color32::from_rgb(57, 25, 64),
    Color32::from_rgb(59, 26, 65),
    Color32::from_rgb(61, 26, 66),
    Color32::from_rgb(62, 26, 67),
    Color32::from_rgb(64, 27, 68),
    Color32::from_rgb(65, 27, 69),
    Color32::from_rgb(67, 27, 69),
    Color32::from_rgb(68, 27, 70),
    Color32::from_rgb(70, 28, 71),
    Color32::from_rgb(71, 28, 72),
    Color32::from_rgb(73, 28, 73),
    Color32::from_rgb(74, 28, 73),
    Color32::from_rgb(76, 28, 74),
    Color32::from_rgb(78, 29, 75),
    Color32::from_rgb(79, 29, 76),
    Color32::from_rgb(81, 29, 76),
    Color32::from_rgb(82, 29, 77),
    Color32::from_rgb(84, 29, 78),
    Color32::from_rgb(85, 29, 78),
    Color32::from_rgb(87, 30, 79),
    Color32::from_rgb(89, 30, 80),
    Color32::from_rgb(90, 30, 80),
    Color32::from_rgb(92, 30, 81),
    Color32::from_rgb(93, 30, 81),
    Color32::from_rgb(95, 30, 82),
    Color32::from_rgb(97, 30, 82),
    Color32::from_rgb(98, 30, 83),
    Color32::from_rgb(100, 30, 83),
    Color32::from_rgb(102, 30, 84),
    Color32::from_rgb(103, 30, 84),
    Color32::from_rgb(105, 31, 85),
    Color32::from_rgb(106, 31, 85),
    Color32::from_rgb(108, 31, 86),
    Color32::from_rgb(110, 31, 86),
    Color32::from_rgb(111, 31, 87),
    Color32::from_rgb(113, 31, 87),
    Color32::from_rgb(115, 31, 87),
    Color32::from_rgb(116, 30, 88),
    Color32::from_rgb(118, 30, 88),
    Color32::from_rgb(120, 30, 88),
    Color32::from_rgb(121, 30, 89),
    Color32::from_rgb(123, 30, 89),
    Color32::from_rgb(125, 30, 89),
    Color32::from_rgb(126, 30, 89),
    Color32::from_rgb(128, 30, 90),
    Color32::from_rgb(130, 30, 90),
    Color32::from_rgb(131, 30, 90),
    Color32::from_rgb(133, 29, 90),
    Color32::from_rgb(135, 29, 90),
    Color32::from_rgb(137, 29, 90),
    Color32::from_rgb(138, 29, 91),
    Color32::from_rgb(140, 29, 91),
    Color32::from_rgb(142, 28, 91),
    Color32::from_rgb(143, 28, 91),
    Color32::from_rgb(145, 28, 91),
    Color32::from_rgb(147, 28, 91),
    Color32::from_rgb(148, 27, 91),
    Color32::from_rgb(150, 27, 91),
    Color32::from_rgb(152, 27, 91),
    Color32::from_rgb(154, 26, 91),
    Color32::from_rgb(155, 26, 91),
    Color32::from_rgb(157, 26, 91),
    Color32::from_rgb(159, 26, 91),
    Color32::from_rgb(161, 25, 90),
    Color32::from_rgb(162, 25, 90),
    Color32::from_rgb(164, 25, 90),
    Color32::from_rgb(166, 24, 90),
    Color32::from_rgb(167, 24, 90),
    Color32::from_rgb(169, 24, 89),
    Color32::from_rgb(171, 23, 89),
    Color32::from_rgb(172, 23, 89),
    Color32::from_rgb(174, 23, 89),
    Color32::from_rgb(176, 22, 88),
    Color32::from_rgb(177, 22, 88),
    Color32::from_rgb(179, 22, 87),
    Color32::from_rgb(181, 22, 87),
    Color32::from_rgb(182, 22, 87),
    Color32::from_rgb(184, 22, 86),
    Color32::from_rgb(186, 22, 86),
    Color32::from_rgb(187, 22, 85),
    Color32::from_rgb(189, 22, 84),
    Color32::from_rgb(191, 22, 84),
    Color32::from_rgb(192, 22, 83),
    Color32::from_rgb(194, 23, 83),
    Color32::from_rgb(195, 23, 82),
    Color32::from_rgb(197, 23, 81),
    Color32::from_rgb(198, 24, 81),
    Color32::from_rgb(200, 25, 80),
    Color32::from_rgb(201, 25, 79),
    Color32::from_rgb(203, 26, 79),
    Color32::from_rgb(204, 27, 78),
    Color32::from_rgb(205, 28, 77),
    Color32::from_rgb(207, 29, 77),
    Color32::from_rgb(208, 30, 76),
    Color32::from_rgb(209, 31, 75),
    Color32::from_rgb(211, 33, 74),
    Color32::from_rgb(212, 34, 74),
    Color32::from_rgb(213, 35, 73),
    Color32::from_rgb(215, 37, 72),
    Color32::from_rgb(216, 38, 71),
    Color32::from_rgb(217, 39, 71),
    Color32::from_rgb(218, 41, 70),
    Color32::from_rgb(219, 42, 69),
    Color32::from_rgb(220, 44, 69),
    Color32::from_rgb(222, 45, 68),
    Color32::from_rgb(223, 47, 67),
    Color32::from_rgb(224, 48, 66),
    Color32::from_rgb(225, 50, 66),
    Color32::from_rgb(226, 52, 65),
    Color32::from_rgb(227, 53, 65),
    Color32::from_rgb(228, 55, 64),
    Color32::from_rgb(229, 57, 64),
    Color32::from_rgb(230, 59, 63),
    Color32::from_rgb(230, 60, 63),
    Color32::from_rgb(231, 62, 62),
    Color32::from_rgb(232, 64, 62),
    Color32::from_rgb(233, 66, 62),
    Color32::from_rgb(233, 68, 61),
    Color32::from_rgb(234, 70, 61),
    Color32::from_rgb(235, 72, 61),
    Color32::from_rgb(235, 74, 61),
    Color32::from_rgb(236, 76, 61),
    Color32::from_rgb(236, 78, 62),
    Color32::from_rgb(237, 80, 62),
    Color32::from_rgb(237, 81, 62),
    Color32::from_rgb(238, 83, 63),
    Color32::from_rgb(238, 85, 63),
    Color32::from_rgb(239, 87, 64),
    Color32::from_rgb(239, 89, 64),
    Color32::from_rgb(239, 91, 65),
    Color32::from_rgb(240, 93, 66),
    Color32::from_rgb(240, 95, 67),
    Color32::from_rgb(240, 97, 68),
    Color32::from_rgb(241, 99, 69),
    Color32::from_rgb(241, 101, 70),
    Color32::from_rgb(241, 103, 71),
    Color32::from_rgb(241, 105, 72),
    Color32::from_rgb(242, 107, 73),
    Color32::from_rgb(242, 109, 74),
    Color32::from_rgb(242, 110, 75),
    Color32::from_rgb(242, 112, 77),
    Color32::from_rgb(242, 114, 78),
    Color32::from_rgb(243, 116, 79),
    Color32::from_rgb(243, 118, 81),
    Color32::from_rgb(243, 120, 82),
    Color32::from_rgb(243, 121, 83),
    Color32::from_rgb(243, 123, 85),
    Color32::from_rgb(243, 125, 86),
    Color32::from_rgb(243, 127, 88),
    Color32::from_rgb(244, 128, 89),
    Color32::from_rgb(244, 130, 91),
    Color32::from_rgb(244, 132, 92),
    Color32::from_rgb(244, 134, 94),
    Color32::from_rgb(244, 135, 95),
    Color32::from_rgb(244, 137, 97),
    Color32::from_rgb(244, 139, 98),
    Color32::from_rgb(244, 141, 100),
    Color32::from_rgb(244, 142, 101),
    Color32::from_rgb(245, 144, 103),
    Color32::from_rgb(245, 146, 105),
    Color32::from_rgb(245, 147, 106),
    Color32::from_rgb(245, 149, 108),
    Color32::from_rgb(245, 151, 110),
    Color32::from_rgb(245, 152, 111),
    Color32::from_rgb(245, 154, 113),
    Color32::from_rgb(245, 156, 115),
    Color32::from_rgb(245, 157, 116),
    Color32::from_rgb(245, 159, 118),
    Color32::from_rgb(245, 161, 120),
    Color32::from_rgb(245, 162, 122),
    Color32::from_rgb(245, 164, 123),
    Color32::from_rgb(245, 166, 125),
    Color32::from_rgb(245, 167, 127),
    Color32::from_rgb(245, 169, 129),
    Color32::from_rgb(245, 170, 131),
    Color32::from_rgb(245, 172, 133),
    Color32::from_rgb(245, 174, 135),
    Color32::from_rgb(246, 175, 137),
    Color32::from_rgb(246, 177, 139),
    Color32::from_rgb(246, 178, 140),
    Color32::from_rgb(246, 180, 142),
    Color32::from_rgb(246, 182, 144),
    Color32::from_rgb(246, 183, 146),
    Color32::from_rgb(246, 185, 149),
    Color32::from_rgb(246, 186, 151),
    Color32::from_rgb(246, 188, 153),
    Color32::from_rgb(246, 189, 155),
    Color32::from_rgb(246, 191, 157),
    Color32::from_rgb(246, 192, 159),
    Color32::from_rgb(246, 194, 161),
    Color32::from_rgb(246, 195, 163),
    Color32::from_rgb(246, 197, 165),
    Color32::from_rgb(246, 199, 168),
    Color32::from_rgb(246, 200, 170),
    Color32::from_rgb(246, 202, 172),
    Color32::from_rgb(247, 203, 174),
    Color32::from_rgb(247, 205, 176),
    Color32::from_rgb(247, 206, 179),
    Color32::from_rgb(247, 207, 181),
    Color32::from_rgb(247, 209, 183),
    Color32::from_rgb(247, 210, 185),
    Color32::from_rgb(247, 212, 187),
    Color32::from_rgb(247, 213, 190),
    Color32::from_rgb(248, 215, 192),
    Color32::from_rgb(248, 216, 194),
    Color32::from_rgb(248, 218, 196),
    Color32::from_rgb(248, 219, 198),
    Color32::from_rgb(248, 221, 201),
    Color32::from_rgb(248, 222, 203),
    Color32::from_rgb(248, 224, 205),
    Color32::from_rgb(249, 225, 207),
    Color32::from_rgb(249, 227, 209),
    Color32::from_rgb(249, 228, 211),
    Color32::from_rgb(249, 230, 214),
    Color32::from_rgb(249, 231, 216),
    Color32::from_rgb(250, 233, 218),
    Color32::from_rgb(250, 234, 220),
];

pub fn get_color_by_index_all(index: usize) -> Color32 {
    use std::hash::Hasher;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    hasher.write_usize(index);
    let index = hasher.finish() as usize;
    ALL_COLORS_DARK[index % ALL_COLORS_DARK.len()]
}

pub fn with_opacity(color: Color32, opacity: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), (255.0 * opacity) as u8)
}

pub trait EColor {
    fn into_color32(self) -> Color32;
    fn from_color32(color: egui::Color32) -> Self;
}

impl EColor for Color {
    fn into_color32(self) -> Color32 {
        Color32::from_rgba_unmultiplied(
            (255.0 * self.r) as u8,
            (255.0 * self.g) as u8,
            (255.0 * self.b) as u8,
            (255.0 * self.a) as u8,
        )
    }

    fn from_color32(color: egui::Color32) -> Self {
        let [r, g, b, a] = color.to_srgba_unmultiplied();
        Self {
            r: r as f32 / 255.0,
            g: g as f32 / 255.0,
            b: b as f32 / 255.0,
            a: a as f32 / 255.0,
        }
    }
}

pub trait ColorExt {
    fn into_bevy(self) -> ::bevy::prelude::Color;
    fn opacity(self, opacity: f32) -> Self;
}

impl ColorExt for Color32 {
    fn into_bevy(self) -> ::bevy::prelude::Color {
        let [r, g, b, a] = self.to_srgba_unmultiplied().map(|c| c as f32 / 255.0);
        ::bevy::prelude::Color::srgba(r, g, b, a)
    }

    fn opacity(self, opacity: f32) -> Self {
        with_opacity(self, opacity)
    }
}

pub mod bevy {
    use bevy::prelude::Color;
    pub const RED: Color = Color::srgb(0.91, 0.29, 0.08);
    pub const GREEN: Color = Color::srgb(0.53, 0.87, 0.62);
    pub const BLUE: Color = Color::srgb(0.08, 0.38, 0.82);
    pub const GREY_900: Color = Color::srgb(0.2, 0.2, 0.2);
}

#[derive(Deserialize, Serialize)]
pub struct ColorScheme {
    pub bg_primary: Color32,
    pub bg_secondary: Color32,

    pub text_primary: Color32,
    pub text_secondary: Color32,
    pub text_tertiary: Color32,

    pub icon_primary: Color32,
    pub icon_secondary: Color32,

    pub border_primary: Color32,

    pub highlight: Color32,
    pub blue: Color32,
    pub error: Color32,
    pub success: Color32,

    pub shadow: Color32,
}

pub static DARK: ColorScheme = ColorScheme {
    bg_primary: Color32::from_rgb(0x1F, 0x1F, 0x1F),
    bg_secondary: Color32::from_rgb(0x16, 0x16, 0x16),

    text_primary: Color32::from_rgb(0xFF, 0xFB, 0xF0),
    text_secondary: Color32::from_rgb(0x6D, 0x6D, 0x6D),
    text_tertiary: Color32::from_rgb(0x6B, 0x6B, 0x6B),

    icon_primary: Color32::from_rgb(0xFF, 0xFB, 0xF0),
    icon_secondary: Color32::from_rgb(0x62, 0x62, 0x62),

    border_primary: Color32::from_rgb(0x2E, 0x2D, 0x2C),

    highlight: Color32::from_rgb(0x14, 0x5F, 0xCF),
    blue: Color32::from_rgb(0x14, 0x5F, 0xCF),
    error: REDDISH_DEFAULT,
    success: Color32::from_rgb(0xFF, 0x4F, 0x00),

    shadow: Color32::BLACK,
};

pub static LIGHT: ColorScheme = ColorScheme {
    bg_primary: Color32::from_rgb(0xFF, 0xFB, 0xF0),
    bg_secondary: Color32::from_rgb(0xE6, 0xE2, 0xD8),

    text_primary: Color32::from_rgb(0x17, 0x16, 0x15),
    text_secondary: Color32::from_rgb(0x2E, 0x2D, 0x2C),
    text_tertiary: Color32::from_rgb(0x45, 0x45, 0x44),

    icon_primary: Color32::from_rgb(0x17, 0x16, 0x15),
    icon_secondary: Color32::from_rgb(0x2E, 0x2D, 0x2C),

    border_primary: Color32::from_rgb(0xCD, 0xC3, 0xB0),

    highlight: Color32::from_rgb(0x14, 0x5F, 0xCF),
    blue: Color32::from_rgb(0x14, 0x5F, 0xCF),
    error: REDDISH_DEFAULT,
    success: Color32::from_rgb(0xFF, 0x4F, 0x00),

    shadow: Color32::BLACK,
};

pub static CATPPUCINI_LATTE: ColorScheme = ColorScheme {
    bg_primary: Color32::from_rgb(0xEF, 0xF1, 0xF5),
    bg_secondary: Color32::from_rgb(0xDC, 0xE0, 0xE8),

    text_primary: Color32::from_rgb(0x4C, 0x4F, 0x69),
    text_secondary: Color32::from_rgb(0x5C, 0x5F, 0x77),
    text_tertiary: Color32::from_rgb(0x6C, 0x6F, 0x85),

    icon_primary: Color32::from_rgb(0x40, 0x40, 0x40),
    icon_secondary: Color32::from_rgb(0x80, 0x80, 0x80),

    border_primary: Color32::from_rgb(0xCC, 0xD0, 0xDA),

    highlight: Color32::from_rgb(0x7C, 0x7F, 0x93),
    blue: Color32::from_rgb(0x1E, 0x66, 0xF5),
    error: Color32::from_rgb(0xE6, 0x45, 0x53),
    success: Color32::from_rgb(0x40, 0xA0, 0x2B),

    shadow: Color32::BLACK,
};

pub static CATPPUCINI_MOCHA: ColorScheme = ColorScheme {
    bg_primary: Color32::from_rgb(0x1E, 0x1E, 0x2E),
    bg_secondary: Color32::from_rgb(0x11, 0x11, 0x1B),

    text_primary: Color32::from_rgb(0xCD, 0xD6, 0xF4),
    text_secondary: Color32::from_rgb(0xBA, 0xC2, 0xDE),
    text_tertiary: Color32::from_rgb(0xA6, 0xAD, 0xC8),

    icon_primary: Color32::from_rgb(0xCD, 0xD6, 0xF4),
    icon_secondary: Color32::from_rgb(0xBA, 0xC2, 0xDE),

    border_primary: Color32::from_rgb(0x31, 0x32, 0x44),

    highlight: Color32::from_rgb(0x93, 0x99, 0xB2),
    blue: Color32::from_rgb(0x89, 0xB4, 0xFA),
    error: Color32::from_rgb(0xF3, 0x8B, 0xA8),
    success: Color32::from_rgb(0xA6, 0xE3, 0xA1),

    shadow: Color32::BLACK,
};

pub static CATPPUCINI_MACCHIATO: ColorScheme = ColorScheme {
    bg_primary: Color32::from_rgb(0x24, 0x27, 0x3A),
    bg_secondary: Color32::from_rgb(0x1E, 0x20, 0x30),

    text_primary: Color32::from_rgb(0xCA, 0xD3, 0xF5),
    text_secondary: Color32::from_rgb(0xA5, 0xAD, 0xCB),
    text_tertiary: Color32::from_rgb(0xB8, 0xC0, 0xE0),

    icon_primary: Color32::from_rgb(0xCA, 0xD3, 0xF5),
    icon_secondary: Color32::from_rgb(0xA5, 0xAD, 0xCB),

    border_primary: Color32::from_rgb(0x45, 0x47, 0x5A),

    highlight: Color32::from_rgb(0x6E, 0x73, 0x8D),
    blue: Color32::from_rgb(0x8A, 0xAD, 0xF4),
    error: Color32::from_rgb(0xED, 0x87, 0x96),
    success: Color32::from_rgb(0xA6, 0xDA, 0x95),

    shadow: Color32::BLACK,
};

pub static AYU_DARK: ColorScheme = ColorScheme {
    bg_primary: Color32::from_rgb(0x0B, 0x0E, 0x14), // ui.bg
    bg_secondary: Color32::from_rgb(0x0F, 0x13, 0x1A), // ui.panel.bg

    text_primary: Color32::from_rgb(0xBF, 0xBD, 0xB6), // editor.fg
    text_secondary: Color32::from_rgb(0x56, 0x5B, 0x66), // ui.fg
    text_tertiary: Color32::from_rgb(0x6C, 0x73, 0x80), // gutter.active

    icon_primary: Color32::from_rgb(0xBF, 0xBD, 0xB6), // editor.fg
    icon_secondary: Color32::from_rgb(0x6C, 0x73, 0x80), // gutter.active

    border_primary: Color32::from_rgb(0x11, 0x15, 0x1C), // ui.line

    highlight: Color32::from_rgb(0x47, 0x52, 0x66), // ui.selection.normal
    blue: Color32::from_rgb(0x59, 0xC2, 0xFF),      // syntax.entity
    error: Color32::from_rgb(0xD9, 0x57, 0x57),     // common.error
    success: Color32::from_rgb(0x7F, 0xD9, 0x62),   // vcs.added

    shadow: Color32::BLACK,
};

pub static AYU_LIGHT: ColorScheme = ColorScheme {
    bg_primary: Color32::from_rgb(0xF8, 0xF9, 0xFA), // ui.bg
    bg_secondary: Color32::from_rgb(0xF3, 0xF4, 0xF5), // ui.panel.bg

    text_primary: Color32::from_rgb(0x5C, 0x61, 0x66), // editor.fg
    text_secondary: Color32::from_rgb(0x8A, 0x91, 0x99), // ui.fg
    text_tertiary: Color32::from_rgb(0x8A, 0x91, 0x99), // gutter.active

    icon_primary: Color32::from_rgb(0x5C, 0x61, 0x66), // editor.fg
    icon_secondary: Color32::from_rgb(0x8A, 0x91, 0x99), // gutter.active

    border_primary: Color32::from_rgb(0x6B, 0x7D, 0x8F), // ui.line (with alpha applied)

    highlight: Color32::from_rgb(0x56, 0x72, 0x8F), // ui.selection.normal
    blue: Color32::from_rgb(0x39, 0x9E, 0xE6),      // syntax.entity
    error: Color32::from_rgb(0xE6, 0x50, 0x50),     // common.error
    success: Color32::from_rgb(0x6C, 0xBF, 0x43),   // vcs.added

    shadow: Color32::BLACK,
};

static COLOR_SCHEME: AtomicPtr<ColorScheme> = AtomicPtr::new(std::ptr::null_mut());

pub fn get_scheme() -> &'static ColorScheme {
    let ptr = COLOR_SCHEME.load(atomic::Ordering::Relaxed);
    if ptr.is_null() {
        let scheme = load_color_scheme()
            .map(|c| &*Box::leak(Box::new(c)))
            .unwrap_or(&DARK);
        COLOR_SCHEME.store((scheme as *const _) as *mut _, atomic::Ordering::Relaxed);
        scheme
    } else {
        unsafe { &*ptr }
    }
}

fn load_color_scheme() -> Option<ColorScheme> {
    let color_scheme_path = dirs().data_dir().join("color_scheme.json");
    let json = std::fs::read_to_string(color_scheme_path).ok()?;
    serde_json::from_str(&json).ok()
}

pub fn set_schema(schema: &'static ColorScheme) {
    COLOR_SCHEME.store((schema as *const _) as *mut _, atomic::Ordering::Relaxed);
    let color_scheme_path = dirs().data_dir().join("color_scheme.json");
    if let Ok(json) = serde_json::to_string(&schema) {
        let _ = std::fs::write(color_scheme_path, json);
    }
}
