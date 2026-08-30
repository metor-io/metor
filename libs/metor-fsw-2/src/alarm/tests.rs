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

/// An [`AlarmEval`] with an upper warning at 1.0 and critical at 2.0, plus
/// the given knobs.
fn eval_with(debounce: u32, hysteresis: f64, latching: bool) -> AlarmEval {
    let mut r = raw(band(Some(1.0), None), band(Some(2.0), None));
    r.debounce = Some(debounce);
    r.hysteresis = Some(hysteresis);
    r.latching = Some(latching);
    AlarmEval::new(&spec(r))
}

/// Step the evaluator, minting occurrence ids by incrementing `next`.
fn step(eval: &mut AlarmEval, v: f64, next: &mut u64) -> Option<EvalEvent> {
    eval.step(v, &mut || {
        *next += 1;
        *next
    })
}

// ---------------------------------------------------------------------------
// AlarmEval
// ---------------------------------------------------------------------------

/// A breach must hold for `debounce` consecutive cycles to raise, and a
/// recovery likewise to clear. A shorter blip does neither.
#[test]
fn debounce_gates_raise_and_clear() {
    let mut eval = eval_with(3, 0.0, false);
    let mut occ = 100;

    // Two breaching cycles, then a recovery. No raise.
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

    // Two in-band cycles interrupted by a breach restart the clear counter too.
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

/// Values between a threshold and its hysteresis margin advance neither
/// counter, so boundary chatter neither raises nor clears.
#[test]
fn hysteresis_dead_zone_resets_both_counters() {
    let mut eval = eval_with(2, 0.2, false);
    let mut occ = 0;

    // One breach, then a dead-zone value (0.9 is above 1.0 - 0.2 but not past
    // the threshold), which resets the raise counter.
    assert_eq!(step(&mut eval, 1.5, &mut occ), None);
    assert_eq!(step(&mut eval, 0.9, &mut occ), None);
    assert_eq!(step(&mut eval, 1.5, &mut occ), None); // count restarted at 1
    assert!(step(&mut eval, 1.5, &mut occ).is_some()); // now raises

    // While active, one recovery followed by dead-zone chatter resets the
    // clear counter; a fresh full debounce of comfortable recovery clears.
    assert_eq!(step(&mut eval, 0.5, &mut occ), None);
    assert_eq!(step(&mut eval, 0.9, &mut occ), None);
    assert_eq!(step(&mut eval, 0.5, &mut occ), None);
    assert_eq!(
        step(&mut eval, 0.5, &mut occ),
        Some(EvalEvent::Clear { occurrence: 1 })
    );
}

/// A NaN value counts as dead zone, freezing the alarm where it is.
#[test]
fn nan_freezes_the_alarm() {
    let mut eval = eval_with(1, 0.0, false);
    let mut occ = 0;
    assert!(step(&mut eval, 1.5, &mut occ).is_some());
    assert_eq!(step(&mut eval, f64::NAN, &mut occ), None);
    // Still active, so an in-band value clears it.
    assert_eq!(
        step(&mut eval, 0.0, &mut occ),
        Some(EvalEvent::Clear { occurrence: 1 })
    );
}

/// Escalation re-raises the same occurrence at the higher severity, and
/// severity only ratchets up. Dropping back to the warning band emits nothing.
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
    // Into the critical band. Same occurrence, escalated.
    assert_eq!(
        step(&mut eval, 2.5, &mut occ),
        Some(EvalEvent::Raise {
            occurrence: 1,
            severity: Severity::Critical
        })
    );
    // Steady critical, then back down to a warning-band breach. Nothing new.
    assert_eq!(step(&mut eval, 2.5, &mut occ), None);
    assert_eq!(step(&mut eval, 1.5, &mut occ), None);
    assert_eq!(
        step(&mut eval, 0.0, &mut occ),
        Some(EvalEvent::Clear { occurrence: 1 })
    );
}

/// A value past both bands at once raises straight at critical.
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

