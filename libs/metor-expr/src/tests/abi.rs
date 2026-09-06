//! P3: the region ABI — describe, state defaults, and state across a rebuild.
//!
//! The rebuild tests do the whole loop the panel will do: compile, run a
//! system until its state means something, edit the source, compile again,
//! seed the new instance from the old one's slots, and check that the filter
//! picks up where it left off rather than starting over.

use wasmi::{Instance, Store, Val};

use super::instantiate;
use crate::state::{Slot, Snapshot, StateKey};
use crate::{Manifest, Program, Ty, compile_expr, compile_module, describe, state};

use super::systems::{Table, imu_table};

/// An instance with the state accessors a rebuild needs.
struct Live {
    store: Store<()>,
    instance: Instance,
    program: Program,
}

impl Live {
    fn new(program: Program) -> Self {
        let (store, instance) = instantiate(&program.wasm, 100_000_000);
        Live {
            store,
            instance,
            program,
        }
    }

    fn call(&mut self, name: &str, args: &[Val]) -> i32 {
        let func = self
            .instance
            .get_func(&self.store, name)
            .unwrap_or_else(|| panic!("missing {name}"));
        let mut out = [Val::I32(0)];
        func.call(&mut self.store, args, &mut out).unwrap();
        match out[0] {
            Val::I32(v) => v,
            ref other => panic!("expected an i32, got {other:?}"),
        }
    }

    fn state_ptr(&mut self, slot: &Slot) -> u32 {
        let name = format!(
            "{}_state_ptr",
            self.program.manifest.systems[slot.system].name
        );
        self.call(&name, &[Val::I32(slot.index as i32)]) as u32
    }

    fn arg_ptr(&mut self, system: usize, port: usize) -> u32 {
        let name = format!("{}_arg_ptr", self.program.manifest.systems[system].name);
        self.call(&name, &[Val::I32(port as i32)]) as u32
    }

    fn ret_ptr(&mut self, system: usize) -> u32 {
        let name = format!("{}_ret_ptr", self.program.manifest.systems[system].name);
        self.call(&name, &[]) as u32
    }

    fn read(&mut self, at: u32, bytes: u32) -> Vec<u8> {
        let memory = self.instance.get_memory(&self.store, "memory").unwrap();
        let mut out = vec![0u8; bytes as usize];
        memory.read(&self.store, at as usize, &mut out).unwrap();
        out
    }

    fn write(&mut self, at: u32, bytes: &[u8]) {
        let memory = self.instance.get_memory(&self.store, "memory").unwrap();
        memory.write(&mut self.store, at as usize, bytes).unwrap();
    }

    fn feed(&mut self, value: f64) -> f64 {
        let at = self.arg_ptr(0, 0);
        self.write(at, &value.to_le_bytes());
        let name = format!("{}_eval", self.program.manifest.systems[0].name);
        assert_eq!(self.call(&name, &[Val::I64(0)]), 0);
        let at = self.ret_ptr(0);
        f64::from_le_bytes(self.read(at, 8).try_into().unwrap())
    }

    /// Read every state field out, keyed the way a rebuild will look them up.
    fn snapshot(&mut self) -> Snapshot {
        let entries = state::slots(&self.program.manifest)
            .into_iter()
            .map(|slot| {
                let at = self.state_ptr(&slot);
                (slot.key.clone(), self.read(at, slot.bytes))
            })
            .collect();
        Snapshot { entries }
    }

    /// Seed from a snapshot, marking every stateful system seeded so the first
    /// evaluation does not write its defaults over what just arrived.
    fn seed(&mut self, snapshot: &Snapshot) {
        for (slot, bytes) in snapshot.restore(&self.program.manifest) {
            let at = self.state_ptr(&slot);
            self.write(at, bytes);
        }
        for (system, index) in state::guards(&self.program.manifest) {
            let name = format!("{}_state_ptr", self.program.manifest.systems[system].name);
            let at = self.call(&name, &[Val::I32(index as i32)]) as u32;
            self.write(at, &1i32.to_le_bytes());
        }
    }
}

fn embedded(program: &Program) -> Manifest {
    let (mut store, instance) = instantiate(&program.wasm, 10_000_000);
    let mut read = |name: &str| {
        let func = instance.get_func(&store, name).expect("the ABI export");
        let mut out = [Val::I32(0)];
        func.call(&mut store, &[], &mut out).unwrap();
        match out[0] {
            Val::I32(v) => v,
            ref other => panic!("expected an i32, got {other:?}"),
        }
    };
    assert_eq!(read("expr_abi_version") as u32, crate::COMPILER_VERSION);
    let (len, at) = (read("expr_describe"), read("expr_manifest_ptr"));

    let memory = instance.get_memory(&store, "memory").unwrap();
    let mut bytes = vec![0u8; len as usize];
    memory.read(&store, at as usize, &mut bytes).unwrap();
    describe(&bytes).expect("the embedded manifest decodes")
}

