use super::*;
use crate::{FrameList, FrameMap, Timestamp};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
#[metor_fsw(name = "sensors")]
struct Sensors {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    omega: f64,
    flags: u64,
}

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
#[metor_fsw(name = "sensors")]
struct ReorderedSensors {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    flags: u64,
    omega: f64,
}

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
#[metor_fsw(name = "sensors")]
struct SensorSubset {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    omega: f64,
}

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
#[metor_fsw(name = "samples")]
struct ListOne {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    values: FrameList<u8, 1>,
}

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
#[metor_fsw(name = "samples")]
struct ListTwo {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    values: FrameList<u8, 2>,
}

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
#[metor_fsw(name = "samples")]
struct MapOne {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    values: FrameMap<u64, 1, 1>,
}

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
#[metor_fsw(name = "samples")]
struct MapTwo {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    values: FrameMap<u64, 1, 2>,
}

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
#[metor_fsw(name = "nested")]
struct Nested {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    values: FrameList<ListOne, 2>,
}

fn endpoint() -> EndpointContract {
    EndpointContract::new(
        "plant_sensors",
        "1.0",
        "simulation",
        vec![
            FrameChannel::of::<Sensors>("imu_a", Delivery::Snapshot).unwrap(),
            FrameChannel::of::<Sensors>("imu_b", Delivery::Snapshot).unwrap(),
        ],
    )
    .unwrap()
}

#[test]
fn distinct_channels_can_share_a_frame_type() {
    let contract = endpoint();
    assert_eq!(
        contract.channels[0].frame.frame_id,
        contract.channels[1].frame.frame_id
    );
    contract.channels[0].require_frame::<Sensors>().unwrap();
    let restored = EndpointContract::from_json(&contract.to_json().unwrap()).unwrap();
    contract.require_exact(&restored).unwrap();
    assert_eq!(contract.hash().unwrap(), restored.hash().unwrap());
}

#[test]
fn canonical_hash_ignores_order_and_descriptions_but_not_semantics() {
    let mut expected = endpoint();
    for channel in &mut expected.channels {
        channel.frame.metadata[0]
            .metadata
            .insert("units".into(), "rad/s".into());
        channel.frame.metadata[0]
            .metadata
            .insert("coordinate_frame".into(), "body".into());
    }
    let mut actual = expected.clone();
    actual.description = "Another deployment's documentation".into();
    for channel in &mut actual.channels {
        channel.description = "A different physical sensor".into();
        let properties = &mut channel.frame.metadata[0].metadata;
        properties.clear();
        properties.insert("coordinate_frame".into(), "body".into());
        properties.insert("units".into(), "rad/s".into());
        channel.frame.metadata.reverse();
    }
    actual.channels.reverse();
    assert_eq!(
        expected.canonical_bytes().unwrap(),
        actual.canonical_bytes().unwrap()
    );
    expected.require_exact(&actual).unwrap();
    actual.channels[0]
        .semantics
        .insert("omega.units".into(), "deg/s".into());
    assert_ne!(expected.hash().unwrap(), actual.hash().unwrap());
    assert!(
        actual
            .require_exact(&expected)
            .unwrap_err()
            .to_string()
            .contains("semantics")
    );
}

#[test]
fn exact_match_rejects_local_subset_compatibility_and_reordered_fields() {
    let contract = endpoint();
    let error = contract.channels[0]
        .require_frame::<ReorderedSensors>()
        .unwrap_err();
    assert!(error.to_string().contains("vtable"), "{error}");
    assert!(crate::compatible(
        &PortDesc::of::<Sensors>(),
        &PortDesc::of::<SensorSubset>()
    ));
    assert!(
        contract.channels[0]
            .require_frame::<SensorSubset>()
            .is_err()
    );
}

#[test]
fn dynamic_bounds_change_identity_even_with_identical_vtables_and_sizes() {
    for (a, b, property) in [
        (
            FrameChannel::of::<ListOne>("samples", Delivery::Snapshot).unwrap(),
            FrameChannel::of::<ListTwo>("samples", Delivery::Snapshot).unwrap(),
            "max_count",
        ),
        (
            FrameChannel::of::<MapOne>("samples", Delivery::Log).unwrap(),
            FrameChannel::of::<MapTwo>("samples", Delivery::Log).unwrap(),
            "max_key_bytes",
        ),
    ] {
        assert_eq!(a.frame.max_record_size, b.frame.max_record_size);
        assert_eq!(
            serde_json::to_value(&a.frame.vtable).unwrap(),
            serde_json::to_value(&b.frame.vtable).unwrap()
        );
        let a = EndpointContract::new("samples", "1", "simulation", vec![a]).unwrap();
        let b = EndpointContract::new("samples", "1", "simulation", vec![b]).unwrap();
        assert_ne!(a.hash().unwrap(), b.hash().unwrap());
        assert!(
            a.require_exact(&b)
                .unwrap_err()
                .to_string()
                .contains(property)
        );
        a.require_exact(&EndpointContract::from_json(&a.to_json().unwrap()).unwrap())
            .unwrap();
    }
}

#[test]
fn delivery_byte_order_clock_and_version_are_contractual() {
    let expected = endpoint();
    let mut variants = vec![expected.clone(); 4];
    variants[0].channels[0].delivery = Delivery::Log;
    variants[1].channels[0].frame.byte_order = match ByteOrder::NATIVE {
        ByteOrder::Little => ByteOrder::Big,
        ByteOrder::Big => ByteOrder::Little,
    };
    variants[2].clock_domain = "unix".into();
    variants[3].version = "2".into();
    for actual in variants {
        assert_ne!(expected.hash().unwrap(), actual.hash().unwrap());
        assert!(expected.require_exact(&actual).is_err());
    }
}

