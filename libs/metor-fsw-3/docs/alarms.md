# Alarms

## Goal

Define alarms in Rust, in a `#[system]` or a `#[sequence]`, as conditions
over the fn's ports. Publish raises and clears as the wkt `AlarmRaised` and
`AlarmCleared` messages the panel already consumes. Every redline is also an
alarm.

## Data

```rust
/// One alarm. A `const` at its call site.
/// `id` is unique among the alarms of one system or sequence.
pub struct AlarmSpec { pub id: &'static str, pub severity: Severity, pub debounce: NonZeroU16 }

/// The state machine: raise, debounce, recover. `active` is the raise time.
pub struct AlarmCell { breach: u16, clear: u16, active: Option<Timestamp> }

/// The `alarm_raised` and `alarm_cleared` outputs, the id prefix, and the
/// fixed-capacity records an edge fills.
pub struct AlarmPorts { .. }
```

A condition is `impl Into<Option<bool>>`: `true` breaches at the spec's
severity, `false` recovers, and `None` (no sample) keeps the counts. Two
severities on one value are two specs with their own cells.

## Systems

The system's state owns its cells; `Alarms` is a param over `AlarmPorts`.

```rust
const BATT_HOT: AlarmSpec = AlarmSpec::critical("batt_hot").debounce(3);

#[derive(Default)]
struct Power { batt_hot: AlarmCell }

#[system]
fn power(#[state] s: &mut Power, batt: &Sample<Batt>, alarms: &mut Alarms) {
    let b = batt.get();
    alarms.check_with(&mut s.batt_hot, &BATT_HOT, b.map(|b| b.temp > 60.0),
                      |m| write!(m, "batt {:.1} C", b.map_or(f64::NAN, |b| b.temp)));
}
```

`check` uses the spec's id as the message. An unchecked cell holds.
Dropping `Alarms` (a `Reset`, a `Load`, a panic, or the system's end) clears
up to 32 active alarms; a raise past that is not cleared on drop. A wasm
host calls `metor_fsw_destroy` when its step drops and copies the guest's
outputs once more, so a pack's clears reach the rings.

## Sequences

Every sequence has the alarm ports, held by its `Seq`.

```rust
#[sequence]
async fn burn(seq: &Seq, pc: &Sample<Chamber>) -> Result<(), SeqError> {
    alarm!(seq, &PC_HIGH, || pc.get().map(|c| c.pressure > WARN_PC),
           |m| write!(m, "pc {:.1}", pc.get().map_or(f64::NAN, |c| c.pressure)));
    redline!(seq, "chamber pressure", || pc.get().is_some_and(|c| c.pressure > MAX_PC));
    ..
}
```

`alarm!` and `redline!` pin a `Watch` in the body future and link it into
the `Seq`'s list. Each cycle, before the body is polled, the `Seq` steps
every linked watch and publishes its edges; the first tripped redline
aborts. Dropping a watch clears its alarm, on every path: block end, abort,
panic, or a leaked frame when the `Seq` drops.

`redline!` is critical with debounce 1. Its id is
`{ns}.{sequence}.redline.{name}`; an unnamed redline is named by
`stringify!` of its condition. An id past 96 bytes is cut and ends in `_`
and an FNV-1a hash of the whole.

## Publishing

- Edges only. Occurrence id: `fnv1a_64(def_id) ^ raised_at`.
- The format closure runs only on a raise. Overflow truncates.
- `RaisedMsg`/`ClearedMsg` are the wkt fields with `heapless::String`; they
  encode to the same postcard bytes and share the wkt schema.
- A raise and a clear in one cycle travel on different ports, so a link may
  deliver the clear first. The panel holds such a clear (up to 64) until
  its raise arrives.
- A prefix that leaves no room for a name fails `bind_out` with a
  `MakeError`.

## Allocation

`Id` and `Msg` are fixed buffers in `AlarmPorts`. Cells live in state or in
the pinned body future. Steady state mutates cells, formats into `Msg`, and
encodes into rings.

## Scope

- Alarms are Rust only; no Python-configured limit alarms.
- Future work: `AlarmDefs` with targets and limits, for meter bands and
  names.
