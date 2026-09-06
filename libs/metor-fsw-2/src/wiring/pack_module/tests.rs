use super::*;
use metor_fsw_2_core::SystemKind;
use metor_fsw_2_core::abi::PackManifest;
use metor_proto::types::ComponentId;
use metor_proto::vtable::VTable;
use metor_proto_wkt::ComponentMetadata;
use postcard_schema::Schema;
use serde::Serialize;

// A params type exercising the scalar/Option split: the defaults blob is
// the whole struct, so every non-Option field gets a default.
#[derive(Serialize, Schema, Default)]
struct Demo {
    count: u64,
    gain: f64,
    label: String,
    armed: bool,
    limit: Option<u32>,
    offsets: [f64; 3],
}

fn demo_schema() -> OwnedNamedType {
    OwnedNamedType::from(<Demo as Schema>::SCHEMA)
}

#[test]
fn defaults_split_from_whole_struct_blob() {
    let blob = postcard::to_allocvec(&Demo {
        count: 7,
        gain: 2.5,
        label: "hi".into(),
        armed: true,
        limit: Some(9),
        offsets: [0.1, 0.0, -0.2],
    })
    .unwrap();
    let params = Codegen::default().params(&demo_schema(), Some(&blob));
    let by = |n: &str| params.iter().find(|p| p.name == n).unwrap();
    assert_eq!(by("count").annotation, "int");
    assert_eq!(by("count").default.as_deref(), Some("7"));
    assert_eq!(by("gain").annotation, "float");
    assert_eq!(by("gain").default.as_deref(), Some("2.5"));
    assert_eq!(by("label").default.as_deref(), Some("\"hi\""));
    assert_eq!(by("armed").default.as_deref(), Some("True"));
    // Option fields default to None regardless of the blob.
    assert_eq!(by("limit").annotation, "int | None");
    assert_eq!(by("limit").default.as_deref(), Some("None"));
    // `[f64; 3]` is an exact tuple.
    assert_eq!(by("offsets").annotation, "tuple[float, float, float]");
    assert_eq!(by("offsets").default.as_deref(), Some("(0.1, 0.0, -0.2)"));
}

#[test]
fn no_blob_makes_non_option_fields_required() {
    let params = Codegen::default().params(&demo_schema(), None);
    let by = |n: &str| params.iter().find(|p| p.name == n).unwrap();
    assert!(
        by("count").default.is_none(),
        "no defaults blob => required"
    );
    // Even without a blob an Option field is optional.
    assert_eq!(by("limit").default.as_deref(), Some("None"));
}

fn table_port(name: &str, frame: &str, telemetered: bool) -> PortDesc {
    PortDesc {
        name: name.to_string(),
        max_size: 16,
        schema: PortSchema::Table {
            frame_id: ComponentId::new(frame),
            vtable: VTable::default(),
            metadata: vec![ComponentMetadata::from(format!("{frame}.value").as_str())],
        },
        delivery: Delivery::Snapshot,
        fan_in: FanIn::One,
        telemetered,
        conn: Default::default(),
    }
}

fn msg_port(name: &str) -> PortDesc {
    PortDesc {
        name: name.to_string(),
        max_size: 8,
        schema: PortSchema::Postcard {
            id: [7, 0],
            schema: None,
        },
        delivery: Delivery::Log,
        fan_in: FanIn::One,
        telemetered: false,
        conn: Default::default(),
    }
}

fn entry_desc(
    name: &str,
    inputs: Vec<PortDesc>,
    outputs: Vec<PortDesc>,
    params_docs: Vec<(String, String)>,
) -> PackEntryDesc {
    PackEntryDesc {
        descriptor: metor_fsw_2_core::SystemDescriptor {
            name: name.to_string(),
            kind: SystemKind::Cyclic,
            inputs,
            outputs,
            capabilities: Vec::new(),
        },
        params_schema: demo_schema(),
        params_docs,
        reloadable: true,
        params_default: None,
    }
}

pub(super) fn demo_manifest() -> Vec<u8> {
    let blob = postcard::to_allocvec(&Demo::default()).unwrap();
    let widget_docs = vec![
        ("count".to_string(), "How many widgets to make.".to_string()),
        (
            "gain".to_string(),
            "Loop gain (1/s), ~3e-12 at 400 km.".to_string(),
        ),
    ];
    let msg = PackManifest {
        systems: vec![
            PackEntryDesc {
                params_default: Some(blob),
                ..entry_desc(
                    "Widget",
                    vec![table_port("cmd", "cmd", false)],
                    vec![table_port("sensors", "sensors", true), msg_port("events")],
                    widget_docs,
                )
            },
            PackEntryDesc {
                params_schema: OwnedNamedType::from(<() as Schema>::SCHEMA),
                ..entry_desc(
                    "startup",
                    Vec::new(),
                    vec![table_port("mode_cmd", "mode_cmd", true)],
                    Vec::new(),
                )
            },
        ],
    };
    postcard::to_allocvec(&msg).unwrap()
}

