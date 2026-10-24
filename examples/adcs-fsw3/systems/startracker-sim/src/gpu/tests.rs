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
