//! The alarm engine (`docs/alarms.md`).
//!
//! An alarm is just a message, and the alarm engine is just a system: `AlarmSystem`
//! is an ordinary [`CyclicSystem`](crate::CyclicSystem) that evaluates configured
//! **limit alarms** against any telemetered component (read through the
//! [`AllOutputs`](crate::AllOutputs) tap) and reports firing over ordinary telemetered
//! message ports carrying the wkt alarm Msgs — [`AlarmDef`] at boot, [`AlarmRaised`] /
//! [`AlarmCleared`] on debounced transitions. The downlink, the db msg-log catch-all,
//! and the panel's alarm store consume them with no new plumbing.
//!
//! This module has two halves:
//!
//! - the **config surface** ([`AlarmsParams`] / [`AlarmSpec`]): serde types a mission
//!   file's `system "alarms" type="Alarms" { alarm … }` node deserializes into, with
//!   semantic validation at load (`docs/alarms.md` §3), and the mechanical
//!   [`AlarmSpec::to_def`] mapping — the firing thresholds *are* the display limits;
//! - the **evaluation state machine** ([`AlarmEval`]): pure, port-free per-alarm state
//!   (debounce both ways, a hysteresis dead zone, severity escalation on the same
//!   occurrence, latching clear gated on recovered ∧ acked — `docs/alarms.md` §5.3);
//! - the **system** ([`AlarmSystem`]): target resolution + def broadcast at the first
//!   execute (`docs/alarms.md` §5.1 — not `init`, the downlink's taps don't exist
//!   yet), per-cycle value extraction off shared registry views, and the ack drain.

use metor_fsw_ring::{NoWake, View};
use metor_proto::types::{ComponentId, Timestamp};
use metor_proto::vtable::VTable;
use metor_proto_wkt::{
    AlarmAck, AlarmCleared, AlarmDef, AlarmLimit, AlarmRaised, AlarmTarget, LimitKind,
    OccurrenceId, Severity,
};
use serde::Deserialize;

use crate::message::{MsgIn, MsgOut};
use crate::registry::{AllOutputs, EntrySchema};
use crate::system::{BuildSystem, CyclicSystem, Out, System};

/// The `AlarmSystem` params: the mission's alarm definitions, one [`AlarmSpec`] per
/// repeated `alarm` child node. An alarm-less instance is legal (it evaluates nothing).
#[derive(Deserialize, Debug, Clone, Default)]
pub struct AlarmsParams {
    #[serde(default)]
    pub alarm: Vec<AlarmSpec>,
}

/// One configured limit alarm, semantically validated (via `RawAlarmSpec`) at
/// deserialization — a bad spec is a spanned load error, not a runtime surprise.
#[derive(Deserialize, Debug, Clone)]
#[serde(try_from = "RawAlarmSpec")]
pub struct AlarmSpec {
    /// The stable [`AlarmId`](metor_proto_wkt::AlarmId), e.g. `"ADCS_RATE_HIGH"`.
    pub id: String,
    /// Human-readable title the panel shows.
    pub name: String,
    pub description: String,
    pub target: TargetSpec,
    pub warning: Option<BandSpec>,
    pub critical: Option<BandSpec>,
    /// Consecutive cycles a breach must hold to raise — and a recovery to clear.
    pub debounce: u32,
    /// Absolute margin a value must recover *past* a threshold before it counts as
    /// in-band (the clear side of the dead zone).
    pub hysteresis: f64,
    /// A latching alarm clears only once recovered **and** operator-acked.
    pub latching: bool,
    /// `AlarmDef::default_severity`; defaults to the lowest configured band.
    pub severity: Severity,
}

/// The monitored component: an instance-prefixed component id text
/// (`"plant.sensors.gyro_b"`) plus an optional element index into its shape.
#[derive(Deserialize, Debug, Clone)]
pub struct TargetSpec {
    pub component: String,
    pub element: Option<usize>,
}