#[test]
fn render_is_deterministic_and_structured() {
    let bytes = demo_manifest();
    let a = render_module(
        "demo",
        "demo-systems",
        "demo_systems",
        &bytes,
        metor_fsw_2_core::abi::FSW_ABI_VERSION,
        "demo-pack",
        "1.2.0",
    )
    .unwrap();
    let b = render_module(
        "demo",
        "demo-systems",
        "demo_systems",
        &bytes,
        metor_fsw_2_core::abi::FSW_ABI_VERSION,
        "demo-pack",
        "1.2.0",
    )
    .unwrap();
    assert_eq!(a, b, "codegen is deterministic");

    assert!(a.contains("@generated by `metor-fsw pack dev`"));
    assert!(!a.contains("202"), "no timestamps in the header");
    assert!(a.contains("ARTIFACT = Artifact("));
    assert!(a.contains("manifest_hash=\"sha256:"));
    // A CapWords entry is a class; a snake_case entry an occupant callable.
    assert!(a.contains("class Widget(System):"));
    assert!(a.contains("def startup() -> System:"));
    // Frame markers, one per distinct frame, named from metadata.
    assert!(a.contains("class Cmd(Frame):"));
    assert!(a.contains("class Sensors(Frame):"));
    assert!(a.contains("class ModeCmd(Frame):"));
    // Port annotations reference the markers; a Postcard port uses `Msg`.
    assert!(a.contains("cmd: InPort[Cmd]"));
    assert!(a.contains("sensors: OutPort[Sensors]  # output, latest-wins, telemetered"));
    assert!(a.contains("events: OutPort[Msg]"));
    // Defaults from the whole-struct blob show up as kwarg defaults.
    assert!(a.contains("count: int = 0"));
    assert!(a.contains("limit: int | None = None"));
}

/// Generated modules self-locate their `_libs` payload and carry the
/// generating ABI version plus distribution provenance.
#[test]
fn module_locates_and_stamps() {
    let bytes = demo_manifest();
    let a = render_module(
        "demo",
        "demo-systems",
        "demo_systems",
        &bytes,
        metor_fsw_2_core::abi::FSW_ABI_VERSION,
        "demo-pack",
        "1.2.0",
    )
    .unwrap();

    assert!(a.contains("@generated by `metor-fsw pack dev`"));
    assert!(a.contains("from pathlib import Path"));
    assert!(a.contains("prebuilt=str(Path(__file__).resolve().parent / \"_libs\"),"));
    assert!(a.contains(&format!(
        "abi_version={},",
        metor_fsw_2_core::abi::FSW_ABI_VERSION
    )));
    assert!(a.contains("dist=\"demo-pack\","));
    assert!(a.contains("dist_version=\"1.2.0\","));
}

/// One fixture, two consumers: the checked-in `python/tests/data/demo.py`
/// (imported by the Python suite) must be exactly what this codegen
/// produces. Regenerate it with the ignored `fixture_dump::write_demo_fixture`.
#[test]
fn demo_fixture_matches_checked_in() {
    let generated = render_module(
        "demo",
        "demo-systems",
        "demo_systems",
        &demo_manifest(),
        metor_fsw_2_core::abi::FSW_ABI_VERSION,
        "demo-pack",
        "1.2.0",
    )
    .unwrap();
    let checked_in = include_str!("../../../python/tests/data/demo.py");
    assert_eq!(
        generated, checked_in,
        "codegen drifted from the checked-in fixture; rerun \
         `cargo test -p metor-fsw-2 fixture_dump -- --ignored`"
    );
}

#[test]
fn manifest_hash_is_stable_and_prefixed() {
    let h = manifest_hash(b"hello");
    assert!(h.starts_with("sha256:"));
    assert_eq!(h, manifest_hash(b"hello"));
    assert_ne!(h, manifest_hash(b"hallo"));
}

#[test]
fn py_float_keeps_a_decimal_point() {
    assert_eq!(py_float(5.0), "5.0");
    assert_eq!(py_float(0.0005), "0.0005");
    assert_eq!(py_float(-0.2), "-0.2");
}

#[test]
fn pascal_case_and_frame_name() {
    assert_eq!(pascal_case("attitude_estimate"), "AttitudeEstimate");
    assert_eq!(pascal_case("tick_in"), "TickIn");
    let meta = vec![
        ComponentMetadata::from("sensors.gyro_b"),
        ComponentMetadata::from("sensors.mag_b"),
    ];
    assert_eq!(frame_name(&meta).as_deref(), Some("sensors"));
}
