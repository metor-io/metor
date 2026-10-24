//! The star simulator's state: a GPU renderer over one catalog, and the
//! records it fills.

use adcs_contracts::{BodyState, MAX_TRUTH, NO_CAT_ID, StarImage, StarPixels, StarTruth};
use metor_fsw_3::{FrameArray, MakeError, ParamError, Rng, Timestamp};
use metor_star::camera::{Camera, Intrinsics};
use metor_star::geom::{self, V3};
use metor_star::render::{
    Frame, HotPixels, Motion, Photometry, RenderError, Renderer, Sensor, Shape, Truth,
};
use metor_star::starcat::StarCat;
use nox::{Quaternion, Vector};

use crate::gpu::{GpuError, GpuRenderer};
use crate::params::StarSimParams;

/// The committed G ≤ 8 catalog.
static G8: &[u8] = include_bytes!("../../../../../libs/metor-star/data/starcat-g8.bin");
/// Speed of light (m/s).
const C_M_S: f64 = 299_792_458.0;
/// How early (µs) a frame may fall due, so a 120 Hz clock keeps a 10 Hz rate.
const RATE_SLACK_US: i64 = 500;
/// `StarImage`'s pixel depth.
const BITS: u32 = 12;

pub struct StarSim {
    params: StarSimParams,
    camera: Camera,
    renderer: GpuRenderer<'static>,
    image: Box<StarImage>,
    truth: StarTruth,
    next_frame_id: u64,
    next_due: Option<i64>,
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
        let camera = camera(&params)?;
        let shape = shape();
        let hot = HotPixels::new(
            shape.pixels(),
            params.hot_pixel_fraction,
            params.hot_pixel_max,
            params.seed,
        );
        let scene = Renderer::new(
            cat,
            camera,
            photometry(&params),
            sensor(&params),
            shape,
            hot,
            params.mag_limit,
        )
        .map_err(|e| invalid(format!("{e:?}")))?;
        let renderer = GpuRenderer::new(scene).map_err(|e| MakeError::Resource {
            id: "gpu".into(),
            message: format!("{e:?}"),
        })?;
        let image = StarImage::new_box().map_err(|_| invalid("no memory for the image"))?;
        Ok(StarSim {
            params,
            camera,
            renderer,
            image,
            truth: StarTruth::default(),
            next_frame_id: 0,
            next_due: None,
        })
    }

    /// Whether an exposure starts at `t`, and if so marks it taken.
    pub fn due(&mut self, t: Timestamp) -> bool {
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

    /// Renders the next frame, exposed from `body`'s time and attitude.
    pub fn expose(&mut self, body: &BodyState) -> Result<(), GpuError> {
        let frame_id = self.next_frame_id;
        self.next_frame_id += 1;
        self.render(body, frame_id)
    }

    /// Renders frame `frame_id` exposed from `body`, into [`image`](Self::image)
    /// and [`truth`](Self::truth), both stamped at mid-exposure.
    pub fn render(&mut self, body: &BodyState, frame_id: u64) -> Result<(), GpuError> {
        let frame = self.frame(body, frame_id)?;
        let truth = self
            .renderer
            .render(&frame, self.image.pixels.pixels_mut())?;
        let exposure_us = (self.params.exposure_s * 1e6).round() as i64;
        let mid = Timestamp(body.timestamp.0 + exposure_us / 2);
        let image = &mut *self.image;
        image.timestamp = mid;
        image.frame_id = frame_id;
        image.exposure_us = exposure_us as u32;
        image.gain_milli = self.params.sensor().gain_milli;
        self.truth = truth_record(&frame, &truth, mid);
        Ok(())
    }

    fn frame(&self, body: &BodyState, frame_id: u64) -> Result<Frame, RenderError> {
        let beta = if self.params.aberration {
            geom::scale(body.vel_eci.into_buf(), 1.0 / C_M_S)
        } else {
            [0.0; 3]
        };
        let motion = Motion {
            q_b_eci: geom::Quat(body.q_b_eci.0.into_buf()),
            omega_b: body.omega_b.into_buf(),
            beta,
            t_exp: self.params.exposure_s,
        };
        // Spread the frame id over all 64 bits so that nearby (seed, frame)
        // pairs never share a noise seed the way `seed ^ frame_id` did.
        let mixed = self.params.seed ^ frame_id.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let seed = Rng::seeded(mixed, "star_sim").next_u64();
        let max_sub = self.params.max_substeps as usize;
        Frame::new(&self.camera, &motion, max_sub, seed, frame_id)
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

fn camera(params: &StarSimParams) -> Result<Camera, MakeError> {
    let c = &params.camera();
    let q_b_st = geom::Quat(c.q_b_st)
        .normalized()
        .ok_or_else(|| invalid("`camera.q_b_st` is not a rotation"))?;
    let intr = Intrinsics {
        f_m: c.f_m,
        pitch_m: c.pitch_m,
        cx: c.cx,
        cy: c.cy,
        k1: c.k1,
        k2: c.k2,
        p1: c.p1,
        p2: c.p2,
        width: StarPixels::WIDTH as u32,
        height: StarPixels::HEIGHT as u32,
    };
    let positive = intr.f_m > 0.0 && intr.pitch_m > 0.0;
    if !positive || !(intr.f_m / intr.pitch_m).is_finite() {
        return Err(invalid(
            "`camera.f_m` and `camera.pitch_m` must be positive",
        ));
    }
    Ok(Camera { intr, q_b_st })
}

/// The record's image size and depth.
fn shape() -> Shape {
    Shape {
        width: StarPixels::WIDTH as u32,
        height: StarPixels::HEIGHT as u32,
        bits: BITS,
    }
}

fn photometry(params: &StarSimParams) -> Photometry {
    let o = &params.optics();
    let radius = 0.5 * o.aperture_m;
    Photometry {
        flux0_ph_s_m2: o.flux0_ph_s_m2,
        aperture_m2: core::f32::consts::PI * radius * radius,
        throughput: o.throughput,
        colour: o.colour,
    }
}

fn sensor(params: &StarSimParams) -> Sensor {
    let s = &params.sensor();
    Sensor {
        psf_sigma_px: s.psf_sigma_px,
        full_well_e: s.full_well_e,
        read_noise_e: s.read_noise_e,
        dark_e_s: s.dark_e_s,
        dark_t0_c: s.dark_t0_c,
        temp_c: s.temp_c,
        gain_milli: s.gain_milli,
        bias_dn: s.bias_dn,
        zodi_mag_arcsec2: s.zodi_mag_arcsec2,
    }
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
