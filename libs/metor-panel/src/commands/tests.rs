use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use metor_proto::types::{ComponentId, PrimType, Timestamp};
use metor_proto::vtable::builder::{component, raw_field, raw_table, schema, vtable};
use metor_proto_wkt::{CommandInfo, CommandKind, ComponentMetadata, MsgMetadata};
use postcard_schema::Schema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::frame::FrameLayout;
use super::*;
use crate::inspector::rows::RowAction;

#[derive(Serialize, Deserialize, Schema, Debug, PartialEq)]
enum Mode {
    Idle,
    Track,
    Slew,
}

#[derive(Serialize, Deserialize, Schema, Debug, PartialEq)]
struct Gains {
    kp: f32,
    kd: f64,
}

#[derive(Serialize, Deserialize, Schema, Debug, PartialEq)]
struct Point {
    mode: Mode,
    gains: Gains,
    count: u8,
    offset: i16,
    armed: bool,
    label: String,
    limit: Option<f32>,
    targets: Vec<u16>,
}

fn point_form() -> Form {
    Form::Postcard(Point::SCHEMA.into())
}

fn set(draft: &mut Value, pointer: &str, value: Value) {
    *draft.pointer_mut(pointer).expect("the draft has the field") = value;
}

fn now() -> Timestamp {
    Timestamp(1_700_000_000_000_000)
}

#[test]
fn a_default_draft_encodes_as_the_zero_value() {
    let form = point_form();
    let bytes = form.encode(&form.default_draft(), now()).unwrap();
    let decoded: Point = postcard::from_bytes(&bytes).unwrap();
    assert_eq!(
        decoded,
        Point {
            mode: Mode::Idle,
            gains: Gains { kp: 0.0, kd: 0.0 },
            count: 0,
            offset: 0,
            armed: false,
            label: String::new(),
            limit: None,
            targets: Vec::new(),
        }
    );
}

/// Edits to a unit enum, a nested struct, and the JSON-edited `Option` and
/// `Vec` all land in the bytes, and decode back to the same draft.
#[test]
fn an_edited_draft_round_trips_through_the_schema() {
    let form = point_form();
    let mut draft = form.default_draft();
    set(&mut draft, "/mode", json!("Slew"));
    set(&mut draft, "/gains/kp", json!(1.5));
    set(&mut draft, "/gains/kd", json!(-0.25));
    set(&mut draft, "/count", json!(200));
    set(&mut draft, "/offset", json!(-300));
    set(&mut draft, "/armed", json!(true));
    set(&mut draft, "/label", json!("burn"));
    set(&mut draft, "/limit", json!(2.5));
    set(&mut draft, "/targets", json!([1, 2, 3]));

    let bytes = form.encode(&draft, now()).unwrap();
    let decoded: Point = postcard::from_bytes(&bytes).unwrap();
    assert_eq!(
        decoded,
        Point {
            mode: Mode::Slew,
            gains: Gains { kp: 1.5, kd: -0.25 },
            count: 200,
            offset: -300,
            armed: true,
            label: "burn".into(),
            limit: Some(2.5),
            targets: vec![1, 2, 3],
        }
    );
    let Form::Postcard(schema) = &form else {
        unreachable!()
    };
    assert_eq!(postcard_dyn::from_slice_dyn(schema, &bytes).unwrap(), draft);
}

#[test]
fn values_the_wire_type_cannot_hold_are_refused_by_field() {
    let form = point_form();
    let refused = |pointer: &str, value: Value| {
        let mut draft = form.default_draft();
        set(&mut draft, pointer, value);
        form.encode(&draft, now()).unwrap_err()
    };
    assert_eq!(
        refused("/count", json!(256)),
        EncodeError::OutOfRange {
            field: "count".into(),
            value: "256".into(),
            ty: "u8",
        }
    );
    assert!(matches!(
        refused("/offset", json!(-40_000)),
        EncodeError::OutOfRange { ty: "i16", .. }
    ));
    assert!(matches!(
        refused("/gains/kp", json!(1e39)),
        EncodeError::OutOfRange { field, ty: "f32", .. } if field == "gains.kp"
    ));
    assert!(matches!(
        refused("/count", json!(1.5)),
        EncodeError::Mismatch { field, .. } if field == "count"
    ));
    assert!(matches!(
        refused("/mode", json!("Reverse")),
        EncodeError::Mismatch { field, .. } if field == "mode"
    ));
    assert!(matches!(
        refused("/targets", json!([1, 70_000])),
        EncodeError::OutOfRange { field, .. } if field == "targets.1"
    ));
}

