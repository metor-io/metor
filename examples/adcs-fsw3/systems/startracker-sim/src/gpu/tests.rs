use std::mem::offset_of;

use metor_star::render::Splat;

use super::{Params, SHADER};

fn module() -> naga::Module {
    naga::front::wgsl::parse_str(SHADER).expect("the shader parses")
}

#[test]
fn shader_validates_without_capabilities() {
    let module = module();
    let mut validator = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::empty(),
    );
    validator.validate(&module).expect("the shader validates");
}

/// `(span, member offsets)` of the WGSL struct `name`.
fn wgsl_layout(module: &naga::Module, name: &str) -> (u32, Vec<u32>) {
    let ty = module
        .types
        .iter()
        .find(|(_, t)| t.name.as_deref() == Some(name))
        .map(|(_, t)| &t.inner);
    let Some(naga::TypeInner::Struct { members, span }) = ty else {
        panic!("no struct {name}");
    };
    (*span, members.iter().map(|m| m.offset).collect())
}

#[test]
fn params_layout_matches_wgsl() {
    let (span, offsets) = wgsl_layout(&module(), "Params");
    assert_eq!(span as usize, size_of::<Params>());
    assert_eq!(offsets.len(), 16);
    assert_eq!(offsets[4] as usize, offset_of!(Params, erf_scale));
    assert_eq!(offsets[12] as usize, offset_of!(Params, seed_lo));
    assert_eq!(offsets[15] as usize, offset_of!(Params, _pad));
}

#[test]
fn splat_layout_matches_wgsl() {
    let (span, offsets) = wgsl_layout(&module(), "Splat");
    assert_eq!(span as usize, size_of::<Splat>());
    let rust = [
        offset_of!(Splat, u),
        offset_of!(Splat, v),
        offset_of!(Splat, e),
        offset_of!(Splat, star),
    ];
    assert_eq!(offsets, rust.map(|o| o as u32));
}

/// A lost device fails every later frame with its reason, without blocking.
#[test]
fn a_lost_device_fails_frames() {
    use metor_star::camera::{Camera, Intrinsics};
    use metor_star::geom::Quat;
    use metor_star::render::testkit::cat_bytes;
    use metor_star::render::{Frame, HotPixels, Motion, Photometry, Renderer, Sensor, Shape};
    use metor_star::starcat::StarCat;

    use super::{GpuError, GpuRenderer};

    let size = 64;
    let camera = Camera {
        intr: Intrinsics {
            f_m: 16e-3,
            pitch_m: 5.5e-6,
            cx: 32.0,
            cy: 32.0,
            k1: 0.0,
            k2: 0.0,
            p1: 0.0,
            p2: 0.0,
            width: size,
            height: size,
        },
        q_b_st: Quat::IDENTITY,
    };
    let shape = Shape {
        width: size,
        height: size,
        bits: 12,
    };
    let bytes = cat_bytes(&[]);
    let cat = StarCat::parse(&bytes).expect("an empty catalog");
    let phot = Photometry {
        flux0_ph_s_m2: 1e10,
        aperture_m2: 1e-3,
        throughput: 0.5,
        colour: [0.0; 4],
    };
    let sensor = Sensor {
        psf_sigma_px: 1.0,
        full_well_e: 1e4,
        read_noise_e: 1.0,
        dark_e_s: 0.0,
        dark_t0_c: 20.0,
        temp_c: 20.0,
        gain_milli: 1000,
        bias_dn: 0.0,
        zodi_mag_arcsec2: None,
    };
    let hot = HotPixels::none(shape.pixels());
    let scene = Renderer::new(cat, camera, phot, sensor, shape, hot, 12.0).expect("a scene");
    let mut gpu = GpuRenderer::new(scene).expect("a GPU");
    let motion = Motion {
        q_b_eci: Quat::IDENTITY,
        omega_b: [0.0; 3],
        beta: [0.0; 3],
        t_exp: 0.1,
    };
    let frame = Frame::new(&camera, &motion, 4, 1, 0).expect("a frame");
    let mut out = vec![0; shape.pixels()];
    gpu.render(&frame, &mut out)
        .expect("a frame before the loss");
    gpu.device.destroy();
    for _ in 0..2 {
        let err = gpu.render(&frame, &mut out).err();
        let Some(GpuError::Device(why)) = err else {
            panic!("{err:?}");
        };
        assert!(why.contains("lost"), "{why}");
    }
}