/// A latched alarm that has recovered clears on the ack; the recovery alone
/// holds it active.
#[test]
fn latching_recover_then_ack() {
    let mut eval = eval_with(1, 0.0, true);
    let mut occ = 0;
    assert!(step(&mut eval, 1.5, &mut occ).is_some());

    // Recovered but latched, so no clear, cycle after cycle.
    assert_eq!(step(&mut eval, 0.0, &mut occ), None);
    assert_eq!(step(&mut eval, 0.0, &mut occ), None);

    assert_eq!(eval.ack(1), Some(EvalEvent::Clear { occurrence: 1 }));
}

/// A latched alarm acked while still breaching clears when the recovery
/// debounce later completes.
#[test]
fn latching_ack_then_recover() {
    let mut eval = eval_with(2, 0.0, true);
    let mut occ = 0;
    step(&mut eval, 1.5, &mut occ);
    assert!(step(&mut eval, 1.5, &mut occ).is_some());

    assert_eq!(eval.ack(1), None); // still breaching, so the ack is only recorded
    assert_eq!(step(&mut eval, 0.0, &mut occ), None);
    assert_eq!(
        step(&mut eval, 0.0, &mut occ),
        Some(EvalEvent::Clear { occurrence: 1 })
    );
}

/// A re-breach after recovery un-recovers a latched alarm, so the earlier
/// recovery no longer satisfies the clear.
#[test]
fn latching_rebreach_unrecovers() {
    let mut eval = eval_with(1, 0.0, true);
    let mut occ = 0;
    assert!(step(&mut eval, 1.5, &mut occ).is_some());
    assert_eq!(step(&mut eval, 0.0, &mut occ), None); // recovered (unacked)
    assert_eq!(step(&mut eval, 1.5, &mut occ), None); // re-breach voids the recovery
    assert_eq!(eval.ack(1), None); // acked, but no longer recovered
    assert_eq!(
        step(&mut eval, 0.0, &mut occ),
        Some(EvalEvent::Clear { occurrence: 1 })
    );
}

/// Acks for a stale or unknown occurrence are dropped, and on a non-latching
/// alarm an ack never clears.
#[test]
fn stale_and_nonlatching_acks_are_inert() {
    let mut eval = eval_with(1, 0.0, false);
    let mut occ = 0;
    assert_eq!(eval.ack(7), None); // nothing active
    assert!(step(&mut eval, 1.5, &mut occ).is_some());
    assert_eq!(eval.ack(999), None); // wrong occurrence
    assert_eq!(eval.ack(1), None); // non-latching, so recorded without a clear
    assert_eq!(
        step(&mut eval, 0.0, &mut occ),
        Some(EvalEvent::Clear { occurrence: 1 })
    );
}

