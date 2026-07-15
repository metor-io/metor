//! Momentum-management acceptance: with the wheels preloaded, the cross-product
//! desaturation law bleeds the stored momentum through the magnetorquers while the
//! controller keeps pointing; with desat disabled the preload only creeps down on bearing
//! friction. The same run is the **live** guard that the `RW_MOMENTUM_HIGH` alarm's
//! nested-array target path (`plant.wheels.wheels.0.ang_momentum`) resolves — the alarm
//! engine only logs a miss, so `tests/alarms.rs`'s def assertions alone can't prove
//! resolution.
//!
//! Both runs use the static path (plant/nav/ctrl as rlibs, like `closed_loop.rs`'s parity
//! reference) off a patched `mission.py`: the wheel preload set to a controllable level,
//! the alarm band scaled around it, and `k_desat` set per run. Gated off `miri`
//! (build_artifacts still builds the sequence cdylibs the `mode` slot loads).

#![cfg(not(miri))]

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use adcs_contracts::{BodyState, V3, Wheels};
use metor_fsw_2::metor_proto::types::ComponentId;
use metor_fsw_2::metor_proto_wkt::AlarmRaised;
use metor_fsw_2::wiring::Registry;
use metor_fsw_2::wiring::{ParamSource, Wiring, build_artifacts, eval_python_mission, resolve};
use metor_fsw_2::{BuildOptions, Coordinator, Input, MsgIn};

fn mission_py() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("mission.py")
}

mod common;

/// The value-tree params of the named system, for in-place patching of the evaluated IR.
fn params_of<'a>(
    wiring: &'a mut Wiring,
    name: &str,
) -> &'a mut serde_json::Map<String, serde_json::Value> {
    let sys = wiring
        .systems
        .iter_mut()
        .find(|s| s.name == name)
        .unwrap_or_else(|| panic!("system `{name}`"));
    match &mut sys.params {
        ParamSource::Value(serde_json::Value::Object(map)) => map,
        _ => panic!("system `{name}` carries a params value tree"),
    }
}

/// 75 s of mission at 120 Hz. The torquers' authority against the ~40 µT field is
/// ~1e-5 N·m at the 0.2 A·m² clamp, so the window is enough to dump a few 1e-4 N·m·s of
/// the field-perpendicular preload — well above the no-desat baseline's drift.
const CYCLES: usize = 9000;

/// Per-wheel stored-momentum preload (N·m·s). Sized by controllability, not the mission
/// alarm band: the gyroscopic coupling `ω × h_w` during the commissioning slew (ω ~ 0.05)
/// must stay well under the wheels' 2e-3 N·m authority, capping |Σh| near 0.017 — so the
/// run patches the alarm band down around the preload instead of preloading into the
/// mission's near-saturation band (where the spacecraft is genuinely uncontrollable).
const PRELOAD: f64 = 0.01;

/// The `mission.kdl` mission with the preload cranked, `k_desat` set for this run, and the
/// boot tumble becalmed — with ~0.06 N·m·s stored, the gyroscopic coupling `ω × h_w` of the
/// full 0.15 rad/s tumble (~6e-3 N·m) dwarfs the wheels' 2e-3 N·m authority and the
/// spacecraft precesses uncontrollably (the very failure desat exists to prevent; not this
/// test's subject). Plant/nav/ctrl link statically. `None` if the build plumbing is
/// unavailable (offline/sandboxed cargo), so callers skip rather than fail spuriously.
fn build_static(k_desat: f64) -> Option<Coordinator> {
    if !common::ensure_stubs() {
        return None;
    }
    let mut wiring = match eval_python_mission(&mission_py()) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("skipping: mission.py did not evaluate: {e}");
            return None;
        }
    };
    // Preload the wheels, becalm the boot tumble, and set this run's desat gain on the
    // evaluated IR's value tree.
    let plant = params_of(&mut wiring, "plant");
    plant.insert("init_wheel_h".into(), PRELOAD.into());
    plant.insert("init_rate".into(), 0.02.into());
    params_of(&mut wiring, "ctrl").insert("k_desat".into(), k_desat.into());
    // The RW_MOMENTUM_HIGH band, scaled around the (controllable) preload.
    let alarms = params_of(&mut wiring, "alarms");
    let band = alarms["alarm"]
        .as_array_mut()
        .expect("alarm list")
        .iter_mut()
        .find(|a| a["id"] == "RW_MOMENTUM_HIGH")
        .expect("the RW_MOMENTUM_HIGH alarm");
    band["warning"] = serde_json::json!({ "above": 0.008, "below": -0.008 });
    band["critical"] = serde_json::json!({ "above": 0.012, "below": -0.012 });
    if let Err(e) = build_artifacts(&mut wiring, &BuildOptions::default()) {
        eprintln!("skipping: build_artifacts failed: {e}");
        return None;
    }
    for spec in &mut wiring.systems {
        // Static rlib systems, in-process (test binaries can't host a process worker).
        spec.artifact = None;
        spec.process = false;
    }
    let mut registry = Registry::with_builtins();
    registry.register_pack(adcs_systems::pack());
    Some(resolve(&wiring, &registry).expect("resolve the patched mission"))
}

