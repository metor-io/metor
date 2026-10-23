//! Star magnitudes to electrons.

use core::f32::consts::LN_10;

/// `V − G` as ascending powers of `BP − RP` (Riello et al. 2021, Table C.2),
/// the default band's colour term. The same values as the catalog tool's
/// `photometry.G_TO_V`.
pub const G_TO_V: [f32; 4] = [0.02704, -0.01424, 0.2156, -0.01426];
/// The photon flux of a V = 0 star over the V band (ph/s/m²).
pub const V0_PH_S_M2: f32 = 8.8e9;
const ARCSEC_RAD: f64 = core::f64::consts::PI / 648_000.0;
/// One square arcsecond (sr).
pub const ARCSEC2_SR: f64 = ARCSEC_RAD * ARCSEC_RAD;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Photometry {
    /// Photon flux of a band magnitude 0 star (ph/s/m²).
    pub flux0_ph_s_m2: f32,
    /// Collecting area (m²).
    pub aperture_m2: f32,
    /// Optics transmission times quantum efficiency.
    pub throughput: f32,
    /// `m_band − G` as ascending powers of `BP − RP`.
    pub colour: [f32; 4],
}

impl Photometry {
    /// The band magnitude of a star of `g` and `bp_rp`.
    pub fn band_mag(&self, g: f32, bp_rp: f32) -> f32 {
        let c = &self.colour;
        g + c[0] + bp_rp * (c[1] + bp_rp * (c[2] + bp_rp * c[3]))
    }

    /// Electrons collected from a star of band magnitude `m` in `t_exp` (s).
    pub fn electrons(&self, m: f32, t_exp: f32) -> f32 {
        let gain = self.flux0_ph_s_m2 * self.aperture_m2 * self.throughput * t_exp;
        gain * libm::expf(-0.4 * LN_10 * m)
    }

    /// Electrons per pixel from a sky of `mag_arcsec2` over a pixel of
    /// `pixel_sr`.
    pub fn background_e(&self, mag_arcsec2: f32, pixel_sr: f64, t_exp: f32) -> f32 {
        self.electrons(mag_arcsec2, t_exp) * (pixel_sr / ARCSEC2_SR) as f32
    }

    pub(super) fn is_valid(&self) -> bool {
        let positive = [self.flux0_ph_s_m2, self.aperture_m2, self.throughput];
        positive.iter().all(|x| x.is_finite() && *x > 0.0)
            && self.colour.iter().all(|c| c.is_finite())
    }
}
