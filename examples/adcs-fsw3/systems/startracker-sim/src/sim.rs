//! The star simulator's state: a GPU renderer over one catalog, and the
//! records it fills.

use adcs_contracts::{BodyState, MAX_TRUTH, NO_CAT_ID, StarImage, StarTruth};
use metor_fsw_3::{FrameArray, MakeError, ParamError, Rng, Timestamp};
use metor_star::geom::{self, V3};
use metor_star::render::gpu::{GpuError, GpuRenderer};
use metor_star::render::{
    Frame, HotPixels, Motion, RenderError, SIM_CAMERA, SIM_PHOTOMETRY, SIM_SENSOR, SIM_SHAPE,
    Scene, Truth,
};
use metor_star::starcat::StarCat;
use metor_star::tracker::predict;
use nox::{Quaternion, Vector};

use crate::params::StarSimParams;

/// The committed G ≤ 8 catalog.
static G8: &[u8] = include_bytes!("../../../../../libs/metor-star/data/starcat-g8.bin");
/// Speed of light (m/s).
const C_M_S: f64 = 299_792_458.0;
/// How early (µs) a frame may fall due, so a 120 Hz clock keeps a 10 Hz rate.
const RATE_SLACK_US: i64 = 500;
/// Stars at or fainter than this G are skipped.
const MAG_LIMIT: f32 = 12.0;
/// The share of pixels with extra dark current, and the largest multiplier.
const HOT_PIXELS: (f32, f32) = (1e-4, 50.0);
/// The most motion-blur sub-steps per frame.
const MAX_SUBSTEPS: usize = 16;

pub struct StarSim {
    params: StarSimParams,
    renderer: GpuRenderer<'static>,
    image: Box<StarImage>,
    truth: StarTruth,
    next_frame_id: u64,
    next_due: Option<i64>,
    /// The start (µs) of the exposure waiting for its mid-exposure state.
    exposing: Option<i64>,
}

impl StarSim {
    /// Loads the catalog, opens the GPU and allocates every buffer.
    pub fn new(params: StarSimParams) -> Result<StarSim, MakeError> {
        let ok_rate = params.rate_hz > 0.0 && params.rate_hz.is_finite();
        let ok_exposure = params.exposure_s >= 0.0 && params.exposure_s * params.rate_hz <= 1.0;
        if !ok_rate || !ok_exposure {
            return Err(invalid(
                "`exposure_s` must fit in one frame at a positive `rate_hz`",
            ));
        }
        let cat = StarCat::parse(catalog_bytes(params.catalog.as_deref())?)
            .map_err(|e| resource(format!("the catalog does not parse: {e:?}")))?;
        let (fraction, max) = HOT_PIXELS;
        let hot = HotPixels::new(SIM_SHAPE.pixels(), fraction, max, params.seed);
        let (phot, sensor) = (SIM_PHOTOMETRY, SIM_SENSOR);
        let scene = Scene::new(cat, SIM_CAMERA, phot, sensor, SIM_SHAPE, hot, MAG_LIMIT)
            .map_err(|e| invalid(format!("{e:?}")))?;
        let renderer = GpuRenderer::new(scene).map_err(|e| MakeError::Resource {
            id: "gpu".into(),
            message: format!("{e:?}"),
        })?;
        let image = StarImage::new_box().map_err(|_| invalid("no memory for the image"))?;
        Ok(StarSim {
            params,
            renderer,
            image,
            truth: StarTruth::default(),
            next_frame_id: 0,
            next_due: None,
            exposing: None,
        })
    }

    /// Takes the next body state. An exposure starts on the frame cadence
    /// and is rendered from the first state at or past its middle, so the
    /// image's attitude is the true attitude at its mid-exposure stamp
    /// whatever the body's rate does over the exposure (an exposure
    /// extrapolated from its start at the start's rate was off by ατ²/8,
    /// a 4σ common-mode error per frame under rate jitter); the
    /// constant-rate streak is anchored there. Returns whether a frame was
    /// rendered into [`image`](Self::image) and [`truth`](Self::truth).
    pub fn sample(&mut self, body: &BodyState) -> Result<bool, GpuError> {
        let t = body.timestamp.0;
        if self.exposing.is_none() && self.due(body.timestamp) {
            self.exposing = Some(t);
        }
        let Some(start) = self.exposing else {
            return Ok(false);
        };
        if t + RATE_SLACK_US < start + self.exposure_us() / 2 {
            return Ok(false);
        }
        self.exposing = None;
        let frame_id = self.next_frame_id;
        self.next_frame_id += 1;
        self.render(body, Timestamp(start), frame_id)?;
        Ok(true)
    }

