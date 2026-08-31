//! End-to-end test of Python systems on the vehicle: a captured `@system`
//! program provisions into an ordinary wasm pack artifact
//! ([`provision_artifacts`]'s program arm compiles it — no fixture crate, no
//! nested cargo), resolves through the wired wasm arm with its edges
//! synthesized from the baked expr manifest, and runs in the cyclic loop
//! with its output as real telemetry.
//!
//! The second test is the fault path: a runaway system burns its fuel grant,
//! latches stopped, surfaces through the coordinator's health vocabulary —
//! and the vehicle keeps cycling.

#![cfg(not(miri))]

use metor_fsw_2::ir::ArtifactKind;
use metor_fsw_2::metor_proto::types::{ComponentId, Timestamp};
use metor_fsw_2::{
    AllowedOccupantSpec, Artifact, ClockSpec, Frame, InitialOccupantSpec, Input, ParamSource,
    ProgramDecl, ProgramSpec, SlotInitState, SlotSpec, StopReason, SystemSpec, Wiring,
    WiringBuilder,
    wiring::{BuildOptions, Registry, provision_artifacts, resolve},
};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

// Host-side mirrors of the compiled frames, for reading the telemetry rings:
// an 8-byte timestamp then eight-byte slots, exactly the layout the compiler
// documents.

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Default)]
#[repr(C)]
#[metor_fsw(name = "beat")]
struct BeatOut {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    v: f64,
}

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Default)]
#[repr(C)]
#[metor_fsw(name = "double")]
struct DoubleOut {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    double: f64,
}

/// Assemble the wiring the recorder would emit: the captured program, one
/// program-built wasm artifact, and one ordinary system spec per `@system`
/// addressing its entry by name.
fn python_wiring(artifact_id: &str, source: &str, decls: &[&str], cycle_rate: f64) -> Wiring {
    let mut wiring = WiringBuilder::new()
        .coordinator(
            cycle_rate,
            ClockSpec::Simulated {
                dt_secs: 1.0 / cycle_rate,
            },
        )
        .build();
    wiring.artifacts.push(Artifact {
        id: artifact_id.into(),
        kind: ArtifactKind::Wasm,
        crate_name: String::new(),
        lib: String::new(),
        path: None,
        prebuilt_dir: None,
        dist: None,
        manifest_hash: None,
        src: None,
    });
    wiring.program = Some(ProgramSpec {
        source: source.into(),
        decls: decls
            .iter()
            .map(|name| ProgramDecl {
                name: name.to_string(),
                src: None,
                offset: source
                    .find(&format!("def {name}"))
                    .or_else(|| source.find(&format!("class {name}")))
                    .expect("declaration present") as u32,
            })
            .collect(),
    });
    for decl in decls {
        if source.contains(&format!("def {decl}")) {
            wiring.systems.push(SystemSpec {
                name: decl.to_string(),
                ty: Some(decl.to_string()),
                artifact: Some(artifact_id.to_string()),
                params: ParamSource::None,
                process: false,
                src: None,
                scope: None,
                attach: None,
                layout: None,
                status: None,
                encompassing: false,
            });
        }
    }
    wiring
}