/// One severity band: breach is `value > above` or `value < below`. At least one
/// side must be configured (validated on the owning spec).
#[derive(Deserialize, Debug, Clone, Copy)]
pub struct BandSpec {
    pub above: Option<f64>,
    pub below: Option<f64>,
}

impl BandSpec {
    fn breached(&self, v: f64) -> bool {
        self.above.is_some_and(|t| v > t) || self.below.is_some_and(|t| v < t)
    }

    /// Every configured threshold respected by at least `margin`.
    fn in_band(&self, v: f64, margin: f64) -> bool {
        self.above.is_none_or(|t| v <= t - margin) && self.below.is_none_or(|t| v >= t + margin)
    }

    fn thresholds_finite(&self) -> bool {
        self.above.is_none_or(f64::is_finite) && self.below.is_none_or(f64::is_finite)
    }
}

/// The unvalidated deserialization target [`AlarmSpec`] is `try_from`-refined out of.
#[derive(Deserialize, Debug)]
struct RawAlarmSpec {
    id: String,
    name: String,
    #[serde(default)]
    description: String,
    target: TargetSpec,
    warning: Option<BandSpec>,
    critical: Option<BandSpec>,
    debounce: Option<u32>,
    hysteresis: Option<f64>,
    latching: Option<bool>,
    severity: Option<Severity>,
}

impl TryFrom<RawAlarmSpec> for AlarmSpec {
    type Error = String;

    fn try_from(raw: RawAlarmSpec) -> Result<Self, String> {
        let id = raw.id;
        if id.is_empty() {
            return Err("alarm id must not be empty".into());
        }
        let err = |msg: &str| Err(format!("alarm {id:?}: {msg}"));
        let (warning, critical) = (raw.warning, raw.critical);
        if warning.is_none() && critical.is_none() {
            return err("at least one of `warning`/`critical` is required");
        }
        for (band, which) in [(&warning, "warning"), (&critical, "critical")] {
            let Some(band) = band else { continue };
            if band.above.is_none() && band.below.is_none() {
                return err(&format!("`{which}` needs `above=` and/or `below=`"));
            }
            if !band.thresholds_finite() {
                return err(&format!("`{which}` thresholds must be finite"));
            }
        }
        if let (Some(w), Some(c)) = (&warning, &critical) {
            // Critical is the outer band: a critical threshold tighter than the
            // warning one would fire critical before (or instead of) warning.
            if let (Some(wa), Some(ca)) = (w.above, c.above)
                && ca < wa
            {
                return err("`critical above=` must not be below `warning above=`");
            }
            if let (Some(wb), Some(cb)) = (w.below, c.below)
                && cb > wb
            {
                return err("`critical below=` must not be above `warning below=`");
            }
        }
        let debounce = raw.debounce.unwrap_or(1);
        if debounce == 0 {
            return err("`debounce` must be at least 1");
        }
        let hysteresis = raw.hysteresis.unwrap_or(0.0);
        if !(hysteresis >= 0.0 && hysteresis.is_finite()) {
            return err("`hysteresis` must be finite and non-negative");
        }
        let severity = raw.severity.unwrap_or(if warning.is_some() {
            Severity::Warning
        } else {
            Severity::Critical
        });
        Ok(AlarmSpec {
            id,
            name: raw.name,
            description: raw.description,
            target: raw.target,
            warning,
            critical,
            debounce,
            hysteresis,
            latching: raw.latching.unwrap_or(false),
            severity,
        })
    }
}

impl AlarmSpec {
    /// The wkt declaration this spec broadcasts: one display limit per configured
    /// threshold, at the *firing* value — the panel draws exactly the boundary the
    /// engine enforces (`docs/alarms.md` §3.1).
    pub fn to_def(&self) -> AlarmDef {
        let mut limits = Vec::new();
        for (band, severity) in [(&self.warning, Severity::Warning), (&self.critical, Severity::Critical)] {
            let Some(band) = band else { continue };
            if let Some(value) = band.above {
                limits.push(AlarmLimit { kind: LimitKind::Upper, value, severity, label: None });
            }
            if let Some(value) = band.below {
                limits.push(AlarmLimit { kind: LimitKind::Lower, value, severity, label: None });
            }
        }
        AlarmDef {
            id: self.id.clone(),
            name: self.name.clone(),
            description: self.description.clone(),
            target: Some(AlarmTarget {
                component_id: ComponentId::new(&self.target.component),
                element_index: self.target.element,
            }),
            limits,
            default_severity: self.severity,
        }
    }
}