#[test]
fn rejects_invalid_or_ambiguous_contracts_on_import() {
    let mut duplicate = endpoint();
    duplicate.channels[1].key = duplicate.channels[0].key.clone();
    let mut invalid_layout = endpoint();
    invalid_layout.channels[0].frame.layout.fields[1].offset = u64::MAX;
    let mut invalid_alignment = endpoint();
    invalid_alignment.channels[0].frame.layout.alignment = 0;
    let mut invalid_timestamp = endpoint();
    invalid_timestamp.channels[0].frame.layout.timestamp_offset = Some(3);
    let mut unknown_version = endpoint();
    unknown_version.format_version += 1;
    let mut empty = endpoint();
    empty.channels.clear();
    for invalid in [
        duplicate,
        invalid_layout,
        invalid_alignment,
        invalid_timestamp,
        unknown_version,
        empty,
    ] {
        let json = serde_json::to_string(&invalid).unwrap();
        assert!(EndpointContract::from_json(&json).is_err());
        assert!(invalid.hash().is_err());
    }
    let mut unknown_field = serde_json::to_value(endpoint()).unwrap();
    unknown_field["channles"] = serde_json::json!([]);
    assert!(EndpointContract::from_json(&unknown_field.to_string()).is_err());
    assert!(EndpointContract::from_json(&" ".repeat(MAX_CONTRACT_BYTES + 1)).is_err());
}

#[test]
fn rejects_unrepresented_nested_dynamic_bounds() {
    let err = FrameChannel::of::<Nested>("nested", Delivery::Snapshot).unwrap_err();
    assert!(err.to_string().contains("nested or unbounded"), "{err}");
}

#[test]
fn rejects_inconsistent_or_overflowing_dynamic_bounds() {
    let good = FrameChannel::of::<ListOne>("samples", Delivery::Snapshot).unwrap();
    let mut huge = good.clone();
    let Some(DynamicBounds::List { max_count, .. }) = &mut huge.frame.layout.fields[1].dynamic
    else {
        panic!()
    };
    *max_count = u64::MAX;
    assert!(
        huge.validate()
            .unwrap_err()
            .to_string()
            .contains("overflow")
    );
    let mut missing = good.clone();
    missing.frame.layout.fields[1].dynamic = None;
    assert!(missing.validate().is_err());
    let mut wrong_stride = good;
    let Some(DynamicBounds::List { stride, .. }) = &mut wrong_stride.frame.layout.fields[1].dynamic
    else {
        panic!()
    };
    *stride = 2;
    assert!(
        wrong_stride
            .validate()
            .unwrap_err()
            .to_string()
            .contains("disagree")
    );
}

#[test]
fn timestamp_opt_out_and_explicit_padding_are_described() {
    #[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
    #[repr(C)]
    #[metor_fsw(no_timestamp)]
    struct Unstamped {
        value: u64,
    }
    #[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
    #[repr(C)]
    struct Padded {
        enabled: u8,
        _pad: [u8; 7],
        #[metor_fsw(timestamp)]
        timestamp: Timestamp,
    }
    let unstamped = FrameChannel::of::<Unstamped>("unstamped", Delivery::Snapshot).unwrap();
    assert_eq!(unstamped.frame.layout.timestamp_offset, None);
    let padded = FrameChannel::of::<Padded>("padded", Delivery::Snapshot).unwrap();
    assert_eq!(padded.frame.layout.timestamp_offset, Some(8));
    assert_eq!(padded.frame.layout.fields[1].size, 7);
    assert!(padded.frame.layout.fields[1].padding);
}

#[test]
fn hidden_typed_values_cannot_silently_escape_the_icd() {
    #[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
    #[repr(C)]
    struct Hidden {
        #[metor_fsw(timestamp)]
        timestamp: Timestamp,
        value: f64,
        #[metor_fsw(skip)]
        hidden_gain: f64,
    }
    let err = FrameChannel::of::<Hidden>("hidden", Delivery::Snapshot).unwrap_err();
    assert!(
        err.to_string().contains("without a telemetry schema"),
        "{err}"
    );
}

#[test]
fn canonical_v1_hash_is_frozen() {
    // Native bytes are intentionally part of the ICD; this fixture pins the
    // little-endian layout shared by our supported ARM and x86 targets.
    if ByteOrder::NATIVE == ByteOrder::Little {
        assert_eq!(
            endpoint().hash().unwrap(),
            "sha256:0f8f098a2c024983f5fad0b02d0ca6ebe3610705373576bd6809c909e18cc38c"
        );
    }
}

#[test]
fn dynamic_elements_must_have_a_complete_value_schema() {
    #[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
    #[repr(C)]
    struct SensorList {
        #[metor_fsw(timestamp)]
        timestamp: Timestamp,
        sensors: FrameList<Sensors, 2>,
    }
    // Today's element vtable suppresses Sensors.timestamp, so accepting this
    // would leave bytes in each element outside the exact ICD.
    let err = FrameChannel::of::<SensorList>("sensors", Delivery::Snapshot).unwrap_err();
    assert!(
        err.to_string()
            .contains("dynamic element contains undescribed bytes"),
        "{err}"
    );
}
