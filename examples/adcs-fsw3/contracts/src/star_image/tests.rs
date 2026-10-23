use metor_fsw_3::metor_proto::com_de::{AsComponentView, Decomponentize};
use metor_fsw_3::metor_proto::types::{ComponentId, ComponentView, PrimType};
use metor_fsw_3::metor_proto_wkt::{ComponentMetadata, ImageFormat, ImageMeta};
use metor_fsw_3::{Payload, Record, RecordSchema};

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

fn metadata<R: Record>() -> Vec<ComponentMetadata> {
    let RecordSchema::Frame { metadata, .. } = R::schema_with_root("st") else {
        panic!("a frame schema");
    };
    metadata
}

/// Runs `f` on a 512 KiB stack, which a 2 MiB temporary would overflow.
fn on_small_stack(f: impl FnOnce() + Send + 'static) {
    let handle = std::thread::Builder::new()
        .stack_size(512 * 1024)
        .spawn(f)
        .expect("spawn");
    assert!(handle.join().is_ok());
}

fn image() -> Box<StarImage> {
    let mut image = StarImage::new_box().expect("alloc");
    image.frame_id = 9;
    image.exposure_us = 100_000;
    image.gain_milli = 250;
    for (i, px) in image.pixels.pixels_mut().iter_mut().enumerate() {
        *px = (i % 4096) as u16;
    }
    image
}

#[test]
fn test_image_components() {
    assert_eq!(StarImage::MAX_LEN, 2_097_176);
    let image = image();
    let scalar = |name: &str, prim| (ComponentId::new(name), prim, vec![]);
    assert_eq!(
        components::<StarImage>(image.as_bytes()),
        [
            scalar("st.frame_id", PrimType::U64),
            scalar("st.exposure_us", PrimType::U32),
            scalar("st.gain_milli", PrimType::U32),
            (
                ComponentId::new("st.pixels"),
                PrimType::U16,
                vec![1024, 1024]
            ),
        ]
    );
}

#[test]
fn test_image_metadata_is_12_bit_mono() {
    let metadata = metadata::<StarImage>();
    let pixels = metadata
        .iter()
        .find(|m| m.name == "st.pixels")
        .expect("pixels");
    let meta = ImageMeta::parse(pixels).expect("valid").expect("an image");
    assert_eq!((meta.format, meta.bits), (ImageFormat::Mono16, 12));
    assert_eq!(meta.check(PrimType::U16, &[1024, 1024]), Ok(()));
    let others = metadata.iter().filter(|m| m.name != "st.pixels");
    assert!(others.into_iter().all(|m| m.image_format().is_none()));
}

#[test]
fn test_image_decodes_in_place() {
    let image = image();
    let read = StarImage::decode(image.as_bytes()).expect("whole frame");
    assert!(core::ptr::eq(read, &*image));
    assert_eq!(read.pixels.get(5, 1), Some(Mono16(1029)));
    assert_eq!(
        (read.frame_id, read.exposure_us, read.gain_milli),
        (9, 100_000, 250)
    );
}

#[test]
fn test_truncated_image_fails_to_decode() {
    let image = image();
    let bytes = image.as_bytes();
    assert!(StarImage::decode(&bytes[..bytes.len() - 1]).is_err());
    assert!(StarImage::decode(&bytes[..16]).is_err());
    assert!(StarImage::decode(&[]).is_err());
}

#[test]
fn test_image_lives_on_the_heap() {
    on_small_stack(|| {
        let source = image();
        let mut target = StarImage::new_box().expect("alloc");
        let applied = target.apply_value(
            ComponentId::new("pixels"),
            source.pixels.as_component_view(),
            None,
        );
        assert!(applied.is_ok());
        assert_eq!(target.pixels.pixels(), source.pixels.pixels());
        let read = StarImage::decode(source.as_bytes()).expect("whole frame");
        assert_eq!(read.pixels.pixels()[4097], 1);
    });
}

fn truth(count: usize) -> StarTruth {
    let mut truth = StarTruth {
        frame_id: 3,
        n_rendered: 40,
        n_detectable: 35,
        blinded: BLINDED_SUN | BLINDED_MOON,
        count: count as u64,
        ..Default::default()
    };
    for i in 0..count {
        truth.u.0[i] = i as f32 + 0.5;
        truth.v.0[i] = 1023.5 - i as f32;
        truth.mag.0[i] = 2.0 + i as f32 * 0.1;
        truth.cat_id.0[i] = 1000 + i as u32;
    }
    truth
}