// ---------------------------------------------------------------------------
// The evaluation state machine
// ---------------------------------------------------------------------------

/// What one evaluation step (or an ack) asks the caller to emit. The caller owns the
/// wire types: it builds the `AlarmRaised` (it has the value and the target name for
/// the message) and the `AlarmCleared`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EvalEvent {
    /// A new occurrence — or, if `occurrence` matches the already-active one, a
    /// severity escalation re-raise the panel applies in place.
    Raise {
        occurrence: OccurrenceId,
        severity: Severity,
    },
    Clear { occurrence: OccurrenceId },
}

/// One active occurrence's lifecycle state.
#[derive(Debug)]
struct Active {
    occurrence: OccurrenceId,
    severity: Severity,
    acked: bool,
    /// The recovery debounce completed; a latching alarm may still be waiting on ack.
    /// Un-set by any new breach.
    recovered: bool,
}

/// Per-alarm evaluation state (`docs/alarms.md` §5.3): pure and port-free — the
/// `AlarmSystem` turns its events into wire Msgs.
///
/// A value classifies three ways each step: **breaching** (some band violated — the
/// raise counter runs), **in band** (every threshold respected by ≥ `hysteresis` —
/// the clear counter runs), or the **dead zone** between them (both counters reset;
/// chatter around a boundary neither raises nor clears). A NaN value lands in the
/// dead zone, freezing the alarm where it is.
#[derive(Debug)]
pub(crate) struct AlarmEval {
    warning: Option<BandSpec>,
    critical: Option<BandSpec>,
    debounce: u32,
    hysteresis: f64,
    latching: bool,
    breach_count: u32,
    clear_count: u32,
    active: Option<Active>,
}

impl AlarmEval {
    pub fn new(spec: &AlarmSpec) -> Self {
        Self {
            warning: spec.warning,
            critical: spec.critical,
            debounce: spec.debounce,
            hysteresis: spec.hysteresis,
            latching: spec.latching,
            breach_count: 0,
            clear_count: 0,
            active: None,
        }
    }

    /// The worst breached band, if any (critical beats warning).
    fn breach(&self, v: f64) -> Option<Severity> {
        if self.critical.is_some_and(|b| b.breached(v)) {
            return Some(Severity::Critical);
        }
        if self.warning.is_some_and(|b| b.breached(v)) {
            return Some(Severity::Warning);
        }
        None
    }

    /// Every configured threshold (both bands) respected by ≥ the hysteresis margin.
    fn in_band(&self, v: f64) -> bool {
        self.warning.is_none_or(|b| b.in_band(v, self.hysteresis))
            && self.critical.is_none_or(|b| b.in_band(v, self.hysteresis))
    }

    /// Step the machine with this cycle's value. `alloc` mints a fresh occurrence id
    /// (called at most once, on a raise).
    pub fn step(&mut self, v: f64, alloc: &mut impl FnMut() -> OccurrenceId) -> Option<EvalEvent> {
        if let Some(severity) = self.breach(v) {
            self.clear_count = 0;
            match &mut self.active {
                Some(active) => {
                    active.recovered = false;
                    if severity > active.severity {
                        active.severity = severity;
                        return Some(EvalEvent::Raise {
                            occurrence: active.occurrence,
                            severity,
                        });
                    }
                }
                None => {
                    self.breach_count += 1;
                    if self.breach_count >= self.debounce {
                        self.breach_count = 0;
                        let occurrence = alloc();
                        self.active = Some(Active {
                            occurrence,
                            severity,
                            acked: false,
                            recovered: false,
                        });
                        return Some(EvalEvent::Raise { occurrence, severity });
                    }
                }
            }
        } else if self.in_band(v) {
            self.breach_count = 0;
            if let Some(active) = &mut self.active {
                if !active.recovered {
                    self.clear_count += 1;
                    if self.clear_count >= self.debounce {
                        self.clear_count = 0;
                        active.recovered = true;
                    }
                }
                if active.recovered && (!self.latching || active.acked) {
                    let occurrence = active.occurrence;
                    self.active = None;
                    return Some(EvalEvent::Clear { occurrence });
                }
            }
        } else {
            // Dead zone: neither a breach nor a comfortable recovery.
            self.breach_count = 0;
            self.clear_count = 0;
        }
        None
    }