/// A `below` threshold breaches downward and needs the hysteresis margin
/// upward to count as in band.
#[test]
fn below_thresholds_breach_downward() {
    let mut r = raw(band(None, Some(-1.0)), None);
    r.hysteresis = Some(0.1);
    let mut eval = AlarmEval::new(&spec(r));
    let mut occ = 0;

    assert!(step(&mut eval, -1.5, &mut occ).is_some());
    assert_eq!(step(&mut eval, -0.95, &mut occ), None); // dead zone, needs >= -0.9
    assert_eq!(
        step(&mut eval, -0.5, &mut occ),
        Some(EvalEvent::Clear { occurrence: 1 })
    );
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

/// Defaults are debounce 1, hysteresis 0, non-latching, and a severity equal
/// to the lowest configured band.
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

// ---------------------------------------------------------------------------
// The system, end to end (coordinator-built, registry-observed)
// ---------------------------------------------------------------------------

#[cfg(not(miri))]
mod system {
    use metor_proto::types::{ComponentId, Msg, Timestamp};
    use metor_proto_wkt::{AlarmAck, AlarmCleared, AlarmDefs, AlarmRaised, LimitKind, Severity};
    use serde::de::DeserializeOwned;
    use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

    use crate::{
        AlarmSystem, AlarmsParams, BuildSystem, ClockMode, CommandOut, Coordinator,
        CoordinatorConfig, CyclicSystem, LogEvent, MsgIn, Out, Output, System, SystemInput,
        SystemOutput,
    };

    use crate::coordinator::PortRef;
    use crate::coordinator::init::cyclic_node;
    use metor_fsw_2_core::PortId;

    use super::super::{AlarmSpec, BandSpec, RawAlarmSpec, TargetSpec};

    #[derive(crate::Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
    #[repr(C)]
    #[metor_fsw(name = "gyro")]
    struct Gyro {
        #[metor_fsw(timestamp)]
        timestamp: Timestamp,
        rates: [f64; 3],
    }

    #[derive(crate::Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
    #[repr(C)]
    #[metor_fsw(name = "status")]
    struct Status {
        #[metor_fsw(timestamp)]
        timestamp: Timestamp,
        // The flag rides as a u8 because `bool` has invalid bit patterns and
        // can't be a zerocopy frame field. `as_f64` makes it alarmable anyway.
        degraded: u8,
        _pad: [u8; 7],
    }

    #[derive(SystemInput)]
    struct NoIn {}

    /// A scripted telemetry source that publishes one `rates[1]` sample per
    /// cycle (the last value repeats) and mirrors `degraded = rate > 10.0` on
    /// a second frame.
    struct Plant {
        script: Vec<f64>,
        cycle: usize,
    }

    #[derive(SystemOutput)]
    struct PlantOut {
        gyro: Output<Gyro>,
        status: Output<Status>,
    }

    impl System for Plant {
        type Input = NoIn;
        type Output = Out<PlantOut>;
        const NAME: &'static str = "plant";
    }

    impl CyclicSystem for Plant {
        fn execute(&mut self, now: Timestamp, _in: &mut NoIn, o: &mut Self::Output) {
            let v = self.script[self.cycle.min(self.script.len() - 1)];
            self.cycle += 1;
            o.gyro.publish(&Gyro {
                timestamp: now,
                rates: [0.0, v, 0.0],
            });
            o.status.publish(&Status {
                timestamp: now,
                degraded: (v > 10.0) as u8,
                _pad: [0; 7],
            });
        }
    }

    /// A stand-in operator that acks every raise it sees, `delay` cycles
    /// after first sighting, closing the latch loop over ordinary message
    /// edges.
    struct Acker {
        delay: usize,
        pending: Vec<(AlarmRaised, usize)>,
    }

    #[derive(SystemInput)]
    struct AckerIn {
        raised: MsgIn<AlarmRaised>,
    }

    #[derive(SystemOutput)]
    struct AckerOut {
        acks: CommandOut<AlarmAck>,
    }

    impl System for Acker {
        type Input = AckerIn;
        type Output = Out<AckerOut>;
        const NAME: &'static str = "acker";
    }

    impl CyclicSystem for Acker {
        fn execute(&mut self, _now: Timestamp, input: &mut AckerIn, o: &mut Self::Output) {
            let pending = &mut self.pending;
            input.raised.drain(|r| pending.push((r, 0))).unwrap();
            for (raised, age) in &mut self.pending {
                *age += 1;
                if *age == self.delay {
                    o.acks.publish(&AlarmAck {
                        def_id: raised.def_id.clone(),
                        occurrence: raised.occurrence,
                        operator: "test".into(),
                        note: None,
                    });
                }
            }
        }
    }

    fn config() -> CoordinatorConfig {
        CoordinatorConfig {
            cycle_rate: 1000.0,
            clock: ClockMode::Wall,
            // Zero slack turns the reader budget into a structural assertion.
            // The alarm system must claim exactly one view per watched entry
            // (its ReceiveAll budgets one slot per ring); if it claims more,
            // the build-time sizing is exceeded and the alarm reports
            // `alarms.reader_slot` instead of raising.
            reader_slack: 0,
            ..CoordinatorConfig::default()
        }
    }

    fn alarm(id: &str, component: &str, element: Option<usize>) -> RawAlarmSpec {
        RawAlarmSpec {
            id: id.into(),
            name: id.into(),
            description: String::new(),
            target: TargetSpec {
                component: component.into(),
                element,
            },
            warning: Some(BandSpec {
                above: Some(0.5),
                below: None,
            }),
            critical: Some(BandSpec {
                above: Some(1.0),
                below: None,
            }),
            debounce: Some(2),
            hysteresis: Some(0.1),
            latching: None,
            severity: None,
        }
    }

    fn params(specs: Vec<RawAlarmSpec>) -> AlarmsParams {
        AlarmsParams {
            alarm: specs
                .into_iter()
                .map(|r| AlarmSpec::try_from(r).expect("valid spec"))
                .collect(),
        }
    }

    /// Claim a decoding view on one of the [`AlarmSystem`]'s message outputs.
    /// Must happen before `run_for`, since a view starts at the live edge.
    fn tap<M: metor_proto::types::Msg + serde::de::DeserializeOwned>(
        coord: &Coordinator,
        key: &str,
    ) -> MsgIn<M> {
        let registry = coord.registry();
        let entry = registry
            .get(ComponentId::new(key))
            .unwrap_or_else(|| panic!("registry entry `{key}`"));
        MsgIn::new(entry.view().expect("reader slot"))
    }

    fn drain<M: Msg + DeserializeOwned>(input: &mut MsgIn<M>) -> Vec<M> {
        let mut messages = Vec::new();
        input.drain(|message| messages.push(message)).unwrap();
        messages
    }

    /// The headline path. A def at the first cycle, a debounced warning raise,
    /// an escalation to critical on the same occurrence, a hysteresis
    /// dead-zone hold, and a debounced clear, while a second alarm on the same
    /// frame shares the single watch view (reader_slack is 0) and stays
    /// silent.
    #[stellarator::test]
    async fn end_to_end_raise_escalate_clear() {
        let mut b = crate::coordinator::init::InitGraph::new(config());
        let script = vec![0.7, 0.7, 1.5, 0.45, 0.3, 0.3];
        let cycles = script.len();
        b.push_node(cyclic_node(Plant::NAME.into(), Plant { script, cycle: 0 }));
        // `[f64; 3]` flattens to per-element components (`rates.0/.1/.2`), so
        // the dotted path itself is the element address; a shaped tensor
        // component would take `element=` instead.
        b.push_node(cyclic_node(
            AlarmSystem::NAME.into(),
            AlarmSystem::new(params(vec![
                alarm("RATE_HIGH", "plant.gyro.rates.1", None),
                alarm("RATE_X", "plant.gyro.rates.0", None), // same frame, never breaches
            ])),
        ));
        let coord = b.build().unwrap();

        let mut defs = tap::<AlarmDefs>(&coord, "alarms.AlarmDefs");
        let mut raised = tap::<AlarmRaised>(&coord, "alarms.AlarmRaised");
        let mut cleared = tap::<AlarmCleared>(&coord, "alarms.AlarmCleared");

        let mut coord = coord;
        coord.run_for(cycles).await;

        // Both defs broadcast in the one snapshot record; thresholds are the
        // display limits.
        let mut got_defs = Vec::new();
        defs.drain(|set| got_defs.extend(set.defs)).unwrap();
        assert_eq!(got_defs.len(), 2);
        let def = got_defs.iter().find(|d| d.id == "RATE_HIGH").unwrap();
        assert_eq!(def.default_severity, Severity::Warning);
        let target = def.target.as_ref().unwrap();
        assert_eq!(target.component_id, ComponentId::new("plant.gyro.rates.1"));
        assert_eq!(target.element_index, None);
        assert_eq!(def.limits.len(), 2);
        assert!(def.limits.iter().all(|l| l.kind == LimitKind::Upper));
        assert!(
            def.limits
                .iter()
                .any(|l| l.value == 0.5 && l.severity == Severity::Warning)
        );
        assert!(
            def.limits
                .iter()
                .any(|l| l.value == 1.0 && l.severity == Severity::Critical)
        );

        // Cycle 2 raises at warning (debounce 2), cycle 3 escalates in place.
        let got_raised = drain(&mut raised);
        assert_eq!(got_raised.len(), 2, "{got_raised:?}");
        assert!(got_raised.iter().all(|r| r.def_id == "RATE_HIGH"));
        assert_eq!(got_raised[0].severity, Severity::Warning);
        assert_eq!(got_raised[0].value, Some(0.7));
        assert_eq!(got_raised[0].message, "plant.gyro.rates.1 = 0.7000");
        assert_eq!(got_raised[1].severity, Severity::Critical);
        assert_eq!(got_raised[1].value, Some(1.5));
        assert_eq!(
            got_raised[1].occurrence, got_raised[0].occurrence,
            "escalation re-raises the same occurrence"
        );

        // Cycle 4 is the dead zone (needs <= 0.4 to count); 5-6 clear (debounce 2).
        let got_cleared = drain(&mut cleared);
        assert_eq!(got_cleared.len(), 1);
        assert_eq!(got_cleared[0].occurrence, got_raised[0].occurrence);
    }

    /// A latching alarm holds after recovery until the ack lands, wired
    /// through a full message-edge loop (raises out to the acker, acks back).
    #[stellarator::test]
    async fn latching_clears_only_on_ack() {
        let run = |delay: usize, cycles: usize| async move {
            let mut b = crate::coordinator::init::InitGraph::new(config());
            b.push_node(cyclic_node(
                Plant::NAME.into(),
                Plant {
                    script: vec![2.0, 0.0],
                    cycle: 0,
                },
            ));
            let acker = b.push_node(cyclic_node(
                Acker::NAME.into(),
                Acker {
                    delay,
                    pending: Vec::new(),
                },
            ));
            let mut spec = alarm("LATCHED", "plant.gyro.rates.1", None);
            spec.debounce = Some(1);
            spec.latching = Some(true);
            let alarms = b.push_node(cyclic_node(
                AlarmSystem::NAME.into(),
                AlarmSystem::new(params(vec![spec])),
            ));
            b.connect(
                PortRef {
                    system: alarms,
                    port: PortId::Packet(AlarmRaised::ID),
                },
                PortRef {
                    system: acker,
                    port: PortId::Packet(AlarmRaised::ID),
                },
            );
            b.connect(
                PortRef {
                    system: acker,
                    port: PortId::Packet(AlarmAck::ID),
                },
                PortRef {
                    system: alarms,
                    port: PortId::Packet(AlarmAck::ID),
                },
            );
            let coord = b.build().unwrap();
            let mut cleared = tap::<AlarmCleared>(&coord, "alarms.AlarmCleared");
            let mut coord = coord;
            coord.run_for(cycles).await;
            drain(&mut cleared).len()
        };

        // Never acked (delay beyond the run): recovered by cycle 2, still no clear.
        assert_eq!(run(100, 8).await, 0, "latched alarm must hold without ack");
        // Acked 2 cycles after sighting: the clear lands.
        assert_eq!(run(2, 8).await, 1, "latched alarm clears once acked");
    }

    /// A u8 flag component alarms through the same numeric path (a raw 1 reads
    /// as 1.0, breaching `above = 0.5`).
    #[stellarator::test]
    async fn flag_component_alarms() {
        let mut b = crate::coordinator::init::InitGraph::new(config());
        b.push_node(cyclic_node(
            Plant::NAME.into(),
            Plant {
                script: vec![0.0, 20.0, 20.0], // degraded = rate > 10
                cycle: 0,
            },
        ));
        let mut spec = alarm("DEGRADED", "plant.status.degraded", None);
        spec.warning = None;
        spec.critical = Some(BandSpec {
            above: Some(0.5),
            below: None,
        });
        spec.debounce = Some(1);
        spec.hysteresis = None;
        b.push_node(cyclic_node(
            AlarmSystem::NAME.into(),
            AlarmSystem::new(params(vec![spec])),
        ));
        let coord = b.build().unwrap();
        let mut raised = tap::<AlarmRaised>(&coord, "alarms.AlarmRaised");
        let mut coord = coord;
        coord.run_for(3).await;

        let got = drain(&mut raised);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].def_id, "DEGRADED");
        assert_eq!(got[0].severity, Severity::Critical);
        assert_eq!(got[0].value, Some(1.0));
        assert_eq!(got[0].message, "plant.status.degraded = 1.0000");
    }

    /// Under a target namespace the engine prefixes its authored targets, so
    /// they resolve against the namespace-qualified registry and the broadcast
    /// def carries the qualified component id. `configure` is the seam the
    /// front-end threads the namespace through.
    #[stellarator::test]
    async fn namespace_prefixes_alarm_targets() {
        use crate::{BuildCtx, MsgTable};

        let mut b = crate::coordinator::init::InitGraph::new(config());
        b.namespace = Some("sat1".into());
        let script = vec![0.7, 0.7, 1.5];
        let cycles = script.len();
        b.push_node(cyclic_node(Plant::NAME.into(), Plant { script, cycle: 0 }));
        let mut alarms =
            AlarmSystem::new(params(vec![alarm("RATE_HIGH", "plant.gyro.rates.1", None)]));
        // The front-end applies the namespace here (the static factory's
        // `configure` call); the graph seam qualifies the Plant's registry
        // keys to match.
        alarms
            .configure(&BuildCtx {
                msgs: &MsgTable::default(),
                namespace: Some("sat1"),
            })
            .unwrap();
        b.push_node(cyclic_node(AlarmSystem::NAME.into(), alarms));
        let coord = b.build().unwrap();

        // The engine's own message outputs are namespace-qualified too.
        let mut defs = tap::<AlarmDefs>(&coord, "sat1.alarms.AlarmDefs");
        let mut raised = tap::<AlarmRaised>(&coord, "sat1.alarms.AlarmRaised");
        let mut coord = coord;
        coord.run_for(cycles).await;

        let mut got_defs = Vec::new();
        defs.drain(|set| got_defs.extend(set.defs)).unwrap();
        let def = got_defs.iter().find(|d| d.id == "RATE_HIGH").unwrap();
        assert_eq!(
            def.target.as_ref().unwrap().component_id,
            ComponentId::new("sat1.plant.gyro.rates.1"),
            "the def target is the namespace-qualified component"
        );

        // The alarm resolved the prefixed target against the qualified
        // registry entry and raised; its message names the qualified component.
        let got_raised = drain(&mut raised);
        assert!(
            !got_raised.is_empty(),
            "the namespaced target resolved and raised"
        );
        assert!(got_raised.iter().all(|r| r.def_id == "RATE_HIGH"));
        assert!(got_raised[0].message.starts_with("sat1.plant.gyro.rates.1"));
    }

    /// Misconfigured targets disable their alarms and surface on the log.
    /// The def still broadcasts and nothing ever raises.
    #[stellarator::test]
    async fn bad_targets_disable_and_report_faults() {
        let mut b = crate::coordinator::init::InitGraph::new(config());
        b.push_node(cyclic_node(
            Plant::NAME.into(),
            Plant {
                script: vec![5.0], // would breach everything, were anything enabled
                cycle: 0,
            },
        ));
        b.push_node(cyclic_node(
            AlarmSystem::NAME.into(),
            AlarmSystem::new(params(vec![
                alarm("GHOST", "nowhere.gyro.rates.1", None), // no such component
                alarm("OOB", "plant.gyro.rates.1", Some(9)),  // element out of bounds (scalar)
                alarm("DUP", "plant.gyro.rates.1", None),
                alarm("DUP", "plant.gyro.rates.1", None), // duplicate id
            ])),
        ));
        let coord = b.build().unwrap();
        let mut defs = tap::<AlarmDefs>(&coord, "alarms.AlarmDefs");
        let mut raised = tap::<AlarmRaised>(&coord, "alarms.AlarmRaised");
        let mut log = tap::<LogEvent>(&coord, "alarms.log");
        let mut coord = coord;
        coord.run_for(4).await;

        let mut n_defs = 0;
        defs.drain(|set| n_defs += set.defs.len()).unwrap();
        assert_eq!(n_defs, 4, "defs broadcast even for disabled alarms");

        // Only the first DUP survives resolution, and it raises; the rest stay dark.
        let got = drain(&mut raised);
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].def_id, "DUP");

        // The three boot failures each land as one fault line on the log.
        let mut kinds: Vec<String> = drain(&mut log)
            .into_iter()
            .filter_map(|ev| {
                ev.fields
                    .into_iter()
                    .find(|(k, _)| k == "kind")
                    .map(|(_, v)| v)
            })
            .collect();
        kinds.sort();
        assert_eq!(
            kinds,
            [
                "alarms_bad_element",
                "alarms_duplicate_id",
                "alarms_unresolved_target"
            ],
            "ghost + oob + duplicate each report once"
        );
    }
}

/// `to_def` maps each configured threshold to one display limit at the firing
/// value.
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

    let mut got: Vec<(LimitKind, f64, Severity)> = def
        .limits
        .iter()
        .map(|l| (l.kind, l.value, l.severity))
        .collect();
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
