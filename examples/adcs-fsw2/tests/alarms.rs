//! The mission's alarm end-to-end gate (`docs/alarms.md`): the `ADCS_RATE_HIGH`
//! alarm declared in `mission.py` — a shaped nox-tensor target
//! (`plant.sensors.gyro_b` element 1) — raises on the boot tumble and clears as the
//! controller detumbles, observed as wkt alarm Msgs on the alarm engine's registered
//! message channels (the exact records the downlink would ship to the panel).

#![cfg(not(miri))]

use metor_fsw_2::MsgIn;
use metor_fsw_2::metor_proto::types::ComponentId;
use metor_fsw_2::metor_proto_wkt::{AlarmCleared, AlarmDef, AlarmRaised, Severity};

/// Enough of the 120 Hz mission for the commissioning detumble to settle well below
/// the alarm's ±0.05 rad/s warning band (the convergence gate uses the same budget).
const CYCLES: usize = 4000;

#[test]
fn rate_alarm_raises_on_boot_tumble_and_clears() {
    let coord = match adcs_fsw2::build_sim_coordinator() {
        Ok(coord) => coord,
        Err(e) => {
            // Build plumbing unavailable (offline/sandboxed cargo): skip, like the
            // convergence test's own guard.
            eprintln!("skipping: build_sim_coordinator failed: {e}");
            return;
        }
    };

    let registry = coord.registry();
    let tap = |name: &str| {
        registry
            .get(ComponentId::new(name))
            .unwrap_or_else(|| panic!("registry entry `{name}`"))
            .view()
            .expect("a reader slot is available")
    };
    let mut defs: MsgIn<AlarmDef> = MsgIn::new(tap("alarms.AlarmDef"));
    let mut raised: MsgIn<AlarmRaised> = MsgIn::new(tap("alarms.AlarmRaised"));
    let mut cleared: MsgIn<AlarmCleared> = MsgIn::new(tap("alarms.AlarmCleared"));

    let coord = stellarator::run(|| async move {
        let mut coord = coord;
        coord.run_for(CYCLES).await;
        coord
    });
    assert!(coord.stopped().is_empty(), "no system hard-stopped");

    // The def broadcast: the panel's plot limit lines are the firing thresholds.
    let mut got_defs = Vec::new();
    defs.drain(|d| got_defs.push(d));
    assert_eq!(
        got_defs.len(),
        2,
        "both mission alarms broadcast defs: {got_defs:?}"
    );
    let def = got_defs
        .iter()
        .find(|d| d.id == "ADCS_RATE_HIGH")
        .expect("the rate alarm's def");
    let target = def.target.as_ref().expect("targeted alarm");
    assert_eq!(
        target.component_id,
        ComponentId::new("plant.sensors.gyro_b")
    );
    assert_eq!(target.element_index, Some(1));
    assert_eq!(
        def.limits.len(),
        4,
        "warn/crit × upper/lower: {:?}",
        def.limits
    );
    // The wheel-momentum alarm's nested-array component path (instance `plant` + frame
    // `wheels` + field `wheels` — the name doubles) round-trips into the def. Its live
    // firing is exercised by `tests/momentum.rs`, which preloads the wheels into a
    // (scaled) band.
    let wheel_def = got_defs
        .iter()
        .find(|d| d.id == "RW_MOMENTUM_HIGH")
        .expect("the wheel-momentum alarm's def");
    let target = wheel_def.target.as_ref().expect("targeted alarm");
    assert_eq!(
        target.component_id,
        ComponentId::new("plant.wheels.wheels.0.ang_momentum")
    );
    assert_eq!(target.element_index, Some(0));

    // The boot tumble (~0.087 rad/s body-Y) breaches the ±0.05 warning band right
    // away. (The commissioning slew can transiently re-fire — and even escalate one
    // occurrence to Critical past ±0.15 — before settling; a re-raise of the SAME
    // occurrence is an escalation, not a new firing.)
    let mut got_raised = Vec::new();
    raised.drain(|r| got_raised.push(r));
    assert!(!got_raised.is_empty(), "the boot tumble raised");
    assert_eq!(got_raised[0].def_id, "ADCS_RATE_HIGH");
    assert_eq!(got_raised[0].severity, Severity::Warning);
    let v = got_raised[0].value.expect("raise carries the value");
    assert!(v.abs() > 0.05, "breaching value: {v}");

    // Detumble cleared every raised occurrence: the distinct occurrence sets match
    // exactly (escalation re-raises collapse onto their occurrence).
    let mut raised_occ: Vec<u64> = got_raised.iter().map(|r| r.occurrence).collect();
    raised_occ.dedup();
    raised_occ.sort_unstable();
    let mut cleared_occ = Vec::new();
    cleared.drain(|c| cleared_occ.push(c.occurrence));
    cleared_occ.sort_unstable();
    assert_eq!(
        raised_occ, cleared_occ,
        "every occurrence cleared: {got_raised:?}"
    );
}