    /// Apply an operator ack. Only the active occurrence matches (stale acks are
    /// dropped, mirroring the panel); an already-recovered latching alarm clears
    /// immediately.
    pub fn ack(&mut self, occurrence: OccurrenceId) -> Option<EvalEvent> {
        let active = self.active.as_mut()?;
        if active.occurrence != occurrence {
            return None;
        }
        active.acked = true;
        if self.latching && active.recovered {
            self.active = None;
            return Some(EvalEvent::Clear { occurrence });
        }
        None
    }
}

// ---------------------------------------------------------------------------
// The system
// ---------------------------------------------------------------------------

/// The alarm system's inputs: panel acks over ordinary message edges
/// (`connect "uplink" -> "alarms" msg="AlarmAck"`; zero producers is a legal,
/// ack-less mission).
#[derive(crate::SystemInput)]
pub struct AlarmIn {
    acks: MsgIn<AlarmAck>,
}

/// The alarm system's outputs: ordinary **telemetered** message ports — the downlink
/// taps them, the db msg-log catch-all records them, the panel's alarm store consumes
/// them (`docs/alarms.md` §2.1) — plus the receive-all registry tap the targets
/// resolve against. `AllOutputs` rides the output bundle (not the input) exactly as
/// the downlink's does: `init` receives only the outputs, and resolving/claiming
/// there rather than at the first execute means cycle-1 values already evaluate.
#[derive(crate::SystemOutput)]
pub struct AlarmOut {
    defs: MsgOut<AlarmDef>,
    raised: MsgOut<AlarmRaised>,
    cleared: MsgOut<AlarmCleared>,
    all: AllOutputs,
}

/// One configured alarm at runtime: its broadcast def, resolved target identity, and
/// evaluation state. `enabled` drops on a boot-resolution failure (unresolvable
/// target, bad element, duplicate id) — surfaced through health, never a panic.
struct AlarmRuntime {
    spec: AlarmSpec,
    component_id: ComponentId,
    element: usize,
    eval: AlarmEval,
    enabled: bool,
}

/// One watched registry entry: the claimed read view, the entry's instance-prefixed
/// vtable (what the per-cycle extraction walks), and the alarms it feeds. One view
/// per **distinct** entry, however many alarms target its components — `ReceiveAll`
/// budgets exactly one reader slot per ring (`docs/alarms.md` §5.1).
struct Watch {
    vtable: VTable,
    view: View<NoWake, NoWake>,
    members: Vec<usize>,
}

/// The alarm engine (`docs/alarms.md`): an ordinary cyclic system evaluating the
/// configured limit alarms each cycle and reporting transitions as wkt alarm Msgs.
/// Registered like any system — `system "alarms" type="Alarms" { … }` in KDL (a
/// framework built-in), or `add_cyclic_named` after the mission's other cyclic
/// systems (a receive-all system registers late, `docs/alarms.md` §7 F1).
pub struct AlarmSystem {
    alarms: Vec<AlarmRuntime>,
    watches: Vec<Watch>,
    /// First-`execute` latch for the def broadcast — records emitted from `init`
    /// would miss the downlink's taps, which are claimed in the (later-registered)
    /// telemetry system's own `init` (B9, `docs/alarms.md` §5.1).
    booted: bool,
    /// The occurrence allocator: wall-clock-micros seeded at construction so ids are
    /// unique across runs in the panel's persisted, occurrence-keyed backfill — the
    /// documented one-time exception to the no-`Timestamp::now()` rule
    /// (`docs/alarms.md` §5.4).
    next_occurrence: u64,
    /// Reused per-watch extraction buffer: `(alarm index, value)` per cycle.
    scratch: Vec<(usize, f64)>,
}

