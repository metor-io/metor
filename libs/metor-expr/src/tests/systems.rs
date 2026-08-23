//! P2: the language layer — frames, systems, bindings, and the one-liner tier.
//!
//! Every system here is driven the way a host will drive it: write each input
//! frame's fields at `<name>_arg_ptr(i)` plus the manifest's offsets, call
//! `<name>_eval(now)`, read the output frame at `<name>_ret_ptr()`. Results
//! that are arithmetic go through the differential rule and are compared
//! against native nox bit for bit.

use std::collections::HashMap;

use nox::{ArrayRepr, Const, ReprMonad, Tensor};
use wasmi::{Instance, Store, Val};

use super::instantiate;
use crate::{
    Binding, CompSchema, Dtype, FrameSchema, Program, Resolver, Ty, compile_expr, compile_module,
};

/// A host that knows a fixed set of components — what the panel's db vtables
/// are, in miniature.
pub(super) struct Table(HashMap<String, Ty>);

impl Table {
    pub(super) fn new(entries: &[(&str, Ty)]) -> Self {
        Table(
            entries
                .iter()
                .map(|(path, ty)| ((*path).to_string(), ty.clone()))
                .collect(),
        )
    }
}

fn vec3() -> Ty {
    Ty::Tensor {
        dtype: Dtype::F64,
        shape: vec![3],
    }
}

impl Resolver for Table {
    fn component(&self, path: &str) -> Option<CompSchema> {
        self.0.get(path).map(|ty| CompSchema { ty: ty.clone() })
    }

    fn suffix(&self, name: &str) -> Vec<String> {
        let mut found: Vec<String> = self
            .0
            .keys()
            .filter(|path| path.rsplit('.').next() == Some(name))
            .cloned()
            .collect();
        found.sort();
        found
    }

    fn frame(&self, _name: &str) -> Option<FrameSchema> {
        None
    }
}

/// Drive one system through the region ABI.
struct Run {
    store: Store<()>,
    instance: Instance,
    program: Program,
    system: usize,
}

impl Run {
    fn new(program: Program, system: &str) -> Self {
        let (store, instance) = instantiate(&program.wasm, 100_000_000);
        let system = program
            .manifest
            .systems
            .iter()
            .position(|s| s.name == system)
            .expect("the manifest names the system");
        Run {
            store,
            instance,
            program,
            system,
        }
    }

    fn name(&self) -> &str {
        &self.program.manifest.systems[self.system].name
    }

    fn address(&mut self, accessor: &str, index: Option<i32>) -> u32 {
        let accessor = format!("{}_{accessor}", self.name());
        let func = self
            .instance
            .get_func(&self.store, &accessor)
            .unwrap_or_else(|| panic!("missing {accessor}"));
        let args: Vec<Val> = index.map(Val::I32).into_iter().collect();
        let mut out = [Val::I32(0)];
        func.call(&mut self.store, &args, &mut out).unwrap();
        match out[0] {
            Val::I32(v) => v as u32,
            ref other => panic!("expected an address, got {other:?}"),
        }
    }

    /// Fill one field of one input frame, addressed the way a host would:
    /// port pointer plus the manifest's offset.
    fn set(&mut self, port: &str, field: &str, values: &[f64]) -> &mut Self {
        let system = &self.program.manifest.systems[self.system];
        let index = system
            .inputs
            .iter()
            .position(|p| p.param == port)
            .unwrap_or_else(|| panic!("no port `{port}`"));
        let offset = system.inputs[index]
            .frame
            .field(field)
            .unwrap_or_else(|| panic!("no field `{field}`"))
            .offset;
        let at = self.address("arg_ptr", Some(index as i32)) + offset;
        let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        let memory = self.instance.get_memory(&self.store, "memory").unwrap();
        memory.write(&mut self.store, at as usize, &bytes).unwrap();
        self
    }

    fn eval(&mut self, now: i64) -> i32 {
        let name = format!("{}_eval", self.name());
        let func = self.instance.get_func(&self.store, &name).unwrap();
        let mut out = [Val::I32(0)];
        func.call(&mut self.store, &[Val::I64(now)], &mut out)
            .expect("a system must not trap");
        match out[0] {
            Val::I32(v) => v,
            ref other => panic!("expected a fault code, got {other:?}"),
        }
    }

