use super::*;
use crate::def::{DefCx, Records};
use crate::mux::cal::TcKind;
use crate::mux::params::SourceParam;
use crate::port::Output;
use crate::record::Record;
use crate::system::SystemDef;
use crate::tests::utils::{Imu, Inner, Nested, Note, Pair, PairA, PairB};
use crate::{Frame, Timestamp};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[frame(name = "swapped")]
#[repr(C)]
struct Swapped {
    #[frame(timestamp)]
    timestamp: Timestamp,
    z: f64,
    inner: Inner,
}

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[frame(name = "quad")]
#[repr(C)]
struct Quad {
    #[frame(timestamp)]
    timestamp: Timestamp,
    s: f64,
    v: crate::FrameArray<f64, 4>,
    b: crate::FrameArray<f64, 3>,
}

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[frame(name = "gauge")]
#[repr(C)]
struct Gauge {
    #[frame(timestamp)]
    timestamp: Timestamp,
    s: f64,
    v: crate::FrameArray<f64, 4>,
    n: i32,
    m: i32,
}

pub(crate) fn def<T: Record>(port: &str) -> PortDef {
    PortDef {
        name: port.to_string().into(),
        ..Output::<T>::def(&DefCx::empty(), T::NAME)
    }
}

pub(crate) fn records() -> Records {
    let defs = [
        def::<Pair>(Pair::NAME),
        def::<PairA>(PairA::NAME),
        def::<PairB>(PairB::NAME),
        def::<Nested>(Nested::NAME),
        def::<Swapped>(Swapped::NAME),
        def::<Note>(Note::NAME),
        def::<Quad>(Quad::NAME),
        def::<Gauge>(Gauge::NAME),
    ];
    Records::from_port_defs(defs.iter())
}

pub(crate) fn system(inputs: Vec<PortDef>, outputs: Vec<PortDef>) -> SystemDef {
    SystemDef {
        name: "mux".into(),
        inputs,
        outputs,
        log: None,
        grants: Vec::new(),
        io: None,
    }
}

pub(crate) fn map(dst: &str, input: &str, src: &str) -> FieldMap {
    FieldMap {
        dst: dst.into(),
        input: input.into(),
        src: src.into(),
        cal: None,
    }
}

pub(crate) fn output(port: &str, fields: Vec<FieldMap>) -> OutParam {
    OutParam {
        port: port.into(),
        on: TriggerParam::Cycle,
        stamp: StampParam::Now,
        max_age: None,
        fields,
    }
}

fn resolve(params: &MuxParams, def: &SystemDef) -> Result<Plan, MuxError> {
    let records = records();
    Plan::resolve(
        params,
        &Bound {
            def,
            records: &records,
        },
    )
}

/// `src.pair` split into `a` and `b`.
pub(crate) fn split() -> (MuxParams, SystemDef) {
    let params = MuxParams {
        inputs: vec!["src.pair".into()],
        outputs: vec![
            output("a", vec![map("a", "src.pair", "a")]),
            output("b", vec![map("b", "src.pair", "b")]),
        ],
    };
    let def = system(
        vec![def::<Pair>("src.pair")],
        vec![def::<PairA>("a"), def::<PairB>("b")],
    );
    (params, def)
}

fn copy(src: u32, dst: u32, len: u32) -> Copy {
    Copy {
        input: 0,
        src,
        dst,
        len,
        cal: None,
    }
}

#[test]
fn test_split() {
    let (params, def) = split();
    let plan = resolve(&params, &def).expect("resolves");
    assert_eq!(plan.outs[0].copies, [copy(8, 8, 8)]);
    assert_eq!(plan.outs[1].copies, [copy(16, 8, 24)]);
    assert_eq!((plan.outs[0].deps, plan.outs[1].port), (1, 1));
    assert_eq!(plan.ins[0].len, size_of::<Pair>() as u32);
    assert_eq!(plan.ins[0].stamp_at, Some(0));
}

#[test]
fn test_group_maps_leaf_by_leaf() {
    let params = MuxParams {
        inputs: vec!["n".into()],
        outputs: vec![output(
            "s",
            vec![map("inner", "n", "inner"), map("z", "n", "z")],
        )],
    };
    let def = system(vec![def::<Nested>("n")], vec![def::<Swapped>("s")]);
    let plan = resolve(&params, &def).expect("resolves");
    let copies = [copy(8, 16, 8), copy(16, 24, 8), copy(24, 8, 8)];
    assert_eq!(plan.outs[0].copies, copies);
}

