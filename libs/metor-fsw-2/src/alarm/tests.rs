use metor_proto_wkt::{LimitKind, Severity};

use super::{AlarmEval, AlarmSpec, BandSpec, EvalEvent, RawAlarmSpec, TargetSpec};

fn raw(warning: Option<BandSpec>, critical: Option<BandSpec>) -> RawAlarmSpec {
    RawAlarmSpec {
        id: "T".into(),
        name: "Test".into(),
        description: String::new(),
        target: TargetSpec {
            component: "sys.frame.field".into(),
            element: None,
        },
        warning,
        critical,
        debounce: None,
        hysteresis: None,
        latching: None,
        severity: None,
    }
}

fn band(above: Option<f64>, below: Option<f64>) -> Option<BandSpec> {
    Some(BandSpec { above, below })
}

fn spec(raw: RawAlarmSpec) -> AlarmSpec {
    AlarmSpec::try_from(raw).expect("valid spec")
}

/// A simple upper warning at 1.0, critical at 2.0, with the given knobs.
fn eval_with(debounce: u32, hysteresis: f64, latching: bool) -> AlarmEval {
    let mut r = raw(band(Some(1.0), None), band(Some(2.0), None));
    r.debounce = Some(debounce);
    r.hysteresis = Some(hysteresis);
    r.latching = Some(latching);
    AlarmEval::new(&spec(r))
}

/// Step with a monotonically-minting allocator starting at 100.
fn step(eval: &mut AlarmEval, v: f64, next: &mut u64) -> Option<EvalEvent> {
    eval.step(v, &mut || {
        *next += 1;
        *next
    })
}

// ---------------------------------------------------------------------------
// AlarmEval
// ---------------------------------------------------------------------------

/// A breach must hold `debounce` consecutive cycles to raise; a recovery likewise
/// to clear. A blip shorter than the debounce does neither.
#[test]
fn debounce_gates_raise_and_clear() {
    let mut eval = eval_with(3, 0.0, false);
    let mut occ = 100;

    // Two breaching cycles, then a recovery: no raise.
    assert_eq!(step(&mut eval, 1.5, &mut occ), None);
    assert_eq!(step(&mut eval, 1.5, &mut occ), None);
    assert_eq!(step(&mut eval, 0.0, &mut occ), None);

    // Three consecutive breaches raise (the counter restarted).
    assert_eq!(step(&mut eval, 1.5, &mut occ), None);
    assert_eq!(step(&mut eval, 1.5, &mut occ), None);
    assert_eq!(
        step(&mut eval, 1.5, &mut occ),
        Some(EvalEvent::Raise {
            occurrence: 101,
            severity: Severity::Warning
        })
    );

    // Two in-band cycles, then a breach: the clear counter restarts too.
    assert_eq!(step(&mut eval, 0.0, &mut occ), None);
    assert_eq!(step(&mut eval, 0.0, &mut occ), None);
    assert_eq!(step(&mut eval, 1.5, &mut occ), None);

    // Three consecutive recoveries clear.
    assert_eq!(step(&mut eval, 0.0, &mut occ), None);
    assert_eq!(step(&mut eval, 0.0, &mut occ), None);
    assert_eq!(
        step(&mut eval, 0.0, &mut occ),
        Some(EvalEvent::Clear { occurrence: 101 })
    );

    // A re-fire mints a fresh occurrence.
    step(&mut eval, 1.5, &mut occ);
    step(&mut eval, 1.5, &mut occ);
    assert_eq!(
        step(&mut eval, 1.5, &mut occ),
        Some(EvalEvent::Raise {
            occurrence: 102,
            severity: Severity::Warning
        })
    );
}

/// Values between a threshold and its hysteresis margin advance neither counter —
/// boundary chatter neither raises nor clears.
#[test]
fn hysteresis_dead_zone_resets_both_counters() {
    let mut eval = eval_with(2, 0.2, false);
    let mut occ = 0;

    // One breach, then a dead-zone value (1.0 - 0.2 < 0.9 <= 1.0): raise counter resets.
    assert_eq!(step(&mut eval, 1.5, &mut occ), None);
    assert_eq!(step(&mut eval, 0.9, &mut occ), None);
    assert_eq!(step(&mut eval, 1.5, &mut occ), None); // count restarted at 1
    assert!(step(&mut eval, 1.5, &mut occ).is_some()); // now raises

    // Active: one recovery, then dead-zone chatter — the clear counter resets, and
    // it takes a fresh full debounce of comfortable recovery to clear.
    assert_eq!(step(&mut eval, 0.5, &mut occ), None);
    assert_eq!(step(&mut eval, 0.9, &mut occ), None);
    assert_eq!(step(&mut eval, 0.5, &mut occ), None);
    assert_eq!(step(&mut eval, 0.5, &mut occ), Some(EvalEvent::Clear { occurrence: 1 }));
}

