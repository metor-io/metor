//! The panel's theme extensions and font choice.
//!
//! The palettes themselves live in [`metor_gpui`]. This module adds the colors only telemetry
//! views draw (alarms, run states, gradients, maps, images and density ramps) as
//! [`PanelThemeExt`], and resolves the panel's persisted [`FontConfig`] to a family.

use gpui::{App, Hsla, Pixels, SharedString, px};
use metor_gpui::{BUNDLED_FAMILY, Theme, UiSettings, hex, lerp_color};
use metor_proto_wkt::SequenceChannelState;

use crate::config::{self, FontConfig, PanelConfig};
use crate::views::gradient::Palette;

/// Backdrop blur under the chrome strips laid over a plot, a pane header and the legend,
/// softer than a surface's so the data still shows shape.
pub const CHROME_BLUR: Pixels = px(8.0);

/// Colors only the panel's telemetry views draw, derived from a [`Theme`]'s tokens.
pub trait PanelThemeExt {
    /// A theme tint over the native blurred backdrop in the splash sidebar.
    fn splash_sidebar_bg(&self) -> Hsla;

    /// The logo's ember ramp, fading into the current splash surface.
    fn logo_color(&self, level: f32) -> Hsla;

    /// Drop shadow behind the window when Linux client-side decorations are
    /// active. Derived rather than per-palette: translucent black reads
    /// correctly over any compositor wallpaper.
    fn window_shadow(&self) -> Hsla;

    /// Translucent band over plot regions whose data is remote-only or
    /// still downloading.
    fn plot_gap_band(&self) -> Hsla;

    /// Outline around the selected widget on a dashboard: the theme's accent
    /// (the hue `drop_target` tints with) at full strength.
    fn selection_ring(&self) -> Hsla;

    /// The filled part of a scrubber, over its `bg_secondary` box. Derived
    /// from the text colour so it reads against the box and against the
    /// surfaces around it (an inspector's `bg_elevated`, a selected row's
    /// `selection_bg`) in light themes and dark.
    fn scrub_fill(&self) -> Hsla;

    /// Semi-transparent `bg_secondary` for plot chrome strips — the legend,
    /// the axis fills and a pane header laid over a plot — which dims the
    /// data running on beneath. Derived so it tracks whatever `bg_secondary`
    /// a theme picks.
    fn plot_chrome_bg(&self) -> Hsla;

    /// Text of a value cell once its stream has gone quiet — the theme's
    /// blue, so the digits themselves say the reading is old.
    fn stale_text(&self) -> Hsla;

    /// Background behind [`stale_text`](Self::stale_text): the same blue at
    /// the weight the sequence run-state chips use, so the two read as one
    /// family of "status" tints.
    fn stale_bg(&self) -> Hsla;

    /// Where a dragged pane or tab will land: a fixed blue, the same on
    /// every theme, so a drop reads as "goes here" rather than as any
    /// palette's status hue.
    fn drop_accent(&self) -> Hsla;

    /// [`drop_accent`](Self::drop_accent) as a fill behind a drop zone.
    fn drop_fill(&self) -> Hsla;

    /// Tint for a component-frame edge in the system graph — the blue family,
    /// the calmer of the two edge kinds since frame edges dominate a target.
    fn frame_edge_color(&self) -> Hsla;

    /// Current-position marker on the map view — the orange family, the
    /// one hue basemaps never paint terrain or water in, so the dot never
    /// dissolves into the land greens and ocean blues around it.
    fn map_marker(&self) -> Hsla;

    /// Ground-track trail behind the map marker — the blue family, which
    /// stays legible over both OSM land and ocean tints.
    fn map_trail(&self) -> Hsla;

    /// Behind an image and its letterbox: black, so the darkest pixel and
    /// the border read the same on every theme.
    fn image_backdrop(&self) -> Hsla;

    /// Pixels at the sensor's top code — the theme's critical red, the one
    /// hue no colormap ends on.
    fn image_saturated(&self) -> Hsla;

    /// Detected-star overlay markers — the green family.
    fn image_overlay_detected(&self) -> Hsla;

