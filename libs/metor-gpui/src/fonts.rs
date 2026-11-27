//! The bundled font and the UI settings that pick fonts and motion.

use std::borrow::Cow;

use gpui::{App, Global, SharedString};

use crate::MotionPreference;

/// Family name of the font bundled into the binary. It is always registered, so it always
/// resolves, including on the web, where no system fonts are listed.
pub const BUNDLED_FAMILY: &str = "IoskeleyMono Nerd Font";

/// Load the bundled font into gpui's text system.
///
/// [`crate::init`] calls this; call it directly only when not using `init`.
pub fn register_fonts(cx: &App) {
    cx.text_system()
        .add_fonts(vec![
            Cow::Borrowed(include_bytes!(
                "../assets/fonts/IoskeleyMonoNerdFont-Regular.ttf"
            )),
            Cow::Borrowed(include_bytes!(
                "../assets/fonts/IoskeleyMonoNerdFont-Bold.ttf"
            )),
            Cow::Borrowed(include_bytes!(
                "../assets/fonts/IoskeleyMonoNerdFont-Italic.ttf"
            )),
        ])
        .expect("failed to register embedded fonts");
}

/// Appearance settings every element reads, held as a [`Global`].
///
/// The crate never persists them. The host app loads them at startup, observes the global and
/// saves changes wherever it keeps preferences.
#[derive(Clone, Debug)]
pub struct UiSettings {
    /// The family the UI renders with. The host resolves it to an installed family.
    pub font_family: SharedString,
    pub motion: MotionPreference,
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            font_family: SharedString::new_static(BUNDLED_FAMILY),
            motion: MotionPreference::default(),
        }
    }
}

impl Global for UiSettings {}

/// The family the UI renders with. Falls back to [`BUNDLED_FAMILY`] when no [`UiSettings`] is
/// installed.
pub fn font_family(cx: &App) -> SharedString {
    cx.try_global::<UiSettings>()
        .map_or(SharedString::new_static(BUNDLED_FAMILY), |settings| {
            settings.font_family.clone()
        })
}
