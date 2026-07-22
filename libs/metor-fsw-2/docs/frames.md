# Frames

A frame groups values that share one timestamp. Systems use frames for sampled state such as sensor data, estimates, and actuator requests.

Each frame is a `#[repr(C)]` struct. Its fixed bytes form the start of each ring record.

A frame gives systems one view of state at a point in FSW time. Values
that belong to one sample stay together, and every consumer agrees on their
names and types. The same frame can pass between systems, telemetry, and
storage without a second data model.

## Define a frame

Derive `Frame` and the four zerocopy traits. Mark one field as the timestamp.

```rust
use metor_fsw_2::{Frame, Timestamp};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
#[metor_fsw(name = "imu")]
struct Imu {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    omega: [f64; 3],
    accel: [f64; 3],
}
```

The name sets `Frame::NAME`. It also sets `Frame::FRAME_ID`, which wiring uses to match ports.

The frame vtable lists each field, its type, its shape, and its component id. The ids use dotted names such as `imu.omega`.

`IntoBytes` rejects hidden padding. Add named padding fields when the C layout needs them.

## Fixed frames

Use `Output::write` when every field has a fixed size.

```rust
let frame = Imu {
    timestamp: now,
    omega: [0.1, 0.2, 0.3],
    accel: [0.0, 0.0, 9.8],
};

output.write(&frame)?;
```

The output writes `frame.as_bytes()` as one ring record. An input reads those bytes in place.

```rust
if let Some(frame) = input.latest()? {
    use_omega(frame.omega);
}
```

`latest` returns a `FrameGrant`. It dereferences to the fixed frame type and keeps the ring record live until the grant drops.

## Dynamic fields

`FrameList<T, MAX>` stores up to `MAX` values by index. `FrameMap<V, MAX, MAX_KEY>` stores up to `MAX` values by string key.

```rust
use metor_fsw_2::{FrameList, FrameMap};

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
#[metor_fsw(name = "processes")]
struct Processes {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    recent: FrameList<Process, 16>,
    by_name: FrameMap<Process, 32, 24>,
}
```

The fixed struct stores an 8-byte slot for each dynamic field. The slot gives a table-relative offset. A list slot counts its value bytes. A map slot counts its entry-array bytes. Map keys sit after that array.

The record layout is:

```text
| fixed frame | pad | list values | pad | map entries | map keys |
```

List values sit next to each other. Each map entry has a key offset, a key length, and one value. The map stores all key bytes after its entry array.

The bounds set the largest record size. The coordinator uses that size when it allocates the output ring.

Map keys must not be empty or contain `.`. A key forms one part of a component path, such as `processes.by_name.nav.cpu`.

## Write dynamic fields

Set every dynamic field to `EMPTY` in the fixed value. `FrameWriter` copies the fixed bytes as given. It does not clear slots for you.

```rust
use core::mem::offset_of;

let frame = Processes {
    timestamp: now,
    recent: FrameList::EMPTY,
    by_name: FrameMap::EMPTY,
};

output.write_with(&frame, |writer| {
    let _ = writer.list(
        &frame.recent,
        offset_of!(Processes, recent),
        |list| {
            list.push(first);
            list.push(second);
        },
    );

    let _ = writer.map(
        &frame.by_name,
        offset_of!(Processes, by_name),
        |map| {
            map.insert("nav", nav);
            map.insert("control", control);
        },
    );
})?;
```

The writer aligns each block, writes its data, and patches its slot. If a list, map, or key exceeds its bound, the writer rolls back that field and reports an error.

`Output::publish_with` uses the same build path. It counts an error as a dropped publish instead of returning it.

Each output keeps scratch space for dynamic writes. After it has grown to fit normal records, later writes reuse the same memory.

## Read dynamic fields

`FrameRef::get` and a `FrameGrant` give typed access to the fixed part. `FrameRef::table` gives the full record, including the trailer.

The general read path is `FrameRef::apply`. It walks the vtable and yields the fixed and dynamic component values with their full ids.

The public grant API does not yet provide a typed accessor for each dynamic field. Broad tools should use `apply`. Code that needs a typed dynamic field must decode the table layout itself for now.

## Size and layout rules

- Keep every frame `#[repr(C)]`.
- Derive all four zerocopy traits.
- Use explicit padding fields where needed.
- Start each dynamic slot as `EMPTY`.
- Treat list and map bounds as part of the data contract.
- Keep map keys to one non-empty dotted-path part.
- Do not send native frame bytes between targets with a different byte order or layout.
