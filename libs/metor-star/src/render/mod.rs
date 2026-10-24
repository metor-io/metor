//! The CPU reference star-field renderer, the test oracle for the GPU
//! simulator, and the scene setup both share.
//!
//! [`Renderer::prepare`] selects the stars, projects each one at every
//! motion-blur sub-step into a [`Splat`] and lists the truth. The CPU then
//! splats and exposes; the GPU takes the same splats.
//!
//! Directions stay in `f64` through [`Camera::st_to_pixel`]. From the pixel
//! position on, the PSF, photometry and noise run in `f32`, as the GPU does.
//!
//! # Agreement with the GPU
//!
//! The two cannot match bit for bit: WGSL bounds `exp`, `log` and the like
//! only to a few ULP, may contract `a * b + c` into an FMA, and has no `f64`.
//! A GPU backend agrees with this renderer when, for the same frame:
//! - every accumulator pixel is within ±2 counts (1 count = 1/256 e⁻),
//!   plus 2e-7 of its value for the brightest stars' peaks;
//! - each star's centre of gravity is within 1e-3 px;
//! - the noisy image matches statistically: per-pixel DN mean and variance
//!   over a flat field, not pixel by pixel.
//!
//! Integer steps (the hash, the accumulator) are exact on both.
//!
//! # Saturation
//!
//! Accumulator pixels saturate at `u32::MAX` on both sides. A star's
//! electrons are clamped to [`MAX_STAR_E`], so one star adds at most 2³⁰
//! counts to a pixel and a single add cannot wrap twice. WGSL has no
//! saturating atomic; the GPU does
//! `let old = atomicAdd(&acc[i], n); if old + n < old { atomicMax(&acc[i], 0xffffffffu); }`,
//! which leaves the pixel at `u32::MAX` once any add overflows.

pub mod hash;
pub mod noise;
pub mod photometry;
pub mod psf;
pub mod scene;
pub mod splat;
#[cfg(any(test, feature = "testkit"))]
pub mod testkit;
pub mod truth;

use alloc::vec;
use alloc::vec::Vec;

use zerocopy::{Immutable, IntoBytes};

pub use noise::Exposure;
pub use photometry::Photometry;
pub use psf::Psf;
pub use scene::{Frame, HotPixels, Motion, Sensor};
pub use truth::{Truth, TruthStar};

use crate::camera::{Camera, EDGE_MARGIN_PX, aberrate};
use crate::geom::{V3, mat_vec, norm};
use crate::starcat::{Star, StarCat};
use splat::Accumulator;
use truth::Detector;

/// The most electrons one star may put on the sensor in a frame, 2²².
pub const MAX_STAR_E: f32 = 4_194_304.0;
/// Splats the renderer holds room for before its first frame.
const SPLAT_CAPACITY: usize = 1 << 15;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderError {
    /// The camera's size is not the image's, or the image's `bits` are not
    /// in `1..=16`.
    BadShape,
    /// `psf_sigma_px` is not in `(0, 1.5]`.
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

/// One star at one motion-blur sub-step: what a splat draws.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, IntoBytes, Immutable)]
pub struct Splat {
    /// Pixel position (px).
    pub u: f32,
    pub v: f32,
    /// Electrons over the sub-step.
    pub e: f32,
    /// The star's index among this frame's splatted stars. A star's splats
    /// are contiguous.
    pub star: u32,
}

/// Renders frames of one catalog through one camera and sensor. Every buffer
/// is allocated in [`Renderer::new`].
#[derive(Debug)]
pub struct Renderer<'a> {
    cat: StarCat<'a>,
    camera: Camera,
    phot: Photometry,
    sensor: Sensor,
    shape: Shape,
    psf: Psf,
    hot: HotPixels,
    mag_limit: f32,
    /// Covers the sensor, its PSF margin and the aberration shift.
    cone_rad: f64,
    pixel_sr: f64,
    cells: Vec<u32>,
    splats: Vec<Splat>,
    acc: Vec<u32>,
}