    fn get(&mut self, field: &str) -> Vec<f64> {
        let output = &self.program.manifest.systems[self.system].output;
        let field = output
            .field(field)
            .unwrap_or_else(|| panic!("no output field `{field}`"));
        let (offset, count) = (
            field.offset,
            match &field.ty {
                Ty::Tensor { shape, .. } => shape.iter().product::<usize>(),
                _ => 1,
            },
        );
        let at = self.address("ret_ptr", None) + offset;
        let memory = self.instance.get_memory(&self.store, "memory").unwrap();
        let mut bytes = vec![0u8; count * 8];
        memory.read(&self.store, at as usize, &mut bytes).unwrap();
        bytes
            .chunks_exact(8)
            .map(|c| f64::from_le_bytes(c.try_into().unwrap()))
            .collect()
    }

    fn scalar(&mut self, field: &str) -> f64 {
        self.get(field)[0]
    }

    /// A `bool` field, which occupies the low four bytes of its eight.
    fn flag(&mut self, field: &str) -> bool {
        self.get(field)[0].to_bits() as u32 != 0
    }
}

fn build(source: &str, table: &Table, system: &str) -> Run {
    match compile_module(source, table) {
        Ok(program) => Run::new(program, system),
        Err(diags) => panic!("expected {source:?} to compile, got:\n{diags}"),
    }
}

fn refuse(source: &str, table: &Table) -> String {
    match compile_module(source, table) {
        Ok(_) => panic!("expected {source:?} to be refused"),
        Err(diags) => format!("{diags}"),
    }
}

fn nox3(v: [f64; 3]) -> Tensor<f64, Const<3>, ArrayRepr> {
    v.into()
}

const IMU: &str = "\
class Imu(Frame):
    omega: Tensor[f64, 3]
    accel: Tensor[f64, 3]

class Rate(Frame):
    rate: f64
    hot: bool
";

pub(super) fn imu_table() -> Table {
    Table::new(&[
        ("imu.omega", vec3()),
        ("imu.accel", vec3()),
        ("wheels.rpm", Ty::F64),
        ("adcs_imu_b.omega", vec3()),
        ("adcs_imu_b.accel", vec3()),
        ("adcs.omega_b", vec3()),
    ])
}

#[test]
fn a_frame_system_runs_against_its_ports() {
    let source = format!(
        "{IMU}
@system
def watchdog(imu: Imu) -> Rate:
    return Rate(
        rate=sqrt(imu.omega[0] ** 2 + imu.omega[1] ** 2 + imu.omega[2] ** 2),
        hot=imu.accel[2] > 9.0,
    )
"
    );
    let mut run = build(&source, &imu_table(), "watchdog");
    let omega = [0.25f64, -0.5, 0.125];
    run.set("imu", "omega", &omega)
        .set("imu", "accel", &[0.0, 0.0, 9.81]);
    assert_eq!(run.eval(0), 0);

    let squared = nox3(omega) * nox3(omega);
    let want = squared.into_inner().view().buf().iter().sum::<f64>().sqrt();
    assert_eq!(run.scalar("rate").to_bits(), want.to_bits());
    assert!(run.flag("hot"));
}

/// The default binding is the snake case of the frame class, field by field,
/// and the manifest records what it resolved to rather than what was written.
#[test]
fn ports_bind_by_frame_name_and_the_manifest_records_the_paths() {
    let source = format!(
        "{IMU}\n@system\ndef w(imu: Imu) -> Rate:\n    return Rate(rate=imu.omega[0], hot=False)\n"
    );
    let program = compile_module(&source, &imu_table()).unwrap();
    let system = program.manifest.system("w").unwrap();
    assert_eq!(
        system.inputs[0].bindings,
        vec![
            Binding::Component("imu.omega".into()),
            Binding::Component("imu.accel".into())
        ]
    );
    assert_eq!(system.publishes, vec!["w.rate", "w.hot"]);
    assert_eq!(system.driving, Some(0));
}

#[test]
fn bind_points_a_port_at_a_different_frame() {
    let source = format!(
        "{IMU}\n@system(bind={{\"imu\": \"adcs_imu_b\"}})\ndef w(imu: Imu) -> Rate:\n    return Rate(rate=imu.omega[0], hot=False)\n"
    );
    let program = compile_module(&source, &imu_table()).unwrap();
    let system = program.manifest.system("w").unwrap();
    assert_eq!(system.inputs[0].frame.name, "adcs_imu_b");
    assert_eq!(
        system.inputs[0].bindings[0],
        Binding::Component("adcs_imu_b.omega".into())
    );
}