/// A NaN value is dead-zone: the alarm freezes where it is.
#[test]
fn nan_freezes_the_alarm() {
    let mut eval = eval_with(1, 0.0, false);
    let mut occ = 0;
    assert!(step(&mut eval, 1.5, &mut occ).is_some());
    assert_eq!(step(&mut eval, f64::NAN, &mut occ), None);
    // Still active: an in-band value clears it.
    assert_eq!(step(&mut eval, 0.0, &mut occ), Some(EvalEvent::Clear { occurrence: 1 }));
}

/// Escalation re-raises the SAME occurrence at the higher severity; severity only
/// ratchets up (a drop back to the warning band re-emits nothing).
#[test]
fn escalation_reuses_the_occurrence_and_ratchets() {
    let mut eval = eval_with(1, 0.0, false);
    let mut occ = 0;

    assert_eq!(
        step(&mut eval, 1.5, &mut occ),
        Some(EvalEvent::Raise {
            occurrence: 1,
            severity: Severity::Warning
        })
    );
    // Into the critical band: same occurrence, escalated.
    assert_eq!(
        step(&mut eval, 2.5, &mut occ),
        Some(EvalEvent::Raise {
            occurrence: 1,
            severity: Severity::Critical
        })
    );
    // Steady critical, then back down to warning-band breach: nothing re-emitted.
    assert_eq!(step(&mut eval, 2.5, &mut occ), None);
    assert_eq!(step(&mut eval, 1.5, &mut occ), None);
    assert_eq!(step(&mut eval, 0.0, &mut occ), Some(EvalEvent::Clear { occurrence: 1 }));
}

/// A breach past both bands at once raises straight at critical.
#[test]
fn worst_band_wins_at_raise() {
    let mut eval = eval_with(1, 0.0, false);
    let mut occ = 0;
    assert_eq!(
        step(&mut eval, 3.0, &mut occ),
        Some(EvalEvent::Raise {
            occurrence: 1,
            severity: Severity::Critical
        })
    );
}

/// Latching: recovered-then-acked clears on the ack; the recovery alone holds.
#[test]
fn latching_recover_then_ack() {
    let mut eval = eval_with(1, 0.0, true);
    let mut occ = 0;
    assert!(step(&mut eval, 1.5, &mut occ).is_some());

    // Recovered, but latched: no clear, cycle after cycle.
    assert_eq!(step(&mut eval, 0.0, &mut occ), None);
    assert_eq!(step(&mut eval, 0.0, &mut occ), None);

    assert_eq!(eval.ack(1), Some(EvalEvent::Clear { occurrence: 1 }));
}

/// Latching: acked-then-recovered clears when the recovery debounce completes.
#[test]
fn latching_ack_then_recover() {
    let mut eval = eval_with(2, 0.0, true);
    let mut occ = 0;
    step(&mut eval, 1.5, &mut occ);
    assert!(step(&mut eval, 1.5, &mut occ).is_some());

    assert_eq!(eval.ack(1), None); // still breaching — ack recorded, no clear
    assert_eq!(step(&mut eval, 0.0, &mut occ), None);
    assert_eq!(step(&mut eval, 0.0, &mut occ), Some(EvalEvent::Clear { occurrence: 1 }));
}

/// A re-breach after recovery un-recovers a latched alarm: the earlier recovery no
/// longer satisfies the clear.
#[test]
fn latching_rebreach_unrecovers() {
    let mut eval = eval_with(1, 0.0, true);
    let mut occ = 0;
    assert!(step(&mut eval, 1.5, &mut occ).is_some());
    assert_eq!(step(&mut eval, 0.0, &mut occ), None); // recovered (unacked)
    assert_eq!(step(&mut eval, 1.5, &mut occ), None); // re-breach: recovery voided
    assert_eq!(eval.ack(1), None); // acked, but no longer recovered
    assert_eq!(step(&mut eval, 0.0, &mut occ), Some(EvalEvent::Clear { occurrence: 1 }));
}

