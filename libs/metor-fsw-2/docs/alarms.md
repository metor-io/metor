# Limit alarms

The built-in `Alarms` system checks numeric components once per cycle. It sends alarm definitions, raises, and clears as message telemetry.

Alarms surface problems in the running target to an operator. They turn raw
values into clear raise and clear events, keep track of active problems, and
support operator acknowledgment when a response is required.

## Setup

This example checks one element of a three-axis rate:

```python
from metor_config import Alarm, Alarms, Component, band

alarms = m.add(
    "alarms",
    Alarms(alarms=[
        Alarm(
            id="BODY_RATE_HIGH",
            name="Body rate high",
            description="Body Y rate is outside the limit",
            target=Component("plant.sensors.gyro_b", element=1),
            warning=band(above=0.05, below=-0.05),
            critical=band(above=0.15, below=-0.15),
            debounce=2,
            hysteresis=0.005,
            latching=True,
        ),
    ]),
)
```

An alarm needs at least one warning or critical band. Each band needs an upper limit, a lower limit, or both.

`debounce` defaults to 1. It sets the number of back-to-back breach cycles needed to raise. It also sets the number of back-to-back recovery cycles needed to clear.

`hysteresis` defaults to 0. It sets an absolute recovery margin. `latching` defaults to false.

The optional `severity` sets `AlarmDef.default_severity`. It defaults to warning when a warning band exists, or critical when only a critical band exists. The band that a value breaks still sets the severity of a raise.

The alarm system uses normal framework ports. `AllOutputs` gives it read
access to telemetered frame values. Message ports send definitions and events
and accept `AlarmAck`. The coordinator needs no alarm-specific path.

## Component references

A `Component` names a component value in a telemetered table output. The common form is:

```text
<instance>.<frame>.<field>
```

The `element` value selects an item from the component shape. Omit it for a scalar. The default element is zero.

If the target has a namespace, the build step adds it to the component id. With namespace `cube_sat`, this reference:

```text
plant.sensors.gyro_b
```

becomes:

```text
cube_sat.plant.sensors.gyro_b
```

The component must be static in the vtable. The alarm resolver skips dynamic member templates. It can still target a fixed component whose value has more than one element.

The source output must allow telemetry. `AllOutputs` hides outputs marked as not telemetered.

## Build checks

Parameter decoding rejects these cases:

- an empty alarm id
- no warning or critical band
- a band with no limit
- a NaN or infinite limit
- a critical upper limit below its warning upper limit
- a critical lower limit above its warning lower limit
- `debounce` equal to zero
- a negative, NaN, or infinite hysteresis

The system resolves component references in `init`. It uses one ring view for each source entry, even when several alarms read fields from that entry.

It disables an alarm when it finds a duplicate id, an unresolved component, a bad element index, or no free reader slot. It logs the cause as a fault line. It does not stop the target.

The fault kinds are:

- `alarms_duplicate_id`
- `alarms_bad_element`
- `alarms_reader_slot`
- `alarms_unresolved_target`

## Definitions

On its first `execute`, the system publishes one `AlarmDefs` message. That message contains every configured definition, including a definition for a disabled alarm.

The system waits until `execute` because the downlink claims its views during `init`. A message sent from alarm `init` could occur before the downlink view exists.

`AlarmDefs` uses snapshot delivery. The link retains its latest value for a client that connects later.

Each definition contains the display name, text, target, default severity, and each configured limit. The display limits use the same values as the raise checks.

## Evaluation

The system reads the newest record for each watched output every cycle. If the producer has not written a new record, the view returns the last value again. The alarm keeps checking that value.

Before the first source record, the alarm does no work.

For each value, the system picks one of three cases:

| Case | Rule | Counter change |
|---|---|---|
| Breach | The value crosses any raw limit | Raise count increases. Clear count resets. |
| In band | The value has moved past every limit by the hysteresis margin | Clear count increases. Raise count resets. |
| Dead zone | The value is between a raw limit and its recovery margin | Both counts reset. |

A NaN value enters the dead zone. It does not raise or clear an alarm.

Critical takes priority over warning. Once active, an alarm can rise again at a higher severity. The new raise keeps the same occurrence id. Severity does not fall within one occurrence.

The system gives a new occurrence a counter value based on wall-clock microseconds at system creation. It then adds one for each new raise. This is a time-based id seed, not a strict global uniqueness proof.

## Clear and ack rules

A non-latching alarm clears after the recovery count reaches `debounce`.

A latching alarm needs both recovery and an ack. Either may happen first.

For example:

```text
raise -> ack -> recover -> clear
raise -> recover -> ack -> clear
```

An ack must match both the alarm id and the active occurrence id. The system ignores old or unknown occurrences.

Ack messages use a normal route from an uplink:

```python
uplink = m.add("uplink", Uplink(link, msgs=["AlarmAck"]))
m.route(uplink, alarms, msg="AlarmAck")
```

The system drains acks before it checks values. A recovered latch can clear in the same cycle that its ack arrives.

Corrupt ack ring data records `alarm_ack_corrupt`. Corrupt source ring data records `alarm_input_corrupt`.

## Event output

Raises and clears use log delivery. The downlink sends each event in order.

`AlarmRaised` contains the definition id, occurrence id, severity, value, and a short generated message. `AlarmCleared` contains the definition id and occurrence id.

The messages do not contain a timestamp. The receiver gets event time from the message log record.
