//! Hand-built catalogs for renderer tests.

use alloc::vec::Vec;

use zerocopy::{FromZeros, IntoBytes};

use crate::geom::{V3, normalize};
use crate::starcat::{Cell, Header, HipEntry, MAGIC, Star as CatStar, VERSION};

/// The HIP value of a star with no Hipparcos number.
pub const NO_HIP: u32 = u32::MAX;

/// A catalog of `(direction, G, HIP)` stars with BP − RP = 0, all in cell 0
/// of an nside-1 grid with a radius of π.
pub fn cat_bytes(stars: &[(V3, f32, u32)]) -> Vec<u8> {
    let mut stars = stars.to_vec();
    stars.sort_by(|a, b| a.1.total_cmp(&b.1));
    let n = stars.len();
    let hips: Vec<(u32, u32)> = (0u32..)
        .zip(&stars)
        .filter(|(_, s)| s.2 != NO_HIP)
        .map(|(row, s)| (row, s.2))
        .collect();
    let stars_off = Header::LEN + 12 * size_of::<Cell>();
    let mut header = Header::new_zeroed();
    header.magic = MAGIC;
    header.version.set(VERSION);
    header.nside.set(1);
    header.n_cells.set(12);
    header.n_stars.set(n as u32);
    header.n_hip.set(hips.len() as u32);
    header.cells_off.set(Header::LEN as u64);
    header.stars_off.set(stars_off as u64);
    header
        .hip_off
        .set((stars_off + n * size_of::<CatStar>()) as u64);
    let mut bytes = header.as_bytes().to_vec();
    for i in 0..12 {
        let mut cell = Cell::new_zeroed();
        cell.centre = [0.0f32, 0.0, 1.0].map(Into::into);
        let first = i == 0;
        cell.radius
            .set(if first { core::f32::consts::PI } else { 0.0 });
        cell.offset.set(if first { 0 } else { n as u32 });
        cell.count.set(if first { n as u32 } else { 0 });
        bytes.extend_from_slice(cell.as_bytes());
    }
    for (dir, g, _) in &stars {
        let mut star = CatStar::new_zeroed();
        star.dir = normalize(*dir)
            .unwrap_or([0.0, 0.0, 1.0])
            .map(|c| (c as f32).into());
        star.mags
            .set(u32::from(libm::roundf(g * 1000.0) as i16 as u16));
        bytes.extend_from_slice(star.as_bytes());
    }
    for (row, hip) in hips {
        let mut e = HipEntry::new_zeroed();
        e.row.set(row);
        e.hip.set(hip);
        bytes.extend_from_slice(e.as_bytes());
    }
    bytes
}

/// The committed G ≤ 8 render catalog.
pub static STARCAT_G8: &[u8] = include_bytes!("../../data/starcat-g8.bin");

/// The star simulator's default camera: 16 mm, 5.5 µm, 1024², no
/// distortion, mounted along the body axes.
pub fn sim_camera() -> crate::camera::Camera {
    crate::camera::Camera {
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
    }
}

/// A CPU renderer of `cat` with the star simulator's default optics, sensor
/// (12 bit) and hot pixels.
pub fn sim_renderer(cat: crate::starcat::StarCat<'_>) -> super::Renderer<'_> {
    let phot = super::Photometry {
        flux0_ph_s_m2: super::photometry::V0_PH_S_M2,
        aperture_m2: core::f32::consts::PI * 0.0125 * 0.0125,
        throughput: 0.5,
        colour: super::photometry::G_TO_V,
    };
    let sensor = super::Sensor {
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
    let shape = super::Shape {
        width: 1024,
        height: 1024,
        bits: 12,
    };
    let hot = super::HotPixels::new(shape.pixels(), 1e-4, 50.0, 13);
    // PANIC Safety: these fixed settings are valid.
    super::Renderer::new(cat, sim_camera(), phot, sensor, shape, hot, 12.0).expect("valid settings")
}