/// Path-bound parameters need no annotations: their types come from the
/// host's schemas.
#[test]
fn path_bound_parameters_take_their_types_from_the_host() {
    let source = "\
@system(\"adcs.omega_b\", \"wheels.rpm\")
def rate(omega_b, rpm) -> f64:
    return sqrt(dot(omega_b, omega_b)) + rpm
";
    let mut run = build(source, &imu_table(), "rate");
    let omega = [1.0f64, 2.0, 3.0];
    run.set("omega_b", "omega_b", &omega)
        .set("rpm", "rpm", &[100.0]);
    run.eval(0);

    let squared = nox3(omega) * nox3(omega);
    let want = squared
        .into_inner()
        .view()
        .buf()
        .iter()
        .fold(0.0, |acc, v| acc + v)
        .sqrt()
        + 100.0;
    assert_eq!(run.scalar("rate").to_bits(), want.to_bits());
    assert_eq!(
        compile_module(source, &imu_table())
            .unwrap()
            .manifest
            .system("rate")
            .unwrap()
            .publishes,
        vec!["rate"]
    );
}

#[test]
fn on_chooses_the_driving_input() {
    let source = "\
@system(\"adcs.omega_b\", \"wheels.rpm\", on=\"rpm\")
def rate(omega_b, rpm) -> f64:
    return rpm
";
    let program = compile_module(source, &imu_table()).unwrap();
    assert_eq!(program.manifest.system("rate").unwrap().driving, Some(1));

    let bad = refuse(
        "@system(\"wheels.rpm\", on=\"nope\")\ndef r(rpm) -> f64:\n    return rpm\n",
        &imu_table(),
    );
    assert!(bad.contains("not a parameter"), "{bad}");
}