#[test]
fn test_output_mapping_nothing() {
    let (mut params, def) = split();
    params.outputs[1].fields.clear();
    let plan = resolve(&params, &def).expect("resolves");
    assert!(plan.outs[1].copies.is_empty());
    assert_eq!(plan.outs[1].deps, 0);
    assert_eq!(plan.outs[1].stamp_at, Some(0));
}

#[test]
fn test_unknown_paths() {
    let (mut params, def) = split();
    params.outputs[0].fields = vec![map("c", "src.pair", "a")];
    assert!(matches!(
        resolve(&params, &def),
        Err(MuxError::UnknownField { port, path }) if port == "a" && path == "c"
    ));
    params.outputs[0].fields = vec![map("a", "src.pair", "c")];
    assert!(matches!(
        resolve(&params, &def),
        Err(MuxError::UnknownField { port, path }) if port == "src.pair" && path == "c"
    ));
    params.outputs[0].fields = vec![map("a", "other", "a")];
    assert_eq!(
        resolve(&params, &def),
        Err(MuxError::UnknownInput("other".into()))
    );
}

#[test]
fn test_signature_mismatch() {
    let (mut params, def) = split();
    params.outputs[1].fields = vec![map("b", "src.pair", "a")];
    assert!(matches!(
        resolve(&params, &def),
        Err(MuxError::Mismatch { .. })
    ));
}

#[test]
fn test_too_many_inputs() {
    let (mut params, def) = split();
    params.inputs = (0..65).map(|i| format!("p{i}")).collect();
    assert_eq!(resolve(&params, &def), Err(MuxError::TooManyInputs(65)));
}

#[test]
fn test_message_port_refused() {
    let params = MuxParams {
        inputs: vec!["note".into()],
        outputs: Vec::new(),
    };
    let def = system(vec![def::<Note>("note")], Vec::new());
    assert!(matches!(
        resolve(&params, &def),
        Err(MuxError::Fields {
            source: FieldError::Message(_),
            ..
        })
    ));
}

#[test]
fn test_registry_layout_mismatch() {
    let (params, system) = split();
    let stale = PortDef {
        layout: Some(7),
        ..def::<Pair>(Pair::NAME)
    };
    let records = Records::from_port_defs(
        [stale, def::<PairA>(PairA::NAME), def::<PairB>(PairB::NAME)].iter(),
    );
    let bound = Bound {
        def: &system,
        records: &records,
    };
    assert_eq!(
        Plan::resolve(&params, &bound),
        Err(MuxError::Layout("src.pair".into()))
    );
}

#[test]
fn test_unknown_record() {
    let params = split().0;
    let def = system(
        vec![def::<Imu>("src.pair")],
        vec![def::<PairA>("a"), def::<PairB>("b")],
    );
    let want = MuxError::UnknownRecord {
        port: "src.pair".into(),
        record: Imu::NAME.into(),
    };
    assert_eq!(resolve(&params, &def), Err(want));
}

#[test]
fn test_unbound_port() {
    let (mut params, def) = split();
    params.outputs[0].port = "c".into();
    assert_eq!(resolve(&params, &def), Err(MuxError::Unbound("c".into())));
}

#[test]
fn test_apply() {
    let src = [1u8, 2, 3, 4];
    let mut dst = [0u8; 4];
    assert!(apply(&copy(1, 2, 2), &[], &src, &mut dst));
    assert_eq!(dst, [0, 0, 2, 3]);
    assert!(!apply(&copy(3, 0, 2), &[], &src, &mut dst));
    assert!(!apply(&copy(0, 3, 2), &[], &src, &mut dst));
    assert!(!apply(&copy(u32::MAX, 0, 2), &[], &src, &mut dst));
}

#[test]
fn test_triggers_drain_last() {
    let (mut params, mut bound) = split();
    params.inputs.insert(0, "idle".into());
    bound.inputs.insert(0, def::<PairA>("idle"));
    params.outputs[0].on = TriggerParam::Input("src.pair".into());
    let plan = resolve(&params, &bound).expect("resolves");
    assert_eq!(plan.order, [0, 1]);
    params.outputs[0].on = TriggerParam::Input("idle".into());
    let plan = resolve(&params, &bound).expect("resolves");
    assert_eq!(plan.order, [1, 0]);
}