    /// Truth overlay markers — the purple family, apart from the detections.
    fn image_overlay_truth(&self) -> Hsla;

    /// Solid color for an alarm severity (`severity_index`: 0 = info, 1 = warning,
    /// 2 = critical), used for limit lines, severity chips, and the status bar.
    /// Critical tracks the theme's `error_accent`; warning/info are fixed amber/blue
    /// that read on both light and dark themes.
    fn alarm_color(&self, severity_index: usize) -> Hsla;

    /// Low-alpha [`alarm_color`](Self::alarm_color) for out-of-bounds plot tinting.
    fn alarm_tint(&self, severity_index: usize) -> Hsla;

    /// A box fill for a gradient position `t` in -1..=1: the plain box at
    /// 0, moving toward the palette's colour, never so far that the value
    /// text stops reading.
    fn gradient_fill(&self, palette: Palette, t: f32) -> Hsla;

    /// A few fills along a palette, for a menu to show what it looks like.
    fn gradient_preview(&self, palette: Palette) -> [Hsla; 4];

    /// Solid color for a sequence channel's state; `None` is a channel with
    /// no reading yet. Tracks the theme's `control_active`/`error_accent` for
    /// the running/failed poles; the intermediate states use fixed hues that
    /// read on both light and dark themes.
    fn run_state_color(&self, state: Option<SequenceChannelState>) -> Hsla;

    /// Low-alpha [`run_state_color`](Self::run_state_color) for swatch/row backgrounds.
    fn run_state_tint(&self, state: Option<SequenceChannelState>) -> Hsla;

    /// Solid color for a cyclic slot's run state, indexed by the FSW's
    /// `SlotState::code` (0 = empty, 1 = loaded, 2 = loading, 3 = running,
    /// 4 = done, 5 = stopped). Distinct from
    /// [`run_state_color`](Self::run_state_color), which colors the sequence
    /// channel states — the two ladders share neither order nor meaning, and
    /// painting a running slot with the sequence table would read as "aborted".
    fn slot_state_color(&self, state_code: usize) -> Hsla;

    /// Low-alpha [`slot_state_color`](Self::slot_state_color) for lane
    /// backgrounds and summarized strips.
    fn slot_state_tint(&self, state_code: usize) -> Hsla;

    /// Sample the [`density_stops`](Self::density_stops) ramp at `t`, clamped
    /// to `0..=1`. Interpolation is component-wise in RGB, which is what the
    /// GPU LUT reproduces, so the CPU-painted colorbar and the shader agree.
    fn density_color(&self, t: f32) -> Hsla;
}

impl PanelThemeExt for Theme {
    fn splash_sidebar_bg(&self) -> Hsla {
        Self::dim(self.bg_elevated, Self::BACKDROP_TINT)
    }

    fn logo_color(&self, level: f32) -> Hsla {
        let stops: [gpui::Rgba; 3] = [
            self.bg_elevated.into(),
            hex(0x8c360e, 1.).into(),
            hex(0xff4f00, 1.).into(),
        ];
        let value = level.clamp(0., 1.) * 2.;
        let i = (value as usize).min(1);
        let fraction = value - i as f32;
        let a = stops[i];
        let b = stops[i + 1];
        gpui::Rgba {
            r: a.r + (b.r - a.r) * fraction,
            g: a.g + (b.g - a.g) * fraction,
            b: a.b + (b.b - a.b) * fraction,
            a: 1.,
        }
        .into()
    }

    fn window_shadow(&self) -> Hsla {
        Hsla {
            h: 0.,
            s: 0.,
            l: 0.,
            a: 0.4,
        }
    }

    fn plot_gap_band(&self) -> Hsla {
        Self::dim(self.zero_line_color, 0.12)
    }

    fn selection_ring(&self) -> Hsla {
        Hsla {
            a: 1.0,
            ..self.drop_target
        }
    }

    fn scrub_fill(&self) -> Hsla {
        Self::dim(self.text_primary, 0.2)
    }

    fn plot_chrome_bg(&self) -> Hsla {
        Hsla {
            a: 0.7,
            ..self.bg_secondary
        }
    }

    fn stale_text(&self) -> Hsla {
        self.alarm_color(0)
    }