    fn exposure_us(&self) -> i64 {
        (self.params.exposure_s * 1e6).round() as i64
    }

    /// Whether an exposure starts at `t`, and if so marks it taken.
    fn due(&mut self, t: Timestamp) -> bool {
        let due = self.next_due.is_none_or(|next| t.0 + RATE_SLACK_US >= next);
        if due {
            let period_us = (1e6 / self.params.rate_hz).round() as i64;
            let next = self.next_due.unwrap_or(t.0) + period_us;
            // After a gap, restart the cadence from `t`.
            self.next_due = Some(if next + RATE_SLACK_US <= t.0 {
                t.0 + period_us
            } else {
                next
            });
        }
        due
    }

    /// Renders frame `frame_id`, an exposure begun at `start`, from `body`'s
    /// attitude and rate, into [`image`](Self::image) and
    /// [`truth`](Self::truth), both stamped at mid-exposure.
    pub fn render(
        &mut self,
        body: &BodyState,
        start: Timestamp,
        frame_id: u64,
    ) -> Result<(), GpuError> {
        let frame = self.frame(body, start, frame_id)?;
        let truth = self
            .renderer
            .render(&frame, self.image.pixels.pixels_mut())?;
        let exposure_us = self.exposure_us();
        let mid = Timestamp(start.0 + exposure_us / 2);
        let image = &mut *self.image;
        image.timestamp = mid;
        image.frame_id = frame_id;
        image.exposure_us = exposure_us as u32;
        image.gain_milli = SIM_SENSOR.gain_milli;
        self.truth = truth_record(&frame, &truth, mid);
        Ok(())
    }

    fn frame(
        &self,
        body: &BodyState,
        start: Timestamp,
        frame_id: u64,
    ) -> Result<Frame, RenderError> {
        let beta = if self.params.aberration {
            geom::scale(body.vel_eci.into_buf(), 1.0 / C_M_S)
        } else {
            [0.0; 3]
        };
        let omega_b = body.omega_b.into_buf();
        // `Motion` holds the attitude at the exposure's start: `body`'s,
        // turned back at its rate. The frame turns it forward again at the
        // same rate, so at `body`'s time, mid-exposure, it is `body`'s.
        let back = (start.0 - body.timestamp.0) as f64 * 1e-6;
        let motion = Motion {
            q_b_eci: predict(geom::Quat(body.q_b_eci.0.into_buf()), omega_b, back),
            omega_b,
            beta,
            t_exp: self.params.exposure_s,
        };
        // Spread the frame id over all 64 bits so that nearby (seed, frame)
        // pairs never share a noise seed the way `seed ^ frame_id` did.
        let mixed = self.params.seed ^ frame_id.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let seed = Rng::seeded(mixed, "star_sim").next_u64();
        Frame::new(&SIM_CAMERA, &motion, MAX_SUBSTEPS, seed, frame_id)
    }

    pub fn image(&self) -> &StarImage {
        &self.image
    }

    pub fn truth(&self) -> &StarTruth {
        &self.truth
    }
}

/// `StarTruth` for one rendered frame: `q_cam_eci = q_b_eci ⊗ q_b_st⁻¹` at
/// mid-exposure, and the brightest stars' centroids.
fn truth_record(frame: &Frame, truth: &Truth, mid: Timestamp) -> StarTruth {
    let stars = truth.brightest();
    let field = |f: fn(&metor_star::render::TruthStar) -> f32| {
        FrameArray(core::array::from_fn(|i| stars.get(i).map_or(0.0, f)))
    };
    let cat_id = core::array::from_fn(|i| stars.get(i).map_or(NO_CAT_ID, |s| s.cat_id));
    let mut record = StarTruth::default();
    record.timestamp = mid;
    record.frame_id = frame.frame_id();
    record.q_cam_eci = Quaternion(Vector::from_buf(frame.q_cam_eci().0));
    record.omega_cam = to_v3(frame.omega_st());
    record.n_rendered = truth.n_rendered;
    record.n_detectable = truth.n_detectable;
    record.count = stars.len().min(MAX_TRUTH) as u64;
    record.u = field(|s| s.u as f32);
    record.v = field(|s| s.v as f32);
    record.mag = field(|s| s.mag);
    record.cat_id = FrameArray(cat_id);
    record
}