/// `src.pair` into a `quad` output, mapped by `fields`.
fn into_quad(fields: Vec<FieldMap>) -> Result<Plan, MuxError> {
    let params = MuxParams {
        inputs: vec!["src.pair".into()],
        outputs: vec![output("q", fields)],
    };
    let def = system(vec![def::<Pair>("src.pair")], vec![def::<Quad>("q")]);
    resolve(&params, &def)
}

#[test]
fn test_element_paths() {
    let (mut params, def) = split();
    params.outputs[0].fields = vec![map("a", "src.pair", "b.1")];
    params.outputs[1].fields = vec![map("b.0", "src.pair", "a")];
    let plan = resolve(&params, &def).expect("resolves");
    assert_eq!(plan.outs[0].copies, [copy(24, 8, 8)]);
    assert_eq!(plan.outs[1].copies, [copy(8, 8, 8)]);
}

#[test]
fn test_whole_leaf_and_element_in_one_output() {
    let plan = into_quad(vec![
        map("b", "src.pair", "b"),
        map("s", "src.pair", "b.2"),
        map("v.3", "src.pair", "a"),
    ])
    .expect("resolves");
    let copies = [copy(16, 48, 24), copy(32, 8, 8), copy(8, 40, 8)];
    assert_eq!(plan.outs[0].copies, copies);
}

#[test]
fn test_element_path_refusals() {
    let index = |port: &str, path: &str| MuxError::Index {
        port: port.into(),
        path: path.into(),
        len: 3,
    };
    let fields = |dst: &str, src: &str| vec![map(dst, "src.pair", src)];
    assert_eq!(into_quad(fields("s", "b.3")), Err(index("src.pair", "b.3")));
    assert_eq!(into_quad(fields("b.3", "a")), Err(index("q", "b.3")));
    assert!(matches!(
        into_quad(fields("s", "b.one")),
        Err(MuxError::UnknownField { port, path }) if port == "src.pair" && path == "b.one"
    ));
    assert!(matches!(
        into_quad(fields("s", "a.0")),
        Err(MuxError::UnknownField { .. })
    ));
    assert!(matches!(
        into_quad(fields("b", "b.0")),
        Err(MuxError::Mismatch { .. })
    ));
    let message = index("q", "b.3").to_string();
    assert_eq!(message, "`q.b.3`: index past a dimension of 3");
}

/// `g` and `src.pair` into a `gauge` output `o` and a `quad` output `q`.
fn calibrated(fields: Vec<FieldMap>, quad: Vec<FieldMap>) -> Result<Plan, MuxError> {
    let params = MuxParams {
        inputs: vec!["g".into(), "src.pair".into()],
        outputs: vec![output("o", fields), output("q", quad)],
    };
    let def = system(
        vec![def::<Gauge>("g"), def::<Pair>("src.pair")],
        vec![def::<Gauge>("o"), def::<Quad>("q")],
    );
    resolve(&params, &def)
}

fn cal_map(dst: &str, src: &str, cal: CalParam) -> FieldMap {
    FieldMap {
        cal: Some(cal),
        ..map(dst, "g", src)
    }
}

fn linear() -> CalParam {
    CalParam::Poly(vec![1.0, 2.0])
}

fn ratio(input: &str, src: &str, span: (f64, f64)) -> CalParam {
    CalParam::Ratiometric {
        excitation: SourceParam {
            input: input.into(),
            src: src.into(),
        },
        span,
        eng: (0.0, 100.0),
    }
}

fn cal_type(port: &str, path: &str) -> MuxError {
    MuxError::CalType {
        port: port.into(),
        path: path.into(),
    }
}

fn cal_table() -> MuxError {
    MuxError::CalTable { dst: "o.s".into() }
}

#[test]
fn test_calibrated_element() {
    let plan = calibrated(vec![cal_map("s", "v.1", linear())], Vec::new()).expect("resolves");
    let want = Copy {
        cal: Some(0),
        ..copy(24, 8, 8)
    };
    assert_eq!(plan.outs[0].copies, [want]);
    assert_eq!(plan.cals, [Cal::poly(&[1.0, 2.0]).expect("finite")]);
}