impl BuildSystem for AlarmSystem {
    type Params = AlarmsParams;

    fn new(params: AlarmsParams) -> Self {
        let alarms = params
            .alarm
            .into_iter()
            .map(|spec| AlarmRuntime {
                component_id: ComponentId::new(&spec.target.component),
                element: spec.target.element.unwrap_or(0),
                eval: AlarmEval::new(&spec),
                enabled: true,
                spec,
            })
            .collect();
        Self {
            alarms,
            watches: Vec::new(),
            booted: false,
            next_occurrence: Timestamp::now().0 as u64,
            scratch: Vec::new(),
        }
    }
}

impl System for AlarmSystem {
    type Input = AlarmIn;
    type Output = Out<AlarmOut>;
    const NAME: &'static str = "alarms";

    /// Resolve every target and claim the watch views (`docs/alarms.md` §5.1). The
    /// registry is frozen by build, so `init` sees the whole graph; claiming here
    /// (not at the first execute) means cycle-1 records already evaluate.
    fn init(&mut self, output: &mut Self::Output) {
        self.resolve_targets(output);
    }
}

impl CyclicSystem for AlarmSystem {
    fn execute(&mut self, _now: Timestamp, input: &mut AlarmIn, output: &mut Out<AlarmOut>) {
        if !self.booted {
            self.booted = true;
            // Defs broadcast at the first execute, not `init` — the downlink's taps
            // are claimed in the telemetry system's later `init` (B9). Latest-wins
            // on the wire, so the (re-)emit is always safe.
            for rt in &self.alarms {
                output.defs.publish(&rt.spec.to_def());
            }
        }

        // Operator acks first: a latched, already-recovered occurrence clears the
        // same cycle its ack lands.
        let alarms = &mut self.alarms;
        input.acks.drain(|ack| {
            for rt in alarms.iter_mut().filter(|rt| rt.spec.id == ack.def_id) {
                if let Some(EvalEvent::Clear { occurrence }) = rt.eval.ack(ack.occurrence) {
                    output.cleared.publish(&AlarmCleared {
                        def_id: rt.spec.id.clone(),
                        occurrence,
                    });
                }
            }
        });

        // Evaluate each watch off its newest record. `try_latest` re-serves the
        // pinned record, so a silent producer keeps being evaluated at its last
        // value (`docs/alarms.md` §5.2); before a first record, nothing evaluates.
        let Self {
            alarms,
            watches,
            next_occurrence,
            scratch,
            ..
        } = self;
        for watch in watches.iter_mut() {
            let Ok(Some(record)) = watch.view.try_latest() else {
                continue;
            };
            scratch.clear();
            let members = &watch.members;
            let _ = watch.vtable.for_each_field(Some(&record), &mut |rf| {
                for &m in members {
                    if rf.component_id == alarms[m].component_id
                        && let Some(view) = &rf.view
                        && let Some(v) = view.get(alarms[m].element)
                    {
                        scratch.push((m, v.as_f64()));
                    }
                }
                Ok(())
            });
            drop(record);

            for &(m, value) in scratch.iter() {
                let rt = &mut alarms[m];
                let event = rt.eval.step(value, &mut || {
                    *next_occurrence += 1;
                    *next_occurrence
                });
                match event {
                    Some(EvalEvent::Raise {
                        occurrence,
                        severity,
                    }) => output.raised.publish(&AlarmRaised {
                        def_id: rt.spec.id.clone(),
                        occurrence,
                        severity,
                        value: Some(value),
                        message: raise_message(&rt.spec.target, value),
                    }),
                    Some(EvalEvent::Clear { occurrence }) => {
                        output.cleared.publish(&AlarmCleared {
                            def_id: rt.spec.id.clone(),
                            occurrence,
                        })
                    }
                    None => {}
                }
            }
        }
    }
}