    fn stale_bg(&self) -> Hsla {
        Hsla {
            a: 0.12,
            ..self.stale_text()
        }
    }

    fn drop_accent(&self) -> Hsla {
        hex(0x3d8bfd, 1.0)
    }

    fn drop_fill(&self) -> Hsla {
        Hsla {
            a: 0.18,
            ..self.drop_accent()
        }
    }

    fn frame_edge_color(&self) -> Hsla {
        self.line_colors[1]
    }

    fn map_marker(&self) -> Hsla {
        self.line_colors[0]
    }

    fn map_trail(&self) -> Hsla {
        self.line_colors[1]
    }

    fn image_backdrop(&self) -> Hsla {
        hex(0x000000, 1.0)
    }

    fn image_saturated(&self) -> Hsla {
        self.error_accent
    }

    fn image_overlay_detected(&self) -> Hsla {
        self.line_colors[2]
    }

    fn image_overlay_truth(&self) -> Hsla {
        self.line_colors[4]
    }

    fn alarm_color(&self, severity_index: usize) -> Hsla {
        match severity_index {
            2 => self.error_accent,
            1 => hex(0xe0a030, 1.0),
            _ => hex(0x4090e0, 1.0),
        }
    }

    fn alarm_tint(&self, severity_index: usize) -> Hsla {
        Hsla {
            a: 0.10,
            ..self.alarm_color(severity_index)
        }
    }

    fn gradient_fill(&self, palette: Palette, t: f32) -> Hsla {
        const MAX_TINT: f32 = 0.6;
        let m = t.abs().min(1.0);
        let hue = match palette {
            _ if t < 0.0 && palette != Palette::Heat => self.error_accent,
            Palette::Status => self.control_active,
            Palette::Series(i) => self.line_colors[usize::from(i) % self.line_colors.len()],
            Palette::Heat => {
                // The intensity ramp carries its own lightness, so walk it
                // instead of fading one hue.
                let at = m * (self.density_stops.len() - 1) as f32;
                let i = (at as usize).min(self.density_stops.len() - 2);
                let ramp = lerp_color(
                    self.density_stops[i],
                    self.density_stops[i + 1],
                    at - i as f32,
                );
                return lerp_color(self.bg_secondary, ramp, (m * 1.5).min(1.0) * MAX_TINT);
            }
        };
        lerp_color(self.bg_secondary, hue, m * MAX_TINT)
    }

    fn gradient_preview(&self, palette: Palette) -> [Hsla; 4] {
        [0.25, 0.5, 0.75, 1.0].map(|t| self.gradient_fill(palette, t))
    }

    fn run_state_color(&self, state: Option<SequenceChannelState>) -> Hsla {
        match state {
            Some(SequenceChannelState::Running) => self.control_active,
            // Calm blue: the run finished.
            Some(SequenceChannelState::Completed) => hex(0x4090e0, 1.0),
            // Amber: a commanded safe stop, or waiting on one to settle.
            Some(SequenceChannelState::Aborted | SequenceChannelState::Quiescing) => {
                hex(0xe0a030, 1.0)
            }
            // Orange: hard-stopped, possibly unsafe.
            Some(SequenceChannelState::Stopped) => hex(0xe06820, 1.0),
            Some(SequenceChannelState::Failed | SequenceChannelState::Panicked) => {
                self.error_accent
            }
            Some(SequenceChannelState::Unloaded) | None => self.text_tertiary,
        }
    }

    fn run_state_tint(&self, state: Option<SequenceChannelState>) -> Hsla {
        Hsla {
            a: 0.12,
            ..self.run_state_color(state)
        }
    }

    fn slot_state_color(&self, state_code: usize) -> Hsla {
        match state_code {
            1 => self.text_secondary, // loaded — built, not polling
            2 => hex(0xe0a030, 1.0),  // loading — amber, mid-pipeline
            3 => self.control_active, // running
            4 => hex(0x4090e0, 1.0),  // done — calm blue
            5 => hex(0xe06820, 1.0),  // stopped — orange (hard stop)
            _ => self.text_tertiary,  // empty / unknown
        }
    }