/// Acks for a stale or unknown occurrence are dropped; on a non-latching alarm an
/// ack never clears.
#[test]
fn stale_and_nonlatching_acks_are_inert() {
    let mut eval = eval_with(1, 0.0, false);
    let mut occ = 0;
    assert_eq!(eval.ack(7), None); // nothing active
    assert!(step(&mut eval, 1.5, &mut occ).is_some());
    assert_eq!(eval.ack(999), None); // wrong occurrence
    assert_eq!(eval.ack(1), None); // non-latching: recorded, no clear
    assert_eq!(step(&mut eval, 0.0, &mut occ), Some(EvalEvent::Clear { occurrence: 1 }));
}

/// A lower (`below=`) threshold breaches downward and needs the margin upward.
#[test]
fn below_thresholds_breach_downward() {
    let mut r = raw(band(None, Some(-1.0)), None);
    r.hysteresis = Some(0.1);
    let mut eval = AlarmEval::new(&spec(r));
    let mut occ = 0;

    assert!(step(&mut eval, -1.5, &mut occ).is_some());
    assert_eq!(step(&mut eval, -0.95, &mut occ), None); // dead zone (needs >= -0.9)
    assert_eq!(step(&mut eval, -0.5, &mut occ), Some(EvalEvent::Clear { occurrence: 1 }));
}

// ---------------------------------------------------------------------------
// Spec validation + to_def
// ---------------------------------------------------------------------------

#[test]
fn try_from_rejects_bad_specs() {
    // No band at all.
    assert!(AlarmSpec::try_from(raw(None, None)).is_err());
    // A band with neither side.
    assert!(AlarmSpec::try_from(raw(band(None, None), None)).is_err());
    // Critical tighter than warning.
    assert!(AlarmSpec::try_from(raw(band(Some(1.0), None), band(Some(0.5), None))).is_err());
    assert!(AlarmSpec::try_from(raw(band(None, Some(-1.0)), band(None, Some(-0.5)))).is_err());
    // Non-finite threshold.
    assert!(AlarmSpec::try_from(raw(band(Some(f64::NAN), None), None)).is_err());
    // debounce = 0.
    let mut r = raw(band(Some(1.0), None), None);
    r.debounce = Some(0);
    assert!(AlarmSpec::try_from(r).is_err());
    // Negative hysteresis.
    let mut r = raw(band(Some(1.0), None), None);
    r.hysteresis = Some(-0.1);
    assert!(AlarmSpec::try_from(r).is_err());
    // Empty id.
    let mut r = raw(band(Some(1.0), None), None);
    r.id = String::new();
    assert!(AlarmSpec::try_from(r).is_err());
}

/// Defaults: debounce 1, hysteresis 0, non-latching, severity = lowest configured band.
#[test]
fn spec_defaults() {
    let s = spec(raw(band(Some(1.0), None), None));
    assert_eq!(s.debounce, 1);
    assert_eq!(s.hysteresis, 0.0);
    assert!(!s.latching);
    assert_eq!(s.severity, Severity::Warning);

    let s = spec(raw(None, band(Some(2.0), None)));
    assert_eq!(s.severity, Severity::Critical);
}

/// `to_def` maps each configured threshold to one display limit at the firing value.
#[test]
fn to_def_mirrors_the_firing_thresholds() {
    let mut r = raw(band(Some(0.5), Some(-0.5)), band(Some(1.0), Some(-1.0)));
    r.target.element = Some(1);
    let def = spec(r).to_def();

    assert_eq!(def.id, "T");
    let target = def.target.expect("target set");
    assert_eq!(
        target.component_id,
        metor_proto::types::ComponentId::new("sys.frame.field")
    );
    assert_eq!(target.element_index, Some(1));

    let mut got: Vec<(LimitKind, f64, Severity)> =
        def.limits.iter().map(|l| (l.kind, l.value, l.severity)).collect();
    got.sort_by(|a, b| a.1.total_cmp(&b.1));
    assert_eq!(
        got,
        vec![
            (LimitKind::Lower, -1.0, Severity::Critical),
            (LimitKind::Lower, -0.5, Severity::Warning),
            (LimitKind::Upper, 0.5, Severity::Warning),
            (LimitKind::Upper, 1.0, Severity::Critical),
        ]
    );
}