impl AlarmSystem {
    /// Init-time resolution (`docs/alarms.md` §5.1): resolve every target against the
    /// telemetered registry entries and claim one shared view per distinct entry.
    fn resolve_targets(&mut self, output: &mut Out<AlarmOut>) {
        // Health reports deferred: iterating `output.all` borrows the bundle, and
        // `output.health()` is a `&mut` borrow.
        let mut failures: Vec<(&'static str, String)> = Vec::new();

        // Duplicate ids would collide on the ack path (and the panel's def store is
        // latest-wins by id): keep the first, disable the rest.
        for i in 0..self.alarms.len() {
            let (head, tail) = self.alarms.split_at_mut(i);
            let rt = &mut tail[0];
            if rt.enabled && head.iter().any(|o| o.enabled && o.spec.id == rt.spec.id) {
                rt.enabled = false;
                failures.push((
                    "alarms.duplicate_id",
                    format!("alarm {:?}: duplicate id — disabled", rt.spec.id),
                ));
            }
        }

        let mut watches = Vec::new();
        for entry in output.all.entries() {
            let EntrySchema::Table { vtable, .. } = &entry.schema else {
                continue;
            };
            // Registration mode surfaces each static field's id + shape; dynamic
            // member templates never receive samples directly and are skipped — a
            // target must be a static component (`docs/alarms.md` §9).
            let mut members: Vec<usize> = Vec::new();
            let alarms = &mut self.alarms;
            let failures = &mut failures;
            let _ = vtable.for_each_field(None, &mut |rf| {
                if rf.dynamic {
                    return Ok(());
                }
                for (m, rt) in alarms.iter_mut().enumerate() {
                    if !rt.enabled
                        || rf.component_id != rt.component_id
                        || members.contains(&m)
                    {
                        continue;
                    }
                    let elements = rf.shape.iter().product::<usize>().max(1);
                    if rt.element < elements {
                        members.push(m);
                    } else {
                        rt.enabled = false;
                        failures.push((
                            "alarms.bad_element",
                            format!(
                                "alarm {:?}: element {} out of bounds for `{}` ({} elements)",
                                rt.spec.id, rt.element, rt.spec.target.component, elements
                            ),
                        ));
                    }
                }
                Ok(())
            });
            if members.is_empty() {
                continue;
            }
            match entry.view() {
                Ok(view) => watches.push(Watch {
                    vtable: vtable.clone(),
                    view,
                    members,
                }),
                // Reader-slot budget exhausted (a hand-built over-subscription —
                // build-time sizing covers the known consumers): diagnosable, not
                // a panic, mirroring the downlink (`src/telemetry/mod.rs`).
                Err(_) => {
                    for m in members {
                        self.alarms[m].enabled = false;
                    }
                    failures.push((
                        "alarms.reader_slot",
                        format!(
                            "no reader slot left on `{}.{}` — raise CoordinatorConfig::reader_slack",
                            entry.instance, entry.name
                        ),
                    ));
                }
            }
        }
        self.watches = watches;

        let watched: Vec<usize> = self.watches.iter().flat_map(|w| w.members.clone()).collect();
        for (m, rt) in self.alarms.iter_mut().enumerate() {
            if rt.enabled && !watched.contains(&m) {
                rt.enabled = false;
                failures.push((
                    "alarms.unresolved_target",
                    format!(
                        "alarm {:?}: no telemetered component `{}`",
                        rt.spec.id, rt.spec.target.component
                    ),
                ));
            }
        }

        for (kind, line) in failures {
            let health = output.health();
            health.error(kind);
            health.log(crate::health::Level::Warn, &line);
        }
    }
}

/// The auto-generated raise message: `"<component>[i] = <value>"` (the element index
/// only when configured).
fn raise_message(target: &TargetSpec, value: f64) -> String {
    match target.element {
        Some(i) => format!("{}[{i}] = {value:.4}", target.component),
        None => format!("{} = {value:.4}", target.component),
    }
}

#[cfg(test)]
mod tests;