/// Describe *is* the signature: the manifest the compiler handed back and the
/// one the module carries are the same object.
#[test]
fn the_embedded_manifest_equals_the_host_side_one() {
    let sources = [
        "def f(x: f64) -> f64:\n    return x * 2.0\n",
        "class Lp(State):\n    filtered: f64 = 0.25\n\n@system(\"wheels.rpm\")\ndef lp(rpm, s: Lp) -> f64:\n    s.filtered = 0.5 * rpm + 0.5 * s.filtered\n    return s.filtered\n",
    ];
    for source in sources {
        let program = compile_module(source, &imu_table()).unwrap();
        assert_eq!(embedded(&program), program.manifest);
    }

    let program = compile_expr("adcs.omega_b * 100.0", &imu_table()).unwrap();
    assert_eq!(embedded(&program), program.manifest);
}

const LOWPASS: &str = "\
class Lp(State):
    filtered: f64 = 10.0

@system(\"wheels.rpm\")
def lp(rpm, s: Lp) -> f64:
    s.filtered = 0.5 * rpm + 0.5 * s.filtered
    return s.filtered
";

/// A state field comes up at its annotation, without the host walking
/// anything at instantiation.
#[test]
fn state_starts_at_its_declared_default() {
    let mut live = Live::new(compile_module(LOWPASS, &imu_table()).unwrap());
    assert_eq!(live.feed(0.0), 5.0);
    assert_eq!(live.feed(0.0), 2.5);

    // A default of zero needs no seeding, and must still be zero.
    let zeroed = LOWPASS.replace("= 10.0", "= 0.0");
    let mut live = Live::new(compile_module(&zeroed, &imu_table()).unwrap());
    assert_eq!(live.feed(4.0), 2.0);
}

#[test]
fn a_tensor_state_field_is_filled_with_its_default() {
    let source = "\
class Bias(State):
    offset: Tensor[f64, 3] = 1.5

@system(\"adcs.omega_b\")
def debias(omega_b, s: Bias) -> Tensor[f64, 3]:
    return omega_b - s.offset
";
    let program = compile_module(source, &imu_table()).unwrap();
    let mut live = Live::new(program);
    let at = live.arg_ptr(0, 0);
    let bytes: Vec<u8> = [1.0f64, 2.0, 3.0]
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect();
    live.write(at, &bytes);
    assert_eq!(live.call("debias_eval", &[Val::I64(0)]), 0);
    let at = live.ret_ptr(0);
    let out: Vec<f64> = live
        .read(at, 24)
        .chunks_exact(8)
        .map(|c| f64::from_le_bytes(c.try_into().unwrap()))
        .collect();
    assert_eq!(out, vec![-0.5, 0.5, 1.5]);
}

/// The rebuild loop: an edit that leaves the state triple alone carries the
/// filter's memory across, and the seeded instance never re-runs its defaults.
#[test]
fn state_survives_a_rebuild_that_keeps_the_triple() {
    let mut old = Live::new(compile_module(LOWPASS, &imu_table()).unwrap());
    old.feed(100.0);
    let carried = old.feed(100.0);
    let snapshot = old.snapshot();

    let edited = LOWPASS.replace("0.5 * rpm + 0.5", "0.25 * rpm + 0.75");
    let mut new = Live::new(compile_module(&edited, &imu_table()).unwrap());
    assert_eq!(snapshot.restore(&new.program.manifest).len(), 1);
    new.seed(&snapshot);
    assert_eq!(new.feed(100.0), 0.25 * 100.0 + 0.75 * carried);
}

/// A field whose name or shape changed is a different field, and resets. The
/// rest of the program is untouched, which is the whole compatibility rule.
#[test]
fn a_changed_triple_resets_that_field_and_nothing_else() {
    let mut old = Live::new(compile_module(LOWPASS, &imu_table()).unwrap());
    old.feed(100.0);
    let snapshot = old.snapshot();

    for edited in [
        LOWPASS.replace("filtered", "smoothed"),
        LOWPASS
            .replace("filtered: f64 = 10.0", "filtered: Tensor[f64, 3] = 10.0")
            .replace("return s.filtered", "return s.filtered[0]")
            .replace(
                "s.filtered = 0.5 * rpm + 0.5 * s.filtered",
                "s.filtered = s.filtered * 0.5",
            ),
    ] {
        let new = compile_module(&edited, &imu_table()).unwrap();
        assert!(
            snapshot.restore(&new.manifest).is_empty(),
            "a changed triple must not be restored"
        );
    }

    // Same triple, different system name: also a different field.
    let renamed = LOWPASS.replace("def lp(", "def lowpass(");
    let new = compile_module(&renamed, &imu_table()).unwrap();
    assert!(snapshot.restore(&new.manifest).is_empty());
}

#[test]
fn the_snapshot_keys_are_the_design_docs_triple() {
    let program = compile_module(LOWPASS, &imu_table()).unwrap();
    assert_eq!(
        state::slots(&program.manifest),
        vec![Slot {
            key: StateKey {
                system: "lp".into(),
                field: "filtered".into(),
                ty: Ty::F64,
            },
            system: 0,
            index: 0,
            bytes: 8,
        }]
    );
    assert_eq!(state::guards(&program.manifest), vec![(0, 1)]);

    let stateless =
        compile_expr("wheels.rpm * 2.0", &Table::new(&[("wheels.rpm", Ty::F64)])).unwrap();
    assert!(state::slots(&stateless.manifest).is_empty());
    assert!(state::guards(&stateless.manifest).is_empty());
}