/// A rate-clocked source feeding an input-driven consumer over a synthesized
/// `Produced` edge: eval → provision (compile) → resolve → cycles →
/// telemetered values.
#[test]
fn a_python_program_provisions_resolves_and_runs() {
    const SRC: &str = "\
class Beat(Frame):
    v: f64

@system(rate=30.0)
def beat() -> Beat:
    return Beat(v=2.5)

@system
def double(beat: Beat) -> f64:
    return beat.v * 2.0
";
    let mut wiring = python_wiring("pyint_program", SRC, &["Beat", "beat", "double"], 120.0);
    provision_artifacts(&mut wiring, &BuildOptions::default())
        .expect("the program arm compiles the artifact without cargo");
    let path = wiring.artifacts[0].path.clone().expect("path filled");
    assert!(path.exists(), "compiled module written");
    let mut sidecar = path.clone().into_os_string();
    sidecar.push(".manifest");
    assert!(
        std::path::PathBuf::from(sidecar).exists(),
        "the .manifest sidecar rides next to the module"
    );

    let mut coord = resolve(&wiring, &Registry::with_builtins())
        .unwrap_or_else(|e| panic!("a Python-only target resolves: {e}"));
    let mut beat_view: Input<BeatOut> = Input::new(
        coord
            .registry()
            .view(ComponentId::new("beat.beat"))
            .expect("the source's output is registered for telemetry")
            .expect("a reader slot is available"),
    );
    let mut double_view: Input<DoubleOut> = Input::new(
        coord
            .registry()
            .view(ComponentId::new("double.double"))
            .expect("the consumer's output is registered for telemetry")
            .expect("a reader slot is available"),
    );

    // 8 cycles at 120 Hz with rate=30: the source fires on cycles 1 and 5.
    let coord = stellarator::run(|| async move {
        coord.run_for(8).await;
        coord
    });
    assert!(coord.stopped().is_empty(), "nothing hard-stopped");

    let mut beats = Vec::new();
    beat_view
        .drain(|record| beats.push((record.get().timestamp, record.get().v)))
        .expect("source ring intact");
    assert_eq!(
        beats.iter().map(|(_, v)| *v).collect::<Vec<_>>(),
        [2.5, 2.5],
        "rate=30 against 120 Hz fires every 4th cycle"
    );

    let mut doubles = Vec::new();
    double_view
        .drain(|record| doubles.push(record.get().double))
        .expect("consumer ring intact");
    assert_eq!(
        doubles,
        [5.0, 5.0],
        "the consumer fires exactly when its driving input moved, same cycle"
    );
}

/// Instance names belong to the registering `add`, not the declaration: a
/// scope-prefixed rename still resolves (the synthesized `Produced` edge
/// maps declaration to instance), and listing the consumer ahead of its
/// producer is the same stale-edge build error a native pair gets — step
/// order is the list order the adds chose.
#[test]
fn renamed_instances_wire_and_misordered_ones_fail_stale() {
    const SRC: &str = "\
class Beat(Frame):
    v: f64

@system(rate=30.0)
def beat() -> Beat:
    return Beat(v=2.5)

@system
def double(beat: Beat) -> f64:
    return beat.v * 2.0
";
    let rename = |wiring: &mut Wiring| {
        for spec in &mut wiring.systems {
            spec.name = format!("adcs.{}", spec.name);
        }
    };

    let mut wiring = python_wiring("pyname_program", SRC, &["Beat", "beat", "double"], 120.0);
    rename(&mut wiring);
    provision_artifacts(&mut wiring, &BuildOptions::default()).expect("compiles");
    let mut coord =
        resolve(&wiring, &Registry::with_builtins()).expect("renamed instances resolve");
    let mut doubles: Input<DoubleOut> = Input::new(
        coord
            .registry()
            .view(ComponentId::new("adcs.double.double"))
            .expect("telemetry announces under the instance name")
            .expect("a reader slot is available"),
    );
    let coord = stellarator::run(|| async move {
        coord.run_for(8).await;
        coord
    });
    assert!(coord.stopped().is_empty(), "{:?}", coord.stopped());
    let mut values = Vec::new();
    doubles
        .drain(|record| values.push(record.get().double))
        .expect("ring intact");
    assert_eq!(values, [5.0, 5.0], "the Produced edge found its instance");

    // Same program, consumer listed first: stale, exactly like native specs.
    let mut wiring = python_wiring("pyorder_program", SRC, &["Beat", "beat", "double"], 120.0);
    rename(&mut wiring);
    wiring.systems.reverse();
    provision_artifacts(&mut wiring, &BuildOptions::default()).expect("compiles");
    let err = resolve(&wiring, &Registry::with_builtins())
        .err()
        .expect("a consumer stepping before its producer is rejected");
    assert!(
        err.to_string().contains("registered before"),
        "the stale-edge diagnostic names the order: {err}"
    );
}

