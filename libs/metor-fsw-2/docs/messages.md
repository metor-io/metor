# Ports, messages, and rings

Systems send frames and messages through typed ports. Both use the same ring type and the same wiring graph. Their record formats and read rules differ.

Ports are the typed connection points between systems. They let one system
publish data without knowing which systems will read it. This keeps system code
independent from the mission layout.

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
- `Output::publish_with`
- `MsgOut::publish`

A publish call drops the new record when the ring is full or too small. It adds to the port's drop count. Cyclic drivers turn a nonzero count into a `publish_dropped` health error after the step.

Log delivery does not make writes block. It tells readers and broad taps to drain records instead of taking only the newest one.

Use the fallible call when the producer must choose what to do after a failed write.

```rust
if output.write(&frame).is_err() {
    health.error("output_full");
}
```

Use `publish` when the normal rule is to keep the cycle moving and report the drop through standard health data.

## Ring size

Each ring has a byte capacity and a fixed reader count. Build computes both before bind.

Frame size comes from `Componentize::MAX_SIZE`. Message ports use a default maximum record size unless a run-time port sets a larger bound.

Snapshot depth defaults to `DEFAULT_DEPTH`. Log rings use a deeper default because each reader drains all records.

The ring never overwrites unread data. A full ring makes `try_write` return `WouldBlock`. The call does not wait for space.

Each input edge claims one reader slot. Each `AllOutputs` grant adds one slot to every output ring. The coordinator also adds spare slots for host readers that claim views through `Registry`.

## Async snapshot inputs

A free-running `AsyncSystem` does not read a cyclic producer's snapshot ring at an unknown point in the cycle. Build gives the async input a private ring.

After all cyclic systems step, the coordinator checks each source. If the source has a new commit, it copies only the newest record into the private ring and wakes the async reader.

```text
cyclic steps
    -> newest source record
    -> async private ring
    -> Input::recv wakes
```

If the private ring is full, the coordinator skips that copy. On the next cycle it tries again with the newest source record.

Message inputs do not use this copy step. They drain their producer rings directly.

## Registry and telemetry

Every output ring has a registry entry. The key is the hash of `<instance>.<port-name>`.

Frame and message entries share this key space. A duplicate key in one instance fails build.

`Registry` exposes all entries, including outputs marked as not telemetered. `AllOutputs` exposes only telemetered entries.

`CommandOut<M>` is an alias for `MsgOut<M>`. The derive and system macros mark it as not telemetered so inbound commands do not return on the downlink.