/// The command the ADCS example's ground sends: a text field beside an enum
/// with payloads, which the form edits as JSON.
#[test]
fn a_sequence_command_encodes_with_its_payload_variant() {
    use metor_proto_wkt::{SequenceCommand, SequenceCommandKind};
    let form = Form::Postcard(SequenceCommand::SCHEMA.into());
    let mut draft = form.default_draft();
    set(&mut draft, "/channel", json!("adcs"));
    let first = draft["command"]
        .as_object()
        .unwrap()
        .keys()
        .next()
        .unwrap()
        .clone();
    set(
        &mut draft,
        &format!("/command/{first}/name"),
        json!("detumble"),
    );
    let sent: SequenceCommand = postcard::from_bytes(&form.encode(&draft, now()).unwrap()).unwrap();
    assert_eq!(sent.channel, "adcs");
    assert!(matches!(sent.command, SequenceCommandKind::Load { name } if name == "detumble"));
}

fn msg_info(id: [u8; 2], name: &str, schema: OwnedNamedType, codec: Option<&str>) -> CommandInfo {
    CommandInfo {
        id,
        kind: CommandKind::Msg(MsgMetadata {
            name: name.into(),
            schema,
            metadata: codec
                .map(|codec| HashMap::from([("codec".to_string(), codec.to_string())]))
                .unwrap_or_default(),
        }),
    }
}

use postcard_schema::schema::owned::OwnedNamedType;

#[test]
fn the_codec_picks_the_form() {
    let none = |_| None;
    let bytes_schema: OwnedNamedType = <Vec<u8>>::SCHEMA.into();
    let json_form = Form::of(
        &msg_info([1, 0], "Note", bytes_schema.clone(), Some("json")),
        &none,
    );
    assert_eq!(json_form, Form::Json);
    assert_eq!(
        json_form.encode(&json!({"text": "hi"}), now()).unwrap(),
        br#"{"text":"hi"}"#
    );

    let raw = Form::of(
        &msg_info([2, 0], "Blob", bytes_schema, Some("bytes")),
        &none,
    );
    assert!(matches!(raw, Form::Unsupported(_)));
    assert!(matches!(
        raw.encode(&Value::Null, now()),
        Err(EncodeError::Unsupported(_))
    ));

    let postcard = Form::of(
        &msg_info([3, 0], "Point", Point::SCHEMA.into(), None),
        &none,
    );
    assert_eq!(postcard, point_form());
}

/// An `Imu`-like record: a timestamp, a vector, then two one-byte fields,
/// padded out to the timestamp's alignment.
fn imu_vtable() -> metor_proto::vtable::VTable<
    Vec<metor_proto::vtable::Op>,
    Vec<u8>,
    Vec<metor_proto::vtable::Field>,
> {
    let stamp = raw_table(0u32, 8);
    let field = |offset: u32, ty: PrimType, dim: &[u64], name: &str| {
        let len = ty.size() as u32 * dim.iter().product::<u64>() as u32;
        raw_field(
            offset,
            len,
            schema(ty, dim, component(ComponentId::new(name))),
        )
        .with_timestamp(stamp.clone())
    };
    vtable([
        field(8, PrimType::F64, &[3], "sat.cmds.imu.accel"),
        field(32, PrimType::U8, &[], "sat.cmds.imu.mode"),
        field(33, PrimType::Bool, &[], "sat.cmds.imu.armed"),
    ])
}