fn to_v3(v: V3) -> adcs_contracts::V3 {
    Vector::from_buf(v)
}

/// The catalog's bytes: the committed G ≤ 8 file, or `path` read once and
/// kept for the life of the process.
fn catalog_bytes(path: Option<&str>) -> Result<&'static [u8], MakeError> {
    let Some(path) = path else {
        return Ok(G8);
    };
    let bytes = std::fs::read(path).map_err(|e| resource(format!("reading `{path}`: {e}")))?;
    Ok(Vec::leak(bytes))
}

fn invalid(message: impl Into<String>) -> MakeError {
    MakeError::Params(ParamError::Decode(message.into()))
}

fn resource(message: String) -> MakeError {
    MakeError::Resource {
        id: "catalog".into(),
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> StarSimParams {
        StarSimParams {
            rate_hz: 10.0,
            exposure_s: 0.1,
            seed: 0,
            catalog: None,
            aberration: false,
        }
    }

    fn body(t_us: i64, q: geom::Quat, omega_b: V3) -> BodyState {
        BodyState {
            timestamp: Timestamp(t_us),
            q_b_eci: Quaternion(Vector::from_buf(q.0)),
            omega_b: Vector::from_buf(omega_b),
            pos_eci: Vector::zeros(),
            vel_eci: Vector::zeros(),
        }
    }

    /// A body whose rate jitters every cycle (as a rate loop fed by a noisy
    /// gyro does): each frame's truth is the attitude at its mid-exposure
    /// stamp, which an exposure extrapolated from its start would miss.
    #[test]
    fn a_frame_shows_the_attitude_at_its_stamp() {
        let mut sim = StarSim::new(params()).expect("a GPU");
        let dt_us = 8_333i64;
        let dt = dt_us as f64 * 1e-6;
        let mut q = geom::Quat([0.1, -0.5, 0.3, 0.8]).normalized().unwrap();
        let mut states: Vec<(i64, geom::Quat, V3)> = Vec::new();
        let mut frames = Vec::new();
        for k in 0..250i64 {
            // 0.6°/s about one axis, ±0.3°/s about another, flipping each cycle.
            let jitter = if k % 2 == 0 { 5e-3 } else { -5e-3 };
            let omega = [1e-2, jitter, 0.0];
            let t = k * dt_us;
            states.push((t, q, omega));
            if sim.sample(&body(t, q, omega)).unwrap() {
                let image = sim.image();
                let truth = sim.truth();
                assert_eq!(truth.timestamp, image.timestamp);
                assert_eq!(truth.frame_id, image.frame_id);
                frames.push((image.timestamp.0, geom::Quat(truth.q_cam_eci.0.into_buf())));
            }
            q = predict(q, omega, dt);
        }
        // 250 cycles of 8333 µs are 2.08 s: 21 starts on the 100 ms cadence
        // (the cadence's slack absorbs the 4 µs a cycle falls short), each
        // stamped 50 ms after its start.
        assert_eq!(frames.len(), 21, "{frames:?}");
        let first_at = |t: i64| *states.iter().find(|s| s.0 + RATE_SLACK_US >= t).unwrap();
        for (i, (stamp, q_cam)) in frames.iter().enumerate() {
            let (start, q_start, omega_start) = first_at(i as i64 * 100_000);
            assert_eq!(*stamp, start + 50_000);
            // The body state the frame was rendered from is within the
            // cadence slack of the stamp; the truth is that attitude turned
            // the rest of the way.
            let (t, q_mid, omega) = first_at(*stamp);
            let expected = predict(q_mid, omega, (*stamp - t) as f64 * 1e-6);
            let err = q_cam.angle_to(expected);
            assert!(err < 1e-9, "frame {i}: {err:.2e} rad off its stamp");
            // Extrapolating from the start at the start's rate, as before,
            // misses the jitter's integral.
            let extrapolated = predict(q_start, omega_start, 0.05);
            assert!(extrapolated.angle_to(expected) > 1e-4, "frame {i}");
        }
    }
}