/// A runaway Python system exhausts its fuel grant: it stops, the
/// coordinator reports it in its health vocabulary, and the rest of the
/// target keeps cycling.
#[test]
fn a_runaway_python_system_degrades_and_the_loop_survives() {
    const SRC: &str = "\
@system(rate=120.0)
def steady() -> f64:
    return 1.0

@system(rate=120.0)
def runaway() -> f64:
    i = 0
    while i < 100000000:
        i = i + 1
    return float(i)
";
    let mut wiring = python_wiring("pyfault_program", SRC, &["steady", "runaway"], 120.0);
    // A grant far below what the loop needs, and plenty for everything else.
    wiring.coordinator.wasm_fuel_per_poll = Some(200_000);
    provision_artifacts(&mut wiring, &BuildOptions::default()).expect("compiles");

    let mut coord = resolve(&wiring, &Registry::with_builtins()).expect("resolves");
    let mut steady_view: Input<SteadyOut> = Input::new(
        coord
            .registry()
            .view(ComponentId::new("steady.steady"))
            .expect("registered")
            .expect("slot free"),
    );
    let coord = stellarator::run(|| async move {
        coord.run_for(6).await;
        coord
    });

    let stopped = coord.stopped();
    assert_eq!(stopped.len(), 1, "only the runaway stopped: {stopped:?}");
    assert_eq!(&*stopped[0].name, "runaway");
    assert_eq!(stopped[0].reason, StopReason::Panicked);

    let mut healthy = 0;
    steady_view
        .drain(|_| healthy += 1)
        .expect("healthy ring intact");
    assert_eq!(healthy, 6, "the loop kept cycling the healthy system");
}

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Default)]
#[repr(C)]
#[metor_fsw(name = "steady")]
struct SteadyOut {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    steady: f64,
}

/// The same artifact shape mounts as a slot occupant through the existing
/// runtime-slot machinery: the occupant loads at startup, gets the mount
/// tail appended (which the guest simply never opens), and publishes through
/// the slot's rings.
#[test]
fn a_python_entry_mounts_as_a_slot_occupant() {
    const SRC: &str = "@system(rate=60.0)\ndef beat() -> f64:\n    return 4.25\n";
    let mut wiring = python_wiring("pyslot_program", SRC, &["beat"], 120.0);
    // The entry occupies a slot instead of a wired position.
    wiring
        .systems
        .retain(|s| s.artifact.as_deref() != Some("pyslot_program"));
    wiring.slots.push(SlotSpec {
        name: "mode".into(),
        inputs: Vec::new(),
        outputs: vec!["beat".into()],
        allow: vec![AllowedOccupantSpec {
            occupant: "beat".into(),
            artifact: Some("pyslot_program".into()),
            params: ParamSource::None,
            src: None,
        }],
        initial: Some(InitialOccupantSpec {
            occupant: "beat".into(),
            state: SlotInitState::Running,
        }),
        process: false,
        src: None,
        scope: None,
        status: None,
    });
    provision_artifacts(&mut wiring, &BuildOptions::default()).expect("compiles");

    let mut coord = resolve(&wiring, &Registry::with_builtins())
        .unwrap_or_else(|e| panic!("a Python slot occupant resolves: {e}"));
    let mut beat_view: Input<BeatOnly> = Input::new(
        coord
            .registry()
            .view(ComponentId::new("mode.beat"))
            .expect("the slot output is registered")
            .expect("a reader slot is available"),
    );
    let coord = stellarator::run(|| async move {
        coord.run_for(8).await;
        coord
    });
    assert!(coord.stopped().is_empty(), "{:?}", coord.stopped());

    let mut beats = Vec::new();
    beat_view
        .drain(|record| beats.push(record.get().beat))
        .expect("slot ring intact");
    assert_eq!(
        beats,
        [4.25, 4.25, 4.25, 4.25],
        "rate=60 against 120 Hz fires every 2nd cycle through the slot"
    );
}

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Default)]
#[repr(C)]
#[metor_fsw(name = "beat")]
struct BeatOnly {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    beat: f64,
}