fn imu_metadata(id: ComponentId) -> Option<ComponentMetadata> {
    [
        "sat.cmds.imu.accel",
        "sat.cmds.imu.mode",
        "sat.cmds.imu.armed",
    ]
    .into_iter()
    .find(|name| ComponentId::new(name) == id)
    .map(|name| {
        let meta = ComponentMetadata {
            component_id: id,
            name: name.into(),
            metadata: HashMap::new(),
        };
        match name.ends_with("accel") {
            true => meta.with_element_names(["x", "y", "z"]),
            false => meta,
        }
    })
}

#[test]
fn a_frame_is_laid_out_and_named_from_its_vtable() {
    let layout = FrameLayout::new(&imu_vtable(), &imu_metadata).unwrap();
    assert_eq!(layout.prefix().map(|p| p.as_ref()), Some("sat.cmds.imu"));
    let labels: Vec<&str> = layout.fields().iter().map(|f| f.label.as_ref()).collect();
    assert_eq!(labels, ["accel", "mode", "armed"]);
    assert_eq!(layout.fields()[0].element_label(1).as_ref(), "y");

    let unnamed = FrameLayout::new(&imu_vtable(), &|_| None).unwrap();
    assert_eq!(unnamed.prefix(), None);
    assert_eq!(
        unnamed.fields()[1].label.as_ref(),
        ComponentId::new("sat.cmds.imu.mode").to_string()
    );
}

/// The encoded record reads back through the vtable the way the db ingests
/// it: every value, and the timestamp each field shares.
#[test]
fn a_frame_round_trips_through_its_vtable() {
    let table = imu_vtable();
    let layout = FrameLayout::new(&table, &imu_metadata).unwrap();
    let form = Form::Frame(layout);
    let mut draft = form.default_draft();
    set(&mut draft, "/accel", json!([1.0, -2.5, 9.81]));
    set(&mut draft, "/mode", json!(7));
    set(&mut draft, "/armed", json!(true));
    let bytes = form.encode(&draft, now()).unwrap();
    assert_eq!(bytes.len(), 40);

    let mut seen = Vec::new();
    for field in table.realize_fields(Some(&bytes)) {
        let field = field.unwrap();
        let view = field.view.unwrap();
        let values: Vec<f64> = (0..field.shape.iter().product::<usize>().max(1))
            .map(|ix| view.get(ix).unwrap().as_f64())
            .collect();
        assert_eq!(field.timestamp, Some(now()));
        seen.push((field.component_id, values));
    }
    assert_eq!(
        seen,
        vec![
            (
                ComponentId::new("sat.cmds.imu.accel"),
                vec![1.0, -2.5, 9.81]
            ),
            (ComponentId::new("sat.cmds.imu.mode"), vec![7.0]),
            (ComponentId::new("sat.cmds.imu.armed"), vec![1.0]),
        ]
    );

    set(&mut draft, "/mode", json!(300));
    assert!(matches!(
        form.encode(&draft, now()),
        Err(EncodeError::OutOfRange { ty: "u8", .. })
    ));
    set(&mut draft, "/mode", json!(1));
    set(&mut draft, "/accel", json!([1.0]));
    assert!(matches!(
        form.encode(&draft, now()),
        Err(EncodeError::Mismatch { .. })
    ));
}

#[test]
fn only_a_live_link_carries_a_command() {
    assert_eq!(link_refusal(&[ConnectionStatus::Connected]), None);
    assert_eq!(
        link_refusal(&[ConnectionStatus::Reconnecting, ConnectionStatus::Connected]),
        None
    );
    assert!(link_refusal(&[]).unwrap().contains("not connected"));
    assert!(
        link_refusal(&[ConnectionStatus::Reconnecting])
            .unwrap()
            .contains("reconnecting")
    );
    assert!(
        link_refusal(&[ConnectionStatus::Failed("refused".into())])
            .unwrap()
            .contains("refused")
    );
}

const POINT: [u8; 2] = [7, 1];

/// Every record pushed onto `id`'s log since `reader` was taken.
fn sent(reader: &mut metor_db::disruptor::Reader) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    while let Some(grant) = reader.try_next() {
        let mut buf: &[u8] = &grant;
        while let Some((rest, _, msg)) = metor_db::msg_log::read_msg(buf) {
            buf = rest;
            out.push(msg.to_vec());
        }
    }
    out
}

