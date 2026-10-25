//! One exposure's geometry, and the sensor it is read out by.

use alloc::vec::Vec;

use super::RenderError;
use crate::camera::Camera;
use crate::geom::{I3, M3, Quat, V3, mat_add, mat_mul, mat_scale, norm, scale, transpose};

/// The most motion-blur sub-steps a frame may take.
pub const MAX_SUB: usize = 32;
/// The streak length (px) one sub-step may cover.
const SUBSTEP_PX: f64 = 0.5;

/// The body's state over one exposure, which starts at `q_b_eci`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Motion {
    /// Body←ECI, `b = A(q) r`, at the start of the exposure.
    pub q_b_eci: Quat,
    /// Body rate in the body frame (rad/s), constant over the exposure.
    pub omega_b: V3,
    /// Observer velocity over c, in ECI, for aberration. Zero turns it off.
    pub beta: V3,
    /// Exposure time (s).
    pub t_exp: f64,
}

/// One exposure: the sensor←ECI rotation at mid-exposure and at the centre
/// of each motion-blur sub-step. Built only by [`Frame::new`].
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    r_st_eci: M3,
    substeps: [M3; MAX_SUB],
    n_sub: usize,
    beta: V3,
    t_exp: f64,
    omega_st: V3,
    streak_px: f64,
    seed: u64,
    frame_id: u64,
}

impl Frame {
    /// Sub-steps are spaced so each covers at most `SUBSTEP_PX` of streak,
    /// up to `max_sub`. `seed` keys this frame's noise.
    pub fn new(
        camera: &Camera,
        motion: &Motion,
        max_sub: usize,
        seed: u64,
        frame_id: u64,
    ) -> Result<Frame, RenderError> {
        if !(1..=MAX_SUB).contains(&max_sub) {
            return Err(RenderError::BadSubsteps);
        }
        let q = motion.q_b_eci.normalized().ok_or(RenderError::BadMotion)?;
        let t_exp = motion.t_exp;
        let finite = [motion.omega_b, motion.beta]
            .as_flattened()
            .iter()
            .all(|x| x.is_finite());
        if !finite || !(t_exp >= 0.0 && t_exp.is_finite()) {
            return Err(RenderError::BadMotion);
        }
        let streak_px = norm(motion.omega_b) * t_exp * camera.intr.f_m / camera.intr.pitch_m;
        let steps = libm::ceil(streak_px / SUBSTEP_PX).min(MAX_SUB as f64);
        let n_sub = (steps as usize).clamp(1, max_sub);
        let a_st_b = transpose(camera.q_b_st.to_m3());
        let at = |t: f64| mat_mul(a_st_b, mat_mul(turn(motion.omega_b, t), q.to_m3()));
        let substeps = core::array::from_fn(|k| {
            let t = (k as f64 + 0.5) / n_sub as f64 * t_exp;
            if k < n_sub { at(t) } else { I3 }
        });
        Ok(Frame {
            r_st_eci: at(0.5 * t_exp),
            substeps,
            n_sub,
            beta: motion.beta,
            t_exp,
            omega_st: camera.body_to_st(motion.omega_b),
            streak_px,
            seed,
            frame_id,
        })
    }

    /// Sensor←ECI at mid-exposure, as a matrix: `A(q_b_st)ᵀ A(q_b_eci)`.
    pub fn r_st_eci(&self) -> M3 {
        self.r_st_eci
    }

    /// Sensor←ECI at mid-exposure: `q_b_eci ⊗ q_b_st⁻¹`.
    pub fn q_cam_eci(&self) -> Quat {
        Quat::from_m3(self.r_st_eci)
    }

    /// The boresight in ECI at mid-exposure.
    pub fn boresight_eci(&self) -> V3 {
        self.r_st_eci[2]
    }

    /// The angle (rad) the boresight sweeps either side of mid-exposure.
    pub fn half_sweep(&self) -> f64 {
        0.5 * norm(self.omega_st) * self.t_exp
    }

