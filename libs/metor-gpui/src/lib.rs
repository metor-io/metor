//! gpui primitives shared by metor-panel and the metor-ci web GUI.
//!
//! Everything here builds for `wasm32-unknown-unknown` on stable: no filesystem, no threads and
//! no persistence. Settings the host app keeps between runs, such as the theme, font and motion
//! preference, are plain globals here; the host loads and saves them.
//!
//! Call [`init`] once before opening a window, and install [`IconAssets`] with
//! `Application::with_assets`.

#![allow(clippy::type_complexity)]

pub mod button;
pub mod chord;
mod fonts;
mod icons;
pub mod motion;
pub mod popover;
pub mod row;
mod scrollbar;
mod switch;
pub mod table;
mod theme;
mod tooltip;
mod tree;

use gpui::App;

pub use fonts::{BUNDLED_FAMILY, UiSettings, font_family, register_fonts};
pub use icons::{Icon, IconAssets};
pub use motion::{Fade, MotionPreference};
pub use scrollbar::Scrollbar;
pub use switch::{OnPick, Switch};
pub use table::{Column, ColumnSort, Table, TableDelegate};
pub use theme::*;
pub use tooltip::TooltipText;
pub use tree::JsonTree;

/// Register the bundled font and install default theme and settings globals.
///
/// Globals the host installed already are kept, so a host loads its saved settings first and
/// then calls this.
pub fn init(cx: &mut App) {
    register_fonts(cx);
    if !cx.has_global::<ThemeRegistry>() {
        install_themes(cx, Vec::new(), None);
    }
    cx.default_global::<UiSettings>();
}