#[test]
fn test_calibrated_and_plain_in_one_output() {
    let fields = vec![map("v", "g", "v"), cal_map("s", "s", linear())];
    let quad = vec![FieldMap {
        cal: Some(CalParam::Thermocouple { kind: TcKind::K }),
        ..map("s", "src.pair", "a")
    }];
    let plan = calibrated(fields, quad).expect("resolves");
    let cal = |input, src, at| Copy {
        input,
        cal: Some(at),
        ..copy(src, 8, 8)
    };
    assert_eq!(plan.outs[0].copies, [copy(16, 16, 32), cal(0, 8, 0)]);
    assert_eq!(plan.outs[1].copies, [cal(1, 8, 1)]);
    assert_eq!(plan.cals[1], Cal::Thermocouple { kind: TcKind::K });
}

#[test]
fn test_ratiometric_excitation_in_the_same_input() {
    let plan = calibrated(
        vec![cal_map("s", "v.0", ratio("g", "v.3", (0.0, 1.0)))],
        Vec::new(),
    );
    let want = Cal::ratiometric(40, (0.0, 1.0), (0.0, 100.0)).expect("finite");
    assert_eq!(plan.expect("resolves").cals, [want]);
}

#[test]
fn test_apply_calibrates() {
    let plan = calibrated(vec![cal_map("s", "v.1", linear())], Vec::new()).expect("resolves");
    let c = plan.outs[0].copies[0];
    let mut src = [0u8; 56];
    src[24..32].copy_from_slice(&3.0f64.to_le_bytes());
    let mut dst = [0u8; 56];
    assert!(apply(&c, &plan.cals, &src, &mut dst));
    assert_eq!(dst[8..16], 7.0f64.to_le_bytes());
    assert!(!apply(&c, &[], &src, &mut dst), "a cal index past `cals`");
    assert!(
        !apply(&c, &plan.cals, &src[..30], &mut dst),
        "a short source"
    );
    assert!(
        !apply(&c, &plan.cals, &src, &mut dst[..12]),
        "a short staging"
    );
}

#[test]
fn test_calibration_types_refused() {
    let one = |dst: &str, src: &str| calibrated(vec![cal_map(dst, src, linear())], Vec::new());
    assert_eq!(one("n", "s"), Err(cal_type("o", "n")));
    assert_eq!(one("s", "n"), Err(cal_type("g", "n")));
    assert_eq!(one("v", "s"), Err(cal_type("o", "v")));
    assert_eq!(one("s", "v"), Err(cal_type("g", "v")));
    let quad = vec![FieldMap {
        cal: Some(linear()),
        ..map("b", "src.pair", "a")
    }];
    assert_eq!(calibrated(Vec::new(), quad), Err(cal_type("q", "b")));
    assert!(matches!(one("s", "w"), Err(MuxError::UnknownField { .. })));
    assert!(matches!(one("s", "v.4"), Err(MuxError::Index { .. })));
}

#[test]
fn test_excitation_refusals() {
    let one = |cal| calibrated(vec![cal_map("s", "s", cal)], Vec::new());
    let other = MuxError::CalInput {
        dst: "o.s".into(),
        input: "src.pair".into(),
    };
    assert_eq!(one(ratio("src.pair", "a", (0.0, 1.0))), Err(other));
    assert_eq!(one(ratio("g", "v", (0.0, 1.0))), Err(cal_type("g", "v")));
    assert_eq!(one(ratio("g", "n", (0.0, 1.0))), Err(cal_type("g", "n")));
}

#[test]
fn test_calibration_table_refusals() {
    let one = |cal| calibrated(vec![cal_map("s", "s", cal)], Vec::new());
    assert_eq!(one(CalParam::Poly(Vec::new())), Err(cal_table()));
    let unsorted = CalParam::Table(vec![(1.0, 0.0), (0.0, 1.0)]);
    assert_eq!(one(unsorted), Err(cal_table()));
    let flat = ratio("g", "v.0", (1.0, 1.0));
    assert_eq!(one(flat), Err(cal_table()));
    let message = cal_table().to_string();
    assert_eq!(
        message,
        "`o.s`'s calibration is empty, unsorted, or has a zero-width span"
    );
}

#[test]
fn test_too_many_cals() {
    let cal = || Cal::Thermocouple { kind: TcKind::K };
    let mut cals = vec![cal(); MAX_CALS - 1];
    assert_eq!(push_cal(&mut cals, cal()), Ok(u16::MAX));
    assert_eq!(
        push_cal(&mut cals, cal()),
        Err(MuxError::TooManyCals(MAX_CALS + 1))
    );
    assert_eq!(cals.len(), MAX_CALS);
}
