use metor_fsw_3::metor_proto::types::{ComponentId, ComponentView, PrimType};
use metor_fsw_3::{Payload, Record, RecordSchema};
use nox::{Quaternion, tensor};

use super::*;

/// Each component of `R` read from `bytes`, with its element and shape.
fn components<R: Record>(bytes: &[u8]) -> Vec<(ComponentId, PrimType, Vec<u64>)> {
    let RecordSchema::Frame { vtable, .. } = R::schema_with_root("st") else {
        panic!("a frame schema");
    };
    let mut out = Vec::new();
    let mut sink = |id: ComponentId, view: ComponentView<'_>, _| {
        let shape = view.shape().iter().map(|&d| d as u64).collect();
        out.push((id, view.prim_type(), shape));
    };
    assert!(matches!(vtable.apply(bytes, &mut sink), Ok(Ok(()))));
    out
}

#[test]
fn layouts() {
    let image = StarImage::new_box().expect("alloc");
    let c = |name: &str, prim, shape: &[u64]| (ComponentId::new(name), prim, shape.to_vec());
    assert_eq!(
        components::<StarImage>(image.as_bytes()),
        [
            c("st.frame_id", PrimType::U64, &[]),
            c("st.exposure_us", PrimType::U32, &[]),
            c("st.gain_milli", PrimType::U32, &[]),
            c("st.pixels", PrimType::U16, &[1024, 1024]),
        ]
    );
    assert_eq!(StarImage::MAX_LEN, 2_097_176);
    let truth = components::<StarTruth>(StarTruth::default().as_bytes());
    assert_eq!(truth.len(), 11);
    assert_eq!(truth[1], c("st.q_cam_eci", PrimType::F64, &[4]));
    assert_eq!(
        truth[10],
        c("st.cat_id", PrimType::U32, &[MAX_TRUTH as u64])
    );
    assert_eq!(StarTruth::MAX_LEN, 608);
}

#[test]
fn decode_in_place_and_reject_truncation() {
    let mut image = StarImage::new_box().expect("alloc");
    image.pixels.pixels_mut()[1029] = 7;
    let bytes = image.as_bytes();
    let read = StarImage::decode(bytes).expect("whole frame");
    assert!(core::ptr::eq(read, &*image));
    assert_eq!(read.pixels.get(5, 1), Some(Mono16(7)));
    assert!(StarImage::decode(&bytes[..bytes.len() - 1]).is_err());
    let truth = StarTruth::default();
    assert_eq!(truth.cat_id.0, [NO_CAT_ID; MAX_TRUTH]);
    let bytes = truth.as_bytes();
    assert!(StarTruth::decode(bytes).is_ok());
    assert!(StarTruth::decode(&bytes[..bytes.len() - 4]).is_err());
}

/// `q_cam_eci = q_b_eci ⊗ q_b_st⁻¹` maps ECI to the sensor frame as going
/// through the body does: `b_body = q_b_eci⁻¹ r`, `b_st = q_b_st b_body`.
#[test]
fn q_cam_eci_composes_body_and_mount() {
    let unit = |q: [f64; 4]| -> Quat { Quaternion(tensor![q[0], q[1], q[2], q[3]].normalize()) };
    let (q_b_st, q_b_eci) = (unit([0.3, 0.1, -0.2, 0.9]), unit([-0.1, 0.5, 0.2, 0.8]));
    let r: V3 = tensor![0.6, 0.0, 0.8];
    let b_st = q_b_st * (q_b_eci.inverse() * r);
    let ours = (q_b_eci * q_b_st.inverse()).inverse() * r;
    let err: f64 = (ours - b_st).norm().into_buf();
    assert!(err < 1e-15, "{err}");
}