#[test]
fn test_truth_components() {
    let truth = truth(3);
    let scalar = |name: &str, prim| (ComponentId::new(name), prim, vec![]);
    let array = |name: &str, prim| (ComponentId::new(name), prim, vec![MAX_TRUTH as u64]);
    let found = components::<StarTruth>(truth.as_bytes());
    assert_eq!(
        found,
        [
            scalar("st.frame_id", PrimType::U64),
            (ComponentId::new("st.q_cam_eci"), PrimType::F64, vec![4]),
            (ComponentId::new("st.omega_cam"), PrimType::F64, vec![3]),
            scalar("st.n_rendered", PrimType::U32),
            scalar("st.n_detectable", PrimType::U32),
            scalar("st.blinded", PrimType::U32),
            scalar("st.count", PrimType::U64),
            array("st.u", PrimType::F32),
            array("st.v", PrimType::F32),
            array("st.mag", PrimType::F32),
            array("st.cat_id", PrimType::U32),
        ]
    );
    assert_eq!(StarTruth::MAX_LEN, 608);
}

#[test]
fn test_default_truth_is_empty() {
    let truth = StarTruth::default();
    assert_eq!(truth.count, 0);
    assert_eq!(truth.cat_id.0, [NO_CAT_ID; MAX_TRUTH]);
    assert_eq!(truth.u.0, [0.0; MAX_TRUTH]);
    assert_eq!(truth.blinded, 0);
}

#[test]
fn test_truth_decodes_in_place_at_zero_and_full() {
    for count in [0, MAX_TRUTH] {
        let truth = truth(count);
        let read = StarTruth::decode(truth.as_bytes()).expect("whole frame");
        assert!(core::ptr::eq(read, &truth));
        assert_eq!(read.count, count as u64);
        let ids = &read.cat_id.0;
        assert!(ids[..count].iter().all(|&id| id != NO_CAT_ID));
        assert!(ids[count..].iter().all(|&id| id == NO_CAT_ID));
        assert_eq!(read.blinded & BLINDED_EARTH, 0);
    }
}

#[test]
fn test_truncated_truth_fails_to_decode() {
    let truth = truth(1);
    let bytes = truth.as_bytes();
    assert!(StarTruth::decode(&bytes[..bytes.len() - 4]).is_err());
}

#[test]
fn test_truth_array_round_trips_through_component_view() {
    let source = truth(MAX_TRUTH);
    let mut target = StarTruth::default();
    for (name, view) in [
        ("u", source.u.as_component_view()),
        ("cat_id", source.cat_id.as_component_view()),
    ] {
        assert!(
            target
                .apply_value(ComponentId::new(name), view, None)
                .is_ok()
        );
    }
    assert_eq!(target.u, source.u);
    assert_eq!(target.cat_id, source.cat_id);
    let wrong = source.mag.as_component_view();
    let _ = target.apply_value(ComponentId::new("cat_id"), wrong, None);
    assert_eq!(target.cat_id, source.cat_id);
}

#[test]
fn test_zeroed_truth_is_not_empty() {
    let zeroed = StarTruth::new_zeroed();
    assert_eq!(zeroed.cat_id.0, [0; MAX_TRUTH]);
    assert_ne!(zeroed.cat_id, StarTruth::default().cat_id);
}

/// `q_cam_eci = q_b_eci ⊗ q_b_st⁻¹` maps ECI to the sensor frame the same way
/// as going through the body: `b_body = q_b_eci⁻¹ r`, `b_st = q_b_st b_body`.
#[test]
fn q_cam_eci_composes_body_and_mount() {
    use nox::{Quaternion, tensor};
    let unit = |q: [f64; 4]| -> Quat {
        let n = q.iter().map(|x| x * x).sum::<f64>().sqrt();
        Quaternion(tensor![q[0] / n, q[1] / n, q[2] / n, q[3] / n])
    };
    let q_b_st = unit([0.3, 0.1, -0.2, 0.9]);
    let q_b_eci = unit([-0.1, 0.5, 0.2, 0.8]);
    let r: V3 = tensor![0.6, 0.0, 0.8];
    let b_st = (q_b_st * (q_b_eci.inverse() * r)).into_buf();
    let q_cam_eci = q_b_eci * q_b_st.inverse();
    let ours = (q_cam_eci.inverse() * r).into_buf();
    assert!(
        (0..3).all(|i| (ours[i] - b_st[i]).abs() < 1e-15),
        "{ours:?} vs {b_st:?}"
    );
    let swapped = ((q_b_st.inverse() * q_b_eci).inverse() * r).into_buf();
    assert!((0..3).any(|i| (swapped[i] - b_st[i]).abs() > 1e-3));
}
