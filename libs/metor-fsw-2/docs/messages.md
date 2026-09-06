# Ports, messages, and rings

Systems send frames and messages through typed ports. Both use the same ring type and the same wiring graph. Their record formats and read rules differ.

Ports are the typed connection points between systems. They let one system
publish data without knowing which systems will read it. This keeps system code
independent from the target layout.

Rings carry records from one producer to one or more readers. They preserve
unread data and let each reader advance at its own rate. Snapshot delivery
serves current state. Log delivery serves commands and events that readers
must drain in order.

## Use ports

Publish a frame when a value describes current state:

```rust
imu_out.publish(&sample);

if let Ok(Some(sample)) = imu_in.latest() {
    update(sample.get());
}
```

Publish a message when each command or event matters:

```rust
command_out.publish(&command);

command_in.drain(|command| {
    apply(command);
})?;
```

Frame readers usually ask for the latest sample. Message readers drain every
record that reached each producer ring.

## Port shape

A port descriptor records four main facts:

| Fact | Frame port | Message port |
| --- | --- | --- |
| Default schema | frame table and vtable | packet id and postcard schema |
| Default delivery | snapshot | log |
| Default input fan-in | one producer | zero or more producers |
| Default telemetry | on | on |

`Output<F>` and `Input<F>` carry frames. `MsgOut<M>` and `MsgIn<M>` carry messages.

An output owns one ring writer. A frame input owns one read view into its producer's ring.

A message input may have many producers. It owns one read view for each producer. The framework does not put many writers on one ring.

## Frame records

A frame ring record starts with the frame's fixed `#[repr(C)]` bytes. A dynamic trailer may follow them.

Frame outputs use snapshot delivery. A reader often wants the newest state, not every old sample.

```rust
match input.latest()? {
    Some(frame) => update_from(&frame),
    None => use_startup_default(),
}
```

`latest` consumes older unread records and keeps the newest record pinned. If no new record arrives, the next call returns that same record.

The writer cannot replace a record that any reader still holds. A pinned latest record is one reason snapshot rings need more than one record of space.

Use `Input::drain` when a frame consumer must visit each record.

```rust
input.drain(|frame| {
    record_sample(frame.get());
})?;
```

Use `Input::recv` in a free-running async system. It waits for the next record and returns a grant.

## Message records

A message record contains a two-byte `PacketId` followed by a postcard payload.

```text
| packet id | postcard payload |
```

`NamedMsg::NAME` gives the stable config and registry name. `Msg::ID` gives the wire and edge key.

```rust
message_out.emit(&command)?;

message_in.drain(|command| {
    apply(command);
})?;
```

`MsgIn::drain` reads all current records from each producer ring. It keeps order within each producer. It does not define one order across producers.

The input skips records with another packet id. It also skips payloads that fail postcard decode. Ring corruption still returns a read error.

An unconnected message input is valid. Its drain call does no work.

## Nonblocking writes

All system output writes use the ring's nonblocking `try_write` path.

The fallible calls return an error:

- `Output::write`
- `Output::write_with`
- `MsgOut::emit`

The publish calls keep the system step running:

- `Output::publish`
- `MsgOut::publish`

A publish call drops the new record when the ring is full or too small. It adds to the port's drop count. Cyclic drivers turn a nonzero count into one `publish_dropped` fault line on the log after the step.

Log delivery does not make writes block. It tells readers and broad taps to drain records instead of taking only the newest one.

Use the fallible call when the producer must choose what to do after a failed write.

```rust
if output.write(&frame).is_err() {
    log.fault(LogLevel::Warn, "output_full", "estimate ring full", &[]);
}
```

Use `publish` when the normal rule is to keep the cycle moving and report the drop on the log.

## Ring size

Each ring has a byte capacity and a fixed reader count. Build computes both before bind.

Frame size comes from `Componentize::MAX_SIZE`. Message ports use a default maximum record size unless a run-time port sets a larger bound.

Snapshot depth defaults to `DEFAULT_DEPTH`. Log rings use a deeper default because each reader drains all records.

The ring never overwrites unread data. A full ring makes `try_write` return `WouldBlock`. The call does not wait for space.

Each input edge claims one reader slot. Each `AllOutputs` grant adds one slot to every output ring. The coordinator also adds spare slots for host readers that claim views through `Registry`.

## Async boundaries

A free-running `AsyncSystem` never attaches directly to graph rings. Build
gives every input and output a private ring and leaves an import/export
boundary at the system's registration position.

For a snapshot input, import copies only the newest record when the source has
a new commit. For a log input, it drains every pending record in order. Export
applies the same delivery rule from private outputs to public graph rings.

```text
public inputs -> import -> private async inputs
                              local task runs between cycles
public outputs <- export <- private async outputs
```

The boundary calls import and then export without yielding. Import may wake the
task but cannot run it inline, so that export contains only work completed
before the boundary. If a destination private or public ring is full, copying
drops the record and logs a fault on the coordinator log rather than blocking the cycle.

Two limits of this shape are worth knowing. A snapshot input is latest-wins
per cycle, but the private ring holds `default_depth` records and `recv`
reads them in order, so a task that falls behind sees samples up to that many
cycles old before it reaches the live edge. And a log-delivery frame input
that a task drains slowly fills its private ring; the coordinator drops the
overflow for that task alone, whereas an idle reader attached directly to a
graph ring, such as a registry tap opened after build and never drained,
holds the producer's records for every consumer once the ring is full.

## Registry and telemetry

Every output ring has a registry entry. The key is the hash of `<instance>.<port-name>`.

Frame and message entries share this key space. A duplicate key in one instance fails build.

`Registry` exposes all entries, including outputs marked as not telemetered. `AllOutputs` exposes only telemetered entries.

`CommandOut<M>` is an alias for `MsgOut<M>`. The derive and system macros mark it as not telemetered so inbound commands do not return on the downlink.
