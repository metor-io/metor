//! Star-field frames: [`Scene::prepare`] selects the stars, projects each one
//! in `f64` at every motion-blur sub-step into a [`Splat`] and lists the
//! truth; the GPU renderer ([`gpu`]) does the PSF, noise and ADC.

#[cfg(feature = "gpu")]
pub mod gpu;
pub mod noise;
pub mod photometry;
pub mod scene;
#[cfg(any(test, feature = "testkit"))]
pub mod testkit;
pub mod truth;

use alloc::vec::Vec;
use core::f32::consts::SQRT_2;

use zerocopy::{Immutable, IntoBytes};

pub use noise::Exposure;
pub use photometry::Photometry;
pub use scene::{Frame, HotPixels, Motion, Sensor};
pub use truth::{Truth, TruthStar};

use crate::camera::{Camera, EDGE_MARGIN_PX, aberrate};
use crate::geom::{V3, mat_vec, norm};
use crate::starcat::{Star, StarCat};
use truth::Detector;

/// The most electrons one star may put on the sensor in a frame, 2²², so
/// one star adds at most 2³⁰ accumulator counts to a pixel.
pub const MAX_STAR_E: f32 = 4_194_304.0;
/// The widest PSF σ (px) the GPU's 13-tap kernel holds.
pub const MAX_SIGMA: f32 = 1.5;
const SPLAT_CAPACITY: usize = 1 << 15;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderError {
    /// The camera's size is not the image's, or `bits` is not in `1..=16`.
    BadShape,
    /// `psf_sigma_px` is not in `(0, MAX_SIGMA]`.
    BadPsf,
    BadGain,
    /// A sensor value is negative or not finite.
    BadSensor,
    BadPhotometry,
    /// The hot-pixel map is not one value per pixel.
    BadHotPixels,
    /// The attitude, rate, velocity or exposure time is not usable.
    BadMotion,
    /// `max_sub` is not in `1..=MAX_SUB`.
    BadSubsteps,
    /// The output slice is not one value per pixel.
    BadOutput,
}

/// The output image's size and ADC depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shape {
    pub width: u32,
    pub height: u32,
    pub bits: u32,
}

impl Shape {
    pub fn pixels(&self) -> usize {
        self.width as usize * self.height as usize
    }

    pub fn max_dn(&self) -> u16 {
        ((1u32 << self.bits) - 1) as u16
    }
}

/// One star at one motion-blur sub-step, as the GPU reads it.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, IntoBytes, Immutable)]
pub struct Splat {
    /// Pixel position (px).
    pub u: f32,
    pub v: f32,
    /// Electrons over the sub-step.
    pub e: f32,
    /// The star's index among this frame's splatted stars; a star's splats
    /// are contiguous.
    pub star: u32,
}

/// One catalog seen through one camera and sensor. Every buffer is allocated
/// in [`Scene::new`].
#[derive(Debug)]
pub struct Scene<'a> {
    cat: StarCat<'a>,
    camera: Camera,
    phot: Photometry,
    sensor: Sensor,
    shape: Shape,
    hot: HotPixels,
    mag_limit: f32,
    /// Covers the sensor, its PSF margin and the aberration shift.
    cone_rad: f64,
    pixel_sr: f64,
    cells: Vec<u32>,
    splats: Vec<Splat>,
}

impl<'a> Scene<'a> {
    /// Stars with G at or above `mag_limit` are skipped.
    pub fn new(
        cat: StarCat<'a>,
        camera: Camera,
        phot: Photometry,
        sensor: Sensor,
        shape: Shape,
        hot: HotPixels,
        mag_limit: f32,
    ) -> Result<Self, RenderError> {
        let intr = &camera.intr;
        if (intr.width, intr.height) != (shape.width, shape.height)
            || !(1..=16).contains(&shape.bits)
        {
            return Err(RenderError::BadShape);
        }
        if !(sensor.psf_sigma_px > 0.0 && sensor.psf_sigma_px <= MAX_SIGMA) {
            return Err(RenderError::BadPsf);
        }
        if sensor.gain_milli == 0 {
            return Err(RenderError::BadGain);
        }
        if !sensor.is_valid() {
            return Err(RenderError::BadSensor);
        }
        if !phot.is_valid() {
            return Err(RenderError::BadPhotometry);
        }
        if hot.as_slice().len() != shape.pixels() {
            return Err(RenderError::BadHotPixels);
        }
        let px_rad = intr.pitch_m / intr.f_m;
        Ok(Scene {
            cat,
            camera,
            phot,
            sensor,
            shape,
            hot,
            mag_limit,
            cone_rad: camera.half_diagonal_rad() + 2.0 * EDGE_MARGIN_PX * px_rad + 1e-3,
            pixel_sr: px_rad * px_rad,
            cells: Vec::with_capacity(cat.cells().len()),
            splats: Vec::with_capacity(SPLAT_CAPACITY),
        })
    }

    /// Selects and projects `frame`'s stars into [`splats`](Self::splats),
    /// and returns the truth with `n_rendered` left at 0.
    pub fn prepare(&mut self, frame: &Frame) -> Truth {
        let reach = self.cone_rad + frame.half_sweep() + norm(frame.beta());
        self.cells.clear();
        self.cells
            .extend(self.cat.cells_in_cone(frame.boresight_eci(), reach));
        self.project(frame);
        self.truth(frame)
    }

    /// The last prepared frame's splats.
    pub fn splats(&self) -> &[Splat] {
        &self.splats
    }

    pub fn sensor(&self) -> &Sensor {
        &self.sensor
    }

    pub fn hot(&self) -> &HotPixels {
        &self.hot
    }

    pub fn shape(&self) -> Shape {
        self.shape
    }

