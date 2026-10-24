//! The Rust mirror of `star_sim.wgsl`'s `Params`.

use metor_star::render::{Exposure, Renderer};
use zerocopy::{Immutable, IntoBytes};

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, IntoBytes, Immutable)]
pub struct Params {
    pub width: u32,
    pub height: u32,
    pub n_splats: u32,
    pub k: u32,
    pub erf_scale: f32,
    pub background_e: f32,
    pub dark_e: f32,
    pub read_noise_e: f32,
    pub full_well_e: f32,
    pub gain_milli: f32,
    pub bias_dn: f32,
    pub max_dn: f32,
    pub seed_lo: u32,
    pub seed_hi: u32,
    pub use_table: u32,
    pub _pad: u32,
}

impl Params {
    pub fn new(scene: &Renderer<'_>, exp: &Exposure, n_splats: u32, use_table: bool) -> Params {
        let (shape, psf) = (scene.shape(), scene.psf());
        Params {
            width: shape.width,
            height: shape.height,
            n_splats,
            k: psf.half_width() as u32,
            erf_scale: psf.erf_scale(),
            background_e: exp.background_e,
            dark_e: exp.dark_e,
            read_noise_e: exp.read_noise_e,
            full_well_e: exp.full_well_e,
            gain_milli: exp.gain_milli as f32,
            bias_dn: exp.bias_dn,
            max_dn: f32::from(exp.max_dn),
            seed_lo: exp.seed as u32,
            seed_hi: (exp.seed >> 32) as u32,
            use_table: u32::from(use_table),
            _pad: 0,
        }
    }
}