/// One run's measurements: total wheel momentum |Σh| at the first and last cycle, the
/// final body rate, and whether `RW_MOMENTUM_HIGH` raised.
struct Measure {
    h0: f64,
    hf: f64,
    rate_f: f64,
    momentum_alarm: bool,
}

/// Run `coord` for [`CYCLES`], sampling `plant.wheels` + `plant.body` every cycle (an
/// idle view would backpressure the ring — the sampler must interleave, like
/// `closed_loop.rs`'s) and draining the alarm channel at the end.
async fn run_and_measure(mut coord: Coordinator) -> Measure {
    let registry = coord.registry();
    let wheels_view: Input<Wheels> = Input::new(
        registry
            .view(ComponentId::new("plant.wheels"))
            .expect("plant.wheels is registered")
            .expect("a reader slot is available"),
    );
    let body_view: Input<BodyState> = Input::new(
        registry
            .view(ComponentId::new("plant.body"))
            .expect("plant.body is registered")
            .expect("a reader slot is available"),
    );
    let mut raised: MsgIn<AlarmRaised> = MsgIn::new(
        registry
            .get(ComponentId::new("alarms.AlarmRaised"))
            .expect("alarms.AlarmRaised is registered")
            .view()
            .expect("a reader slot is available"),
    );

    let samples = Rc::new(RefCell::new((Vec::<f64>::new(), 0.0f64)));
    let captured = samples.clone();
    let sampler = stellarator::spawn(async move {
        let mut wheels = wheels_view;
        let mut body = body_view;
        loop {
            stellarator::yield_now().await;
            let mut s = captured.borrow_mut();
            if let Ok(Some(w)) = wheels.latest() {
                let h: V3 = w
                    .get()
                    .wheels
                    .iter()
                    .fold(V3::zeros(), |acc, wheel| acc + wheel.ang_momentum);
                let h: f64 = h.norm().into_buf();
                s.0.push(h);
            }
            if let Ok(Some(b)) = body.latest() {
                s.1 = b.get().omega_b.norm().into_buf();
            }
        }
    })
    .drop_guard();

    coord.run_for(CYCLES).await;
    drop(sampler);

    let mut momentum_alarm = false;
    raised.drain(|r| momentum_alarm |= r.def_id == "RW_MOMENTUM_HIGH");

    let (h, rate_f) = &*samples.borrow();
    Measure {
        h0: *h.first().expect("sampler captured the initial wheels"),
        hf: *h.last().expect("sampler captured the final wheels"),
        rate_f: *rate_f,
        momentum_alarm,
    }
}

#[stellarator::test]
async fn desat_dumps_preloaded_momentum() {
    // Run (a): desat cranked (authority-limited by the 0.2 A·m² clamp regardless).
    let Some(coord) = build_static(0.05) else {
        return;
    };
    let a = run_and_measure(coord).await;
    // Run (b): desat off — the baseline the dump is measured against (the pointing slew
    // also exchanges a little momentum with the body, so the bar is comparative).
    let Some(coord) = build_static(0.0) else {
        return;
    };
    let b = run_and_measure(coord).await;

    println!(
        "[desat on ] |h| {:.5} -> {:.5} N·m·s; final rate {:.4} rad/s; alarm raised: {}",
        a.h0, a.hf, a.rate_f, a.momentum_alarm
    );
    println!("[desat off] |h| {:.5} -> {:.5} N·m·s", b.h0, b.hf);

    // The preload is real: |Σh| starts near √3·PRELOAD in both runs.
    assert!(a.h0 > 0.015, "preloaded momentum present: {}", a.h0);
    // The alarm's nested-array target resolved and fired on the preload — the live check
    // of the `plant.wheels.wheels.0.ang_momentum` path.
    assert!(a.momentum_alarm, "RW_MOMENTUM_HIGH raised on the preload");
    // Desat rides along without destabilizing the mission: the boot tumble still damps.
    assert!(a.rate_f < 0.05, "mission still converges under desat: {}", a.rate_f);
    // The torquers dumped a measurable slice of the preload relative to the no-desat
    // baseline (only the field-perpendicular component is reachable in the window).
    assert!(a.hf < a.h0, "stored momentum decreased: {} -> {}", a.h0, a.hf);
    let dumped = b.hf - a.hf;
    assert!(
        dumped > 2e-4,
        "desat dumped a measurable margin over the baseline: Δ = {dumped:.2e} N·m·s"
    );
}
