use std::mem::offset_of;

use super::*;
use crate::geom::Quat;
use crate::render::Motion;
use crate::render::testkit::{cat_bytes, sim_camera, sim_scene};
use crate::starcat::StarCat;

/// `(span, member offsets)` of the WGSL struct `name`.
fn wgsl_layout(name: &str) -> (usize, Vec<usize>) {
    let module = naga::front::wgsl::parse_str(SHADER).expect("the shader parses");
    let flags = naga::valid::ValidationFlags::all();
    let mut validator = naga::valid::Validator::new(flags, naga::valid::Capabilities::empty());
    validator.validate(&module).expect("the shader validates");
    let ty = module
        .types
        .iter()
        .find(|(_, t)| t.name.as_deref() == Some(name));
    let Some(naga::TypeInner::Struct { members, span }) = ty.map(|(_, t)| &t.inner) else {
        panic!("no struct {name}");
    };
    let offsets = members.iter().map(|m| m.offset as usize).collect();
    (*span as usize, offsets)
}

#[test]
fn layouts_match_wgsl() {
    let (span, offsets) = wgsl_layout("Params");
    assert_eq!(span, size_of::<Params>());
    assert_eq!(offsets.len(), 16);
    assert_eq!(offsets[4], offset_of!(Params, erf_scale));
    assert_eq!(offsets[15], offset_of!(Params, _pad));
    let (span, offsets) = wgsl_layout("Splat");
    assert_eq!(span, size_of::<Splat>());
    let rust = [
        offset_of!(Splat, u),
        offset_of!(Splat, v),
        offset_of!(Splat, e),
        offset_of!(Splat, star),
    ];
    assert_eq!(offsets, rust);
}

/// A lost device fails every later frame with its reason, without blocking.
#[test]
fn a_lost_device_fails_frames() {
    let bytes = cat_bytes(&[]);
    let scene = sim_scene(StarCat::parse(&bytes).expect("an empty catalog"));
    let mut out = vec![0; scene.shape().pixels()];
    let mut gpu = GpuRenderer::new(scene).expect("a GPU");
    let motion = Motion {
        q_b_eci: Quat::IDENTITY,
        omega_b: [0.0; 3],
        beta: [0.0; 3],
        t_exp: 0.1,
    };
    let frame = Frame::new(&sim_camera(), &motion, 4, 1, 0).expect("a frame");
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