fn send_page(cx: &App) -> Vec<Box<dyn crate::inspector::rows::InspectorRow>> {
    let send = Category::Custom("Send".into());
    ItemRegistry::snapshot(cx)
        .into_iter()
        .filter(|(category, _)| *category == send)
        .flat_map(|(_, items)| items)
        .find_map(|item| match item {
            InspectionItem::SubMenu { label, build, .. } if label == "Point" => Some(build(cx)),
            _ => None,
        })
        .expect("the palette offers the command")
}

/// Opening a command's Send page, editing a field and activating Send puts
/// the encoded draft in the command's log; with the link down it puts
/// nothing there and says why.
#[gpui::test]
fn the_send_page_records_the_encoded_draft(cx: &mut gpui::TestAppContext) {
    let temp = tempfile::tempdir().unwrap();
    let db = Arc::new(DB::create(temp.path().join("db")).unwrap());
    db.add_commands(&[msg_info(POINT, "Point", Point::SCHEMA.into(), None)]);
    let mut log = db
        .with_state_mut(|s| s.get_or_insert_msg_log(POINT, &db.path).cloned())
        .unwrap()
        .wal_reader();
    let link = Arc::new(Mutex::new(Vec::<ConnectionStatus>::new()));
    let catalog = cx.update(|cx| {
        crate::theme::set_theme(cx, Arc::new(crate::theme::DARK.clone()));
        ItemRegistry::init(cx);
        let probe = link.clone();
        let catalog = CommandCatalog::install(
            db.clone(),
            Arc::new(move |_| probe.lock().unwrap().clone()),
            cx,
        );
        CommandCatalog::register_provider(cx);
        catalog
    });
    let cx = cx.add_empty_window();

    cx.update(|window, cx| {
        let mut rows = send_page(cx);
        let count = rows.iter_mut().find(|row| row.label() == "count").unwrap();
        let RowAction::StartEdit { on_commit, .. } = count.activate(window, cx) else {
            panic!("a number edits inline");
        };
        on_commit("42".into(), window, cx);
        // `Option<f32>` has no row of its own, so it is edited as JSON and
        // checked against its schema as it is entered.
        let limit = rows.iter_mut().find(|row| row.label() == "limit").unwrap();
        let RowAction::StartEdit { on_commit, .. } = limit.activate(window, cx) else {
            panic!("JSON edits inline");
        };
        on_commit("\"high\"".into(), window, cx);
        assert!(matches!(
            catalog.read(cx).outcome(POINT),
            Some(Outcome::Refused(reason)) if reason.starts_with("limit")
        ));
        assert_eq!(catalog.read(cx).field(POINT, "/limit"), Some(&Value::Null));
        let limit = rows.iter_mut().find(|row| row.label() == "limit").unwrap();
        let RowAction::StartEdit { on_commit, .. } = limit.activate(window, cx) else {
            panic!("JSON edits inline");
        };
        on_commit("2.5".into(), window, cx);
        assert_eq!(catalog.read(cx).field(POINT, "/limit"), Some(&json!(2.5)));
        let send = rows.iter_mut().find(|row| row.label() == "Send").unwrap();
        send.activate(window, cx);
    });
    assert!(sent(&mut log).is_empty(), "the link is down");
    cx.update(|_, cx| {
        assert!(matches!(
            catalog.read(cx).outcome(POINT),
            Some(Outcome::Refused(reason)) if reason.contains("not connected")
        ));
    });

    *link.lock().unwrap() = vec![ConnectionStatus::Connected];
    cx.update(|window, cx| {
        let mut rows = send_page(cx);
        let send = rows.iter_mut().find(|row| row.label() == "Send").unwrap();
        send.activate(window, cx);
        assert!(matches!(
            catalog.read(cx).outcome(POINT),
            Some(Outcome::Sent(_))
        ));
    });
    let records = sent(&mut log);
    assert_eq!(records.len(), 1, "Send recorded the command once");
    let point: Point = postcard::from_bytes(&records[0]).unwrap();
    assert_eq!(point.count, 42, "the draft outlived reopening the page");
    assert_eq!(point.limit, Some(2.5));
}