    /// The longest streak (px) a star can draw, from the body rate.
    pub fn streak_px(&self) -> f64 {
        self.streak_px
    }

    pub fn substeps(&self) -> &[M3] {
        &self.substeps[..self.n_sub]
    }

    pub fn beta(&self) -> V3 {
        self.beta
    }

    pub fn t_exp(&self) -> f64 {
        self.t_exp
    }

    /// Body rate in the star-tracker frame (rad/s).
    pub fn omega_st(&self) -> V3 {
        self.omega_st
    }

    pub fn seed(&self) -> u64 {
        self.seed
    }

    pub fn frame_id(&self) -> u64 {
        self.frame_id
    }
}

/// How a body at rate `omega` sees fixed directions turn after `t`:
/// `b(t) = R b(0)`, a rotation by `−|ω| t` about `ω`.
fn turn(omega: V3, t: f64) -> M3 {
    let w = norm(omega);
    if w * t == 0.0 {
        return I3;
    }
    let [x, y, z] = scale(omega, 1.0 / w);
    let k = [[0.0, -z, y], [z, 0.0, -x], [-y, x, 0.0]];
    let phi = -w * t;
    let k2 = mat_mul(k, k);
    mat_add(
        I3,
        mat_add(
            mat_scale(k, libm::sin(phi)),
            mat_scale(k2, 1.0 - libm::cos(phi)),
        ),
    )
}

/// The detector. Electrons become DN as `e · 1000 / gain_milli + bias_dn`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sensor {
    pub psf_sigma_px: f32,
    pub full_well_e: f32,
    pub read_noise_e: f32,
    /// Dark current at `dark_t0_c` (e⁻/s); it doubles every 6.5 °C.
    pub dark_e_s: f32,
    pub dark_t0_c: f32,
    pub temp_c: f32,
    /// Gain (milli-e⁻/DN).
    pub gain_milli: u32,
    pub bias_dn: f32,
    /// Sky background (mag/arcsec²), or none.
    pub zodi_mag_arcsec2: Option<f32>,
}

impl Sensor {
    /// Dark electrons per pixel in `t_exp`, before the hot-pixel multiplier.
    pub fn dark_e(&self, t_exp: f32) -> f32 {
        self.dark_e_s * t_exp * libm::exp2f((self.temp_c - self.dark_t0_c) / 6.5)
    }

    pub(super) fn is_valid(&self) -> bool {
        let non_negative = [
            self.full_well_e,
            self.read_noise_e,
            self.dark_e_s,
            self.bias_dn,
        ];
        let finite = [self.dark_t0_c, self.temp_c];
        non_negative.iter().all(|x| x.is_finite() && *x >= 0.0)
            && finite.iter().all(|x| x.is_finite())
            && self.zodi_mag_arcsec2.is_none_or(f32::is_finite)
    }
}

/// A per-pixel dark-current multiplier.
#[derive(Debug, Clone, PartialEq)]
pub struct HotPixels(Vec<f32>);

impl HotPixels {
    /// Every pixel at 1.
    pub fn none(len: usize) -> HotPixels {
        HotPixels(alloc::vec![1.0; len])
    }

    /// A `fraction` of pixels hot, each with a multiplier log-uniform in
    /// `[1, max_mult]`, the same for the same `seed`.
    pub fn new(len: usize, fraction: f32, max_mult: f32, seed: u64) -> HotPixels {
        let ln_max = libm::logf(max_mult.max(1.0));
        let mut state = seed;
        let mut uniform = move || {
            // SplitMix64, top 24 bits.
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            ((z ^ (z >> 31)) >> 40) as f32 / 16_777_216.0
        };
        let map = (0..len).map(|_| {
            let (hot, mult) = (uniform() < fraction, uniform());
            if hot { libm::expf(ln_max * mult) } else { 1.0 }
        });
        HotPixels(map.collect())
    }

    pub fn as_slice(&self) -> &[f32] {
        &self.0
    }
}