    fn slot_state_tint(&self, state_code: usize) -> Hsla {
        Hsla {
            a: 0.12,
            ..self.slot_state_color(state_code)
        }
    }

    fn density_color(&self, t: f32) -> Hsla {
        let last = self.density_stops.len() - 1;
        let pos = t.clamp(0.0, 1.0) * last as f32;
        let i = (pos.floor() as usize).min(last - 1);
        lerp_color(
            self.density_stops[i],
            self.density_stops[i + 1],
            pos - i as f32,
        )
    }
}

/// Family the [`FontConfig::Auto`] mode reaches for first, when the system has it installed.
/// Falls back to [`BUNDLED_FAMILY`] otherwise.
pub const PREFERRED_FAMILY: &str = "Berkeley Mono";

/// Resolve a [`FontConfig`] to a concrete family the text system can render.
///
/// Both modes degrade to [`BUNDLED_FAMILY`] when their preferred family isn't available, so the
/// UI never ends up with an unresolvable font. Must be called after
/// [`metor_gpui::register_fonts`] so the bundled family appears in the system list.
pub fn resolve_font_family(cx: &App, font: &FontConfig) -> SharedString {
    let available = cx.text_system().all_font_names();
    let has = |name: &str| available.iter().any(|f| f == name);

    match font {
        FontConfig::Auto => {
            if has(PREFERRED_FAMILY) {
                SharedString::new_static(PREFERRED_FAMILY)
            } else {
                SharedString::new_static(BUNDLED_FAMILY)
            }
        }
        FontConfig::Family(name) => {
            if has(name) {
                SharedString::from(name.clone())
            } else {
                tracing::warn!("font '{name}' not found; using bundled {BUNDLED_FAMILY}");
                SharedString::new_static(BUNDLED_FAMILY)
            }
        }
    }
}

/// Apply a new font choice: resolve it, update [`UiSettings`], and persist it so the choice
/// survives a restart. The palette dismissal re-renders the root, which picks up the new family.
pub fn set_font(cx: &mut App, font: FontConfig) {
    let family = resolve_font_family(cx, &font);
    cx.default_global::<UiSettings>().font_family = family;
    let config = cx.global_mut::<PanelConfig>();
    config.font = font;
    if let Err(e) = config::save(config) {
        tracing::error!(%e, "save config failed");
    }
}

#[cfg(test)]
mod tests {
    use metor_gpui::builtin_themes;

    use super::*;

    fn approx(a: Hsla, b: Hsla) -> bool {
        (a.h - b.h).abs() < 1e-5
            && (a.s - b.s).abs() < 1e-5
            && (a.l - b.l).abs() < 1e-5
            && (a.a - b.a).abs() < 1e-5
    }

    /// The ramp's ends are the tables' end stops: a colorbar labelled with the
    /// intensity extremes has to paint the colors those extremes map to.
    #[test]
    fn density_color_hits_the_stops() {
        for theme in builtin_themes() {
            let stops = &theme.density_stops;
            assert!(
                approx(theme.density_color(0.0), stops[0]),
                "{} low end",
                theme.name
            );
            assert!(
                approx(theme.density_color(1.0), stops[4]),
                "{} high end",
                theme.name
            );
            assert!(
                approx(theme.density_color(0.5), stops[2]),
                "{} midpoint",
                theme.name
            );
            // Out-of-range intensity clamps rather than extrapolating off the
            // ramp into a color the palette never chose.
            assert!(approx(theme.density_color(-1.0), stops[0]));
            assert!(approx(theme.density_color(2.0), stops[4]));
        }
    }

    /// Intensity has to read as intensity even in greyscale, so every ramp
    /// moves steadily away from its background rather than wandering.
    #[test]
    fn density_stops_are_monotonic_in_lightness() {
        for theme in builtin_themes() {
            let l: Vec<f32> = theme.density_stops.iter().map(|c| c.l).collect();
            let rising = l.windows(2).all(|w| w[0] < w[1]);
            let falling = l.windows(2).all(|w| w[0] > w[1]);
            assert!(rising || falling, "{} stops: {l:?}", theme.name);
        }
    }
}