    pub fn exposure(&self, frame: &Frame) -> Exposure {
        let t = frame.t_exp() as f32;
        let s = &self.sensor;
        let sky = |m| self.phot.background_e(m, self.pixel_sr, t);
        Exposure {
            background_e: s.zodi_mag_arcsec2.map_or(0.0, sky),
            dark_e: s.dark_e(t),
            read_noise_e: s.read_noise_e,
            full_well_e: s.full_well_e,
            gain_milli: s.gain_milli,
            bias_dn: s.bias_dn,
            max_dn: self.shape.max_dn(),
            seed: frame.seed(),
        }
    }

    /// Projects every selected star at each sub-step. A star that lands on
    /// no sub-step is skipped.
    fn project(&mut self, frame: &Frame) {
        self.splats.clear();
        let stars = catalog_stars(&self.cells, self.cat, &self.phot, self.mag_limit, frame);
        let n_sub = frame.substeps().len() as f32;
        let mut star = 0;
        for (_, dir, _, e) in stars {
            let e = e.min(MAX_STAR_E) / n_sub;
            let before = self.splats.len();
            for m in frame.substeps() {
                if let Some((u, v)) = self.camera.st_to_pixel(mat_vec(*m, dir)) {
                    let (u, v) = (u as f32, v as f32);
                    self.splats.push(Splat { u, v, e, star });
                }
            }
            star += u32::from(self.splats.len() > before);
        }
    }

    /// The stars on the sensor at mid-exposure, without `n_rendered`.
    fn truth(&self, frame: &Frame) -> Truth {
        let detector = self.detector(frame);
        let (w, h) = (f64::from(self.shape.width), f64::from(self.shape.height));
        let mut truth = Truth::empty();
        for (row, dir, mag, e) in
            catalog_stars(&self.cells, self.cat, &self.phot, self.mag_limit, frame)
        {
            let Some((u, v)) = self.camera.st_to_pixel(mat_vec(frame.r_st_eci(), dir)) else {
                continue;
            };
            if !((0.0..w).contains(&u) && (0.0..h).contains(&v)) {
                continue;
            }
            truth.n_detectable += u32::from(detector.detects(e));
            let cat_id = self.cat.hip(row).unwrap_or(u32::MAX);
            truth.offer(TruthStar { u, v, mag, cat_id });
        }
        truth
    }

    fn detector(&self, frame: &Frame) -> Detector {
        let exp = self.exposure(frame);
        let sigma = self.sensor.psf_sigma_px;
        // The peak pixel's share of a pixel-centred star, spread along a
        // streak.
        let edge = libm::erff(0.5 / (SQRT_2 * sigma));
        let streak = frame.streak_px();
        let blur = (f64::from(sigma) * libm::sqrt(core::f64::consts::TAU) / streak).min(1.0);
        Detector {
            peak_fraction: edge * edge * blur as f32,
            noise_var: exp.background_e + exp.dark_e + exp.read_noise_e * exp.read_noise_e,
        }
    }
}

/// Each star of `cells` brighter than `mag_limit`, as its row, its ECI
/// direction after aberration, its band magnitude and its electrons.
fn catalog_stars<'s>(
    cells: &'s [u32],
    cat: StarCat<'s>,
    phot: &'s Photometry,
    mag_limit: f32,
    frame: &Frame,
) -> impl Iterator<Item = (u32, V3, f32, f32)> + 's {
    let (t_exp, beta) = (frame.t_exp() as f32, frame.beta());
    cells.iter().flat_map(move |&cell| {
        let first = cat.cells().get(cell as usize).map_or(0, |c| c.offset.get());
        let stars = (first..).zip(cat.brighter_than(cell, mag_limit));
        stars.filter_map(move |(row, star)| {
            let mag = phot.band_mag(star.g(), star.bp_rp());
            let dir = apparent(star, beta)?;
            Some((row, dir, mag, phot.electrons(mag, t_exp)))
        })
    })
}

/// The star's direction as seen at `beta`; the stored one when `beta` is 0.
fn apparent(star: &Star, beta: V3) -> Option<V3> {
    let dir = star.dir().map(f64::from);
    if beta == [0.0; 3] {
        Some(dir)
    } else {
        aberrate(dir, beta)
    }
}

/// The example's 20° tracker: 16 mm at 5.5 µm on a 1024² sensor, mounted
/// along the body axes.
pub const SIM_CAMERA: Camera = Camera {
    intr: crate::camera::Intrinsics {
        f_m: 16e-3,
        pitch_m: 5.5e-6,
        cx: 512.0,
        cy: 512.0,
        k1: 0.0,
        k2: 0.0,
        p1: 0.0,
        p2: 0.0,
        width: 1024,
        height: 1024,
    },
    q_b_st: crate::geom::Quat::IDENTITY,
};
/// [`SIM_CAMERA`]'s image, 12 bit.
pub const SIM_SHAPE: Shape = Shape {
    width: 1024,
    height: 1024,
    bits: 12,
};
/// A 25 mm aperture in the V band.
pub const SIM_PHOTOMETRY: Photometry = Photometry {
    flux0_ph_s_m2: photometry::V0_PH_S_M2,
    aperture_m2: core::f32::consts::PI * 0.0125 * 0.0125,
    throughput: 0.5,
    colour: photometry::G_TO_V,
};
pub const SIM_SENSOR: Sensor = Sensor {
    psf_sigma_px: 1.0,
    full_well_e: 20_000.0,
    read_noise_e: 5.0,
    dark_e_s: 10.0,
    dark_t0_c: 20.0,
    temp_c: 20.0,
    gain_milli: 5_000,
    bias_dn: 100.0,
    zodi_mag_arcsec2: Some(22.5),
};

#[cfg(test)]
mod tests;