/// State is what a system keeps, and a body that never assigns it reads the
/// same slot it wrote last time.
#[test]
fn state_carries_between_evaluations() {
    let source = "\
class Lp(State):
    filtered: f64 = 0.0

@system(\"wheels.rpm\")
def lowpass(rpm, s: Lp) -> f64:
    s.filtered = 0.5 * rpm + 0.5 * s.filtered
    return s.filtered
";
    let mut run = build(source, &imu_table(), "lowpass");
    let mut want = 0.0;
    for step in 0..4 {
        let sample = 100.0 + f64::from(step);
        run.set("rpm", "rpm", &[sample]);
        run.eval(step.into());
        want = 0.5 * sample + 0.5 * want;
        assert_eq!(run.scalar("lowpass").to_bits(), want.to_bits());
    }

    let system = &compile_module(source, &imu_table())
        .unwrap()
        .manifest
        .systems[0];
    assert_eq!(system.state.len(), 1);
    assert_eq!(system.state[0].name, "filtered");
}

/// A binding naming an earlier binding is a frame edge between two anonymous
/// systems, which is what makes the canvas projection recoverable from names.
#[test]
fn top_level_bindings_are_anonymous_systems_that_can_chain() {
    let source = "\
scaled = adcs.omega_b * 100.0
biggest = scaled[0] + scaled[1] + scaled[2]
";
    let program = compile_module(source, &imu_table()).unwrap();
    assert_eq!(program.manifest.systems.len(), 2);
    assert_eq!(
        program.manifest.system("scaled").unwrap().inputs[0].bindings[0],
        Binding::Component("adcs.omega_b".into())
    );
    assert_eq!(
        program.manifest.system("biggest").unwrap().inputs[0].bindings[0],
        Binding::Produced {
            system: 0,
            field: 0
        }
    );
    assert_eq!(
        program.manifest.system("scaled").unwrap().publishes,
        ["scaled"]
    );

    let mut run = Run::new(program, "scaled");
    run.set("adcs.omega_b", "omega_b", &[1.0, 2.0, 3.0]);
    run.eval(0);
    assert_eq!(run.get("scaled"), vec![100.0, 200.0, 300.0]);
}

/// The one-liner tier: type an expression into a field and it is a system.
#[test]
fn a_bare_expression_is_a_system() {
    let program = compile_expr("adcs.omega_b * 100.0", &imu_table()).unwrap();
    let system = &program.manifest.systems[0];
    assert_eq!(system.name, "expr");
    assert_eq!(system.publishes, ["expr"]);
    assert_eq!(
        system.inputs[0].bindings[0],
        Binding::Component("adcs.omega_b".into())
    );

    let mut run = Run::new(program, "expr");
    run.set("adcs.omega_b", "omega_b", &[0.5, 1.5, -2.0]);
    run.eval(0);
    assert_eq!(run.get("expr"), vec![50.0, 150.0, -200.0]);
}

/// A bare name resolves by unique suffix, and what the manifest keeps is the
/// full path — so a component added later cannot change what a saved
/// expression reads.
#[test]
fn a_bare_name_resolves_by_unique_suffix_and_is_recorded_resolved() {
    let program = compile_expr("rpm * 2.0", &imu_table()).unwrap();
    assert_eq!(
        program.manifest.systems[0].inputs[0].bindings[0],
        Binding::Component("wheels.rpm".into())
    );

    let crowded = Table::new(&[("wheels.rpm", Ty::F64), ("motor.rpm", Ty::F64)]);
    let diags = compile_expr("rpm * 2.0", &crowded).unwrap_err();
    let text = format!("{diags}");
    assert!(text.contains("ambiguous"), "{text}");
    assert!(
        text.contains("motor.rpm") && text.contains("wheels.rpm"),
        "{text}"
    );
}

#[test]
fn now_is_the_timestamp_the_host_passed() {
    let mut run = build(
        "@system(\"wheels.rpm\")\ndef stamp(rpm) -> f64:\n    return float(now()) + rpm\n",
        &imu_table(),
        "stamp",
    );
    run.set("rpm", "rpm", &[0.5]);
    run.eval(41);
    assert_eq!(run.scalar("stamp"), 41.5);
}

#[test]
fn systems_are_refused_with_spans_when_they_do_not_add_up() {
    let table = imu_table();
    for (source, needle) in [
        (
            format!("{IMU}\n@system\ndef w(imu: Imu) -> Rate:\n    return Rate(rate=imu.nope, hot=False)\n"),
            "has no field `nope`",
        ),
        (
            format!("{IMU}\n@system\ndef w(imu: Imu) -> Rate:\n    return Rate(rate=imu.omega[0])\n"),
            "`hot` is not given a value",
        ),
        (
            format!("{IMU}\n@system\ndef w(imu: Imu) -> Rate:\n    imu.omega = imu.accel\n    return Rate(rate=0.0, hot=False)\n"),
            "cannot be assigned",
        ),
        (
            "@system\ndef w(missing: Nothing) -> f64:\n    return 1.0\n".to_string(),
            "not a Frame class",
        ),
        (
            "@system(\"no.such.thing\")\ndef w(x) -> f64:\n    return x\n".to_string(),
            "no component `no.such.thing`",
        ),
        (
            "class S(State):\n    x: f64\n\n@system(\"wheels.rpm\")\ndef w(rpm, s: S) -> f64:\n    return rpm\n".to_string(),
            "needs a default",
        ),
    ] {
        let text = refuse(&source, &table);
        assert!(text.contains(needle), "expected {needle:?}, got:\n{text}");
    }
}

/// The differential rule, applied to a whole system rather than one
/// expression: the host feeds frames and compares the output frame.
#[test]
fn a_system_agrees_with_nox_field_by_field() {
    let source = format!(
        "{IMU}
@system
def watchdog(imu: Imu) -> Rate:
    scaled = imu.omega * 2.0 - imu.accel
    return Rate(rate=dot(scaled, scaled), hot=scaled[0] < 0.0)
"
    );
    let mut run = build(&source, &imu_table(), "watchdog");
    let omega = [0.125f64, -0.75, 1.5];
    let accel = [0.5f64, 0.25, -9.81];
    run.set("imu", "omega", &omega).set("imu", "accel", &accel);
    run.eval(0);

    let scaled = nox3(omega) * nox3([2.0; 3]) - nox3(accel);
    let squared = scaled * scaled;
    let want = squared
        .into_inner()
        .view()
        .buf()
        .iter()
        .fold(0.0, |acc, v| acc + v);
    assert_eq!(run.scalar("rate").to_bits(), want.to_bits());
    assert_eq!(run.flag("hot"), scaled.into_inner().view().buf()[0] < 0.0);
}