impl<'a> Renderer<'a> {
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
        let psf = Psf::new(sensor.psf_sigma_px).ok_or(RenderError::BadPsf)?;
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
        Ok(Renderer {
            cat,
            camera,
            phot,
            sensor,
            shape,
            psf,
            hot,
            mag_limit,
            cone_rad: camera.half_diagonal_rad() + 2.0 * EDGE_MARGIN_PX * px_rad + 1e-3,
            pixel_sr: px_rad * px_rad,
            cells: Vec::with_capacity(cat.cells().len()),
            splats: Vec::with_capacity(SPLAT_CAPACITY),
            acc: vec![0; shape.pixels()],
        })
    }

    /// Renders `frame` into `out`, row-major, one DN per pixel, and returns
    /// what was drawn. Allocates nothing.
    pub fn render(&mut self, frame: &Frame, out: &mut [u16]) -> Result<Truth, RenderError> {
        if out.len() != self.shape.pixels() {
            return Err(RenderError::BadOutput);
        }
        let mut truth = self.prepare(frame);
        self.acc.fill(0);
        truth.n_rendered = self.splat();
        noise::expose(&self.acc, self.hot.as_slice(), &self.exposure(frame), out);
        Ok(truth)
    }

    /// Selects and projects `frame`'s stars into [`splats`](Self::splats),
    /// and returns the truth with `n_rendered` left at 0.
    pub fn prepare(&mut self, frame: &Frame) -> Truth {
        self.select_cells(frame);
        self.project(frame);
        self.truth(frame)
    }

    /// The last prepared frame's splats.
    pub fn splats(&self) -> &[Splat] {
        &self.splats
    }

    pub fn psf(&self) -> &Psf {
        &self.psf
    }

    pub fn hot(&self) -> &HotPixels {
        &self.hot
    }

    pub fn shape(&self) -> Shape {
        self.shape
    }

    /// The last frame's accumulator, `1/FIXED_SCALE` e⁻ per count.
    pub fn acc(&self) -> &[u32] {
        &self.acc
    }

    pub fn exposure(&self, frame: &Frame) -> Exposure {
        let t = frame.t_exp() as f32;
        let s = &self.sensor;
        Exposure {
            background_e: self.background_e(t),
            dark_e: s.dark_e(t),
            read_noise_e: s.read_noise_e,
            full_well_e: s.full_well_e,
            gain_milli: s.gain_milli,
            bias_dn: s.bias_dn,
            max_dn: self.shape.max_dn(),
            seed: frame.seed(),
        }
    }

    fn background_e(&self, t_exp: f32) -> f32 {
        let zodi = self.sensor.zodi_mag_arcsec2;
        zodi.map_or(0.0, |m| self.phot.background_e(m, self.pixel_sr, t_exp))
    }

    fn select_cells(&mut self, frame: &Frame) {
        let reach = self.cone_rad + frame.half_sweep() + norm(frame.beta());
        self.cells.clear();
        self.cells
            .extend(self.cat.cells_in_cone(frame.boresight_eci(), reach));
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

    /// Splats [`splats`](Self::splats) into the accumulator. Returns how many
    /// stars put a count on the sensor.
    fn splat(&mut self) -> u32 {
        let mut acc = Accumulator {
            counts: &mut self.acc,
            width: self.shape.width as usize,
            height: self.shape.height as usize,
        };
        let (mut n, mut last_hit) = (0, None);
        for s in &self.splats {
            if acc.splat(&self.psf, s.u, s.v, s.e) && last_hit != Some(s.star) {
                last_hit = Some(s.star);
                n += 1;
            }
        }
        n
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
        let read_var = exp.read_noise_e * exp.read_noise_e;
        // A streak spreads the peak over its length.
        let sigma = f64::from(self.psf.sigma());
        let streak = frame.streak_px();
        let blur = (sigma * libm::sqrt(core::f64::consts::TAU) / streak).min(1.0) as f32;
        Detector {
            peak_fraction: self.psf.peak_fraction() * blur,
            noise_var: exp.background_e + exp.dark_e + read_var,
        }
    }
}

/// Each star of `cells` brighter than `mag_limit`, as its row, its ECI
/// direction after aberration, its band magnitude and its electrons over the
/// exposure.
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

#[cfg(test)]
mod tests;
