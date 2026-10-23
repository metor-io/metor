//! A frame holding a 1024² image.

use metor_component::{FrameImage, Mono16};
use metor_proto::com_de::{AsComponentView, Decomponentize};
use metor_proto::types::{ComponentId, ComponentView, PrimType, Timestamp};
use metor_proto_wkt::{ImageFormat, ImageMeta, Stretch};
use zerocopy::{FromBytes, FromZeros, Immutable, IntoBytes, KnownLayout};

use crate::Frame;
use crate::record::{Payload, Record, RecordSchema};

type Pixels = FrameImage<Mono16<12>, 1024, 1024>;

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[frame(name = "shot")]
#[repr(C)]
struct Shot {
    #[frame(timestamp)]
    timestamp: Timestamp,
    frame_id: u64,
    exposure_us: u32,
    gain_milli: u32,
    pixels: Pixels,
}

fn shot() -> Box<Shot> {
    let mut shot = Shot::new_box_zeroed().expect("alloc");
    shot.frame_id = 7;
    for (i, px) in shot.pixels.pixels_mut().iter_mut().enumerate() {
        *px = (i & 0xfff) as u16;
    }
    shot
}

/// Each component of `shot` under `root`, with its element and shape.
fn components(root: &str, shot: &Shot) -> Vec<(ComponentId, PrimType, Vec<u64>)> {
    let RecordSchema::Frame { vtable, .. } = Shot::schema_with_root(root) else {
        panic!("a frame schema");
    };
    let mut out = Vec::new();
    let mut sink = |id, view: ComponentView<'_>, _| {
        let shape = view.shape().iter().map(|&d| d as u64).collect();
        out.push((id, view.prim_type(), shape));
    };
    let applied = vtable.apply(shot.as_bytes(), &mut sink);
    assert!(matches!(applied, Ok(Ok(()))));
    out
}

#[test]
fn test_frame_layout() {
    assert_eq!(Shot::MAX_LEN, 2_097_176);
    let shot = shot();
    let components = components("st", &shot);
    assert_eq!(
        components,
        [
            (ComponentId::new("st.frame_id"), PrimType::U64, vec![]),
            (ComponentId::new("st.exposure_us"), PrimType::U32, vec![]),
            (ComponentId::new("st.gain_milli"), PrimType::U32, vec![]),
            (
                ComponentId::new("st.pixels"),
                PrimType::U16,
                vec![1024, 1024]
            ),
        ]
    );
}

#[test]
fn test_decode_borrows_in_place() {
    let shot = shot();
    let bytes = shot.as_bytes();
    let read = Shot::decode(bytes).expect("whole frame");
    assert!(core::ptr::eq(read, &*shot));
    assert_eq!(read.pixels.get(5, 1), Some(Mono16(1029)));
    assert!(Shot::decode(&bytes[..bytes.len() - 1]).is_err());
}

#[test]
fn test_fields_share_a_parent() {
    let RecordSchema::Frame { metadata, .. } = Shot::schema_with_root("st") else {
        panic!("a frame schema");
    };
    let names: Vec<_> = metadata.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "st.frame_id",
            "st.exposure_us",
            "st.gain_milli",
            "st.pixels"
        ]
    );
}

#[test]
fn test_image_meta_from_frame() {
    let RecordSchema::Frame { metadata, .. } = Shot::schema_with_root("st") else {
        panic!("a frame schema");
    };
    let pixels = metadata
        .iter()
        .find(|m| m.name == "st.pixels")
        .expect("pixels metadata");
    let image = ImageMeta::parse(pixels).expect("valid").expect("an image");
    assert_eq!(image.format, ImageFormat::Mono16);
    assert_eq!(image.bits, 12);
    assert_eq!(image.stretch, Stretch::Auto);
    assert_eq!(image.check(PrimType::U16, &[1024, 1024]), Ok(()));
    assert_eq!(pixels.lod(), Some(false));
    let scalars = metadata.iter().filter(|m| m.name != "st.pixels");
    assert!(scalars.into_iter().all(|m| m.image_format().is_none()));
}

#[test]
fn test_apply_value_in_place_on_small_stack() {
    let source = shot();
    let handle = std::thread::Builder::new()
        .stack_size(512 * 1024)
        .spawn(move || {
            let mut target = Shot::new_box_zeroed().expect("alloc");
            let applied = target.apply_value(
                ComponentId::new("pixels"),
                source.pixels.as_component_view(),
                None,
            );
            assert!(applied.is_ok());
            assert_eq!(target.pixels.pixels(), source.pixels.pixels());
            let wrong = Pixels::new_box_zeroed().expect("alloc");
            let _ = target.apply_value(
                ComponentId::new("frame_id"),
                wrong.as_component_view(),
                None,
            );
            assert_eq!(target.frame_id, 0);
        })
        .expect("spawn");
    assert!(handle.join().is_ok());
}
