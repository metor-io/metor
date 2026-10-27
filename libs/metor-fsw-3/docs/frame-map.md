# FrameMap: keyed dynamic fields in frames

Status: proposal, revision 2.

## Goal

Let a frame carry a runtime-varying set of keyed values, such as one entry
per NIC, process, or disk, at 100 Hz. Record size must track the entries
actually written. The first user is a system statistics example for Linux and
macOS.

```rust
#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
#[frame(max_len = 64 * 1024)]
struct Nics {
    #[frame(timestamp)]
    timestamp: Timestamp,
    nics: FrameMap<String, NicStats>,
}

#[derive(AsVTable, Metadatatize, Componentize, Decomponentize)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
struct NicStats {
    rx_bytes: u64,
    tx_bytes: u64,
    ethtool: FrameMap<String, u64>,
}

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
#[frame(max_len = 128 * 1024)]
struct Procs {
    #[frame(timestamp)]
    timestamp: Timestamp,
    procs: FrameMap<u32, ProcStats>,
}
```

Components are named by key: `host.nics.en0.rx_bytes`,
`host.nics.en0.ethtool.rx_crc_errors`, `host.procs.4211.cpu_ns`.

## What fsw2 does

fsw2 (`metor-fsw-2/core/src/dynamic.rs`, `writer.rs`) splits a record into a
fixed region and a trailer.

- `FrameMap<V, MAX, MAX_KEY>` is an 8-byte slot `{ trailer_off, byte_len }`
  in the fixed struct. Keys are always strings.
- `FrameWriter` copies the fixed bytes. Each `writer.map(&field,
  offset_of!(..), |m| ..)` call appends a fixed-stride entry array
  `{ key_off, key_len, value }`, then a key pool. Key bytes are staged in a
  side buffer and their offsets rebased once the array length is known.
- The vtable uses `Op::Map { name, members, stride, value_offset }`. Offsets
  are table-absolute. `VTable::for_each_field` expands each entry into
  `prefix.key.member` components, and the DB creates them lazily.

What to keep: the wire layout and `Op::Map`. `metor-proto` and the DB
already handle both, including nesting.

What to fix:

1. Writing needs `offset_of!` by hand, a growable packet, a key staging
   buffer, and a rebase pass. A map value cannot have its own nested map
   written, because `ListWriter`/`MapWriter` take values whole.
2. There is no typed read of a map. `FrameRef::table` gives raw bytes.
3. Keys are always `&str`, and bounds are const generics on every field.
4. A slot copied from a received frame points into some other table.

## Proposal

The on-ring and on-wire bytes are exactly fsw2's: fixed region, then
trailer. The changes are in how a frame is bounded, written, and read.

### Types

```rust
#[repr(transparent)]
pub struct FrameMap<K, V> {
    slot: Slot,                    // { off: u32, byte_len: u32 }
    _kv: PhantomData<(K, V)>,
}
```

- No size parameters. The frame's `#[frame(max_len = ..)]` bounds the whole
  record, trailer included, the same way `#[record(max_len)]` bounds a
  message. `MAX_LEN` is that value, so ring and scratch sizing are unchanged.
- `FrameMap` is not `Clone` or `Copy`. A frame holding one cannot be
  copied out of a read, so a stale slot cannot be written back into a new
  record. This also keeps dynamic frames out of `Sample<T: Frame + Copy>`,
  which copies only the fixed region.
- `V` is a fixed `#[repr(C)]` type with `align_of::<V>() <= 8`. It may hold
  `FrameMap`s of its own.

### Keys

```rust
pub trait MapKey {
    /// What `insert` takes: `&str` for `String`, `u32` for `u32`.
    type Arg<'a>;
    /// What a read yields: `&'a str` for `String`, `u32` for `u32`.
    type Ref<'a>;
    fn write(key: Self::Arg<'_>, out: &mut KeyBuf<'_>) -> Result<(), KeyError>;
    fn read(key: &str) -> Option<Self::Ref<'_>>;
}
```

Implemented for `String` and the unsigned integers. An integer key is
written as decimal digits with no allocation. On the wire a key is always a
path segment: non-empty, UTF-8, no `.`. `Op::Map` is unchanged.

### Writing

A new method on `Output<T>`, writing straight into the port's existing
`MAX_LEN` scratch:

```rust
pub fn write_with(
    &mut self,
    fixed: &T,
    build: impl FnOnce(&mut FrameWriter<'_, T>) -> Result<(), WriteError>,
) -> Result<(), SendError>;
```

```rust
out.write_with(&Nics { timestamp, nics: FrameMap::EMPTY }, |w| {
    w.map(|f| &f.nics, nics.len(), |m| {
        for nic in &nics {
            let stats = NicStats { rx_bytes: nic.rx, tx_bytes: nic.tx, ethtool: FrameMap::EMPTY };
            let mut e = m.insert(&nic.name, &stats)?;
            e.map(|s| &s.ethtool, nic.ethtool.len(), |m| {
                for (name, value) in &nic.ethtool {
                    m.insert(name, value)?;
                }
                Ok(())
            })
        }
        Ok(())
    })
})?;
```

- The field is chosen by a projection closure, not `offset_of!`. The writer
  takes the offset from the address the closure returns, relative to
  `fixed`, and rejects a reference outside it.
- `insert` returns a `FrameWriter<'_, V>` scoped to that entry, so a nested
  map is written the same way as a top-level one.
- `map` takes the entry count. It reserves `count * stride` bytes for the
  entry array at the trailer tail. Keys and nested maps then append after
  the reservation in one forward pass, with final absolute offsets at once.
  There is no staging buffer and no rebase. `byte_len` counts the entries
  written, and reserved slots left unused stay zeroed. Inserting past
  `count` returns `WriteError::MapFull`.
- Running past `max_len` returns `EncodeError::Oversize`. It is counted in
  `dropped` like any other failed write.
- A repeated key returns `WriteError::DuplicateKey`. The writer checks by
  linear scan of the keys written so far.
- Plain `write(&T)` still works and sends every map empty.

Steady state allocates nothing. The scratch is allocated when the port is
built, and the writer only borrows it.

### Reading

A frame with `max_len` decodes to a view instead of `&Self`:

```rust
pub struct FrameRef<'a, T> {
    table: &'a [u8],
    at: usize,
    value: &'a T,
}

impl<'a, T> Deref for FrameRef<'a, T> { type Target = T; .. }
impl<'a, T> Borrow<T> for FrameRef<'a, T> { .. }

impl<'a, T> FrameRef<'a, T> {
    pub fn map<K: MapKey, V>(&self, field: impl FnOnce(&T) -> &FrameMap<K, V>) -> MapRef<'a, K, V>;
}

impl<'a, K: MapKey, V> MapRef<'a, K, V> {
    pub fn len(&self) -> usize;
    pub fn iter(&self) -> impl Iterator<Item = (K::Ref<'a>, FrameRef<'a, V>)>;
    pub fn get(&self, key: K::Arg<'_>) -> Option<FrameRef<'a, V>>;
}
```

```rust
for (name, nic) in frame.map(|f| &f.nics).iter() {
    let crc = nic.map(|s| &s.ethtool).get("rx_crc_errors");
    // nic.rx_bytes through Deref
}
```

- Fixed fields read through `Deref`, so existing code using `latest()` and
  `drain()` keeps working.
- Each entry is borrowed in place. Ring payloads are 16-byte aligned and
  entry arrays start on 8-byte boundaries, so any `V` with alignment up to 8
  borrows without a copy.
- A read never trusts the slot. A slot or key outside the table, a
  `byte_len` that is not a multiple of the stride, or a key that is not
  UTF-8 yields an empty map or skips that entry. A read does not panic.

Frames without `max_len` keep `Read<'a> = &'a Self` and the zero-copy encode
path. Nothing about them changes.

### Vtable

The same op fsw2 emits, with no proto, DB, or editor changes:

```rust
raw_field(0, 8, map(&prefix, V::element_fields(String::new()), stride, value_offset))
```

`stride` and `value_offset` follow fsw2's `map_stride`/`map_value_offset`.

### Compile-time checks

`AsVTable` gains one defaulted item, `const DYNAMIC: bool = false`.
`FrameMap` sets it to `true`, and the struct derive ORs its fields. The
`Frame` derive then rejects a frame that holds a map but has no `max_len`,
which would otherwise fail on its first non-empty write.

## Changes

| Where | Change |
| --- | --- |
| `metor-component` | `FrameMap<K, V>`, `MapKey`, `const DYNAMIC` on `AsVTable`. Derive ORs `DYNAMIC`. |
| `metor-fsw-3` | `FrameWriter`, `MapWriter`, `FrameRef`, `MapRef`, `Output::write_with`. |
| `metor-fsw-3/macros` | `#[frame(max_len)]`. When set: `MAX_LEN = max_len`, `Read = FrameRef`, and the `DYNAMIC` check. |

| `db` | Dynamic ingest takes the write lock only when a key is new. See below. |

No changes to `metor-proto`, rings, links, or the editor. Rings already hold
variable-length records bounded by `MAX_LEN`, and the link hands record
bytes to `ingest_table` unchanged.

### DB ingest

`DB::ingest_dynamic` takes the `State` write lock on every record, clones
the vtable, and copies the record into `latest_dynamic_tables` with
`to_vec`. The write lock is needed only to create components for new keys.
`push_field` needs only `&Component`, as on the static path.

- Fast path, under the read lock: walk the record once and check that every
  component exists. If they all do, walk again and push. No vtable clone.
- Slow path, when a component is missing: the current write-lock path
  creates it and pushes. It runs only when a new key appears. A key that
  disappears costs nothing.
- `insert_vtable` creates the latest-record buffer for each dynamic table,
  behind its own mutex. Ingest does `clear` and `extend_from_slice` into it,
  with no allocation and no `State` write lock.

Checking before pushing means a record is never pushed partway before the
fallback. If the extra walk shows up in profiles, cache the resolved
components per table and check whether the key set changed.

fsw2 keeps its own `FrameMap`. It only picks up the defaulted `DYNAMIC`.

## Rate and size

Record bytes per entry are `8 + size_of::<V>()`, rounded to 8, plus the key.
Rough figures for one host at 100 Hz:

| Frame | Entries | Bytes/record | At 100 Hz |
| --- | --- | --- | --- |
| NICs with ethtool | 8 NICs × 60 counters | ~18 KB | ~1.8 MB/s |
| Processes, 10 `u64` each | 300 | ~30 KB | ~3 MB/s |
| CPU per core | 32 | ~1.5 KB | ~150 KB/s |
| Memory | fixed | <100 B | <10 KB/s |

Keys repeat in every record and are a large share of the map bytes.

## Alternatives considered

- Inline fixed capacity (revision 1). Every record is the maximum size, which
  costs too much at 100 Hz.
- Owned maps (`Vec`/arena) in the frame, encoded on write. Needs separate
  read types for every struct that holds a map, which means generated view
  types.
- Messages with a postcard map. No vtable, so the panel cannot plot the
  values.

## Open questions

1. Is 100 Hz the rate for every source? Scanning `/proc/<pid>` for 300
   processes is roughly 90k syscalls/s at 100 Hz, and SMART data changes on
   the order of minutes. I would run processes and disks slower, in their own
   systems.
2. Process keys. The DB keeps every key it has seen as a component, so
   PID keys grow without bound on a long run. Options: key by name and sum
   across same-named processes, or keep the top N by CPU.
3. ethtool counts vary widely. mlx5 exposes over 1000 counters. Allowlist
   per driver, or take what fits under `max_len`?
4. `FrameList` (index-keyed) uses the same writer and reader with `Op::List`.
   Add it now or when needed?

## Example sketch

`examples/sysstats-fsw3`, one system per source.

| System | Frame | Linux | macOS |
| --- | --- | --- | --- |
| `cpu` | `FrameMap<u16, CoreStats>` plus totals | `/proc/stat` | `host_processor_info` |
| `mem` | fixed: total, free, available, cached, buffers, swap | `/proc/meminfo` | `host_statistics64`, `vm.swapusage` |
| `nics` | `FrameMap<String, NicStats>` with nested `ethtool` | `/sys/class/net/*/statistics`, `SIOCETHTOOL` | `getifaddrs` `if_data` |
| `procs` | `FrameMap<u32, ProcStats>` | `/proc/<pid>/{stat,status,io}` | `libproc` |
| `disks` | `FrameMap<String, DiskStats>` with nested `smart: FrameMap<String, u64>` | `/proc/diskstats`, NVMe admin ioctl | IOKit block storage statistics |

SMART needs root or `CAP_SYS_RAWIO` on Linux. macOS exposes little of it
outside private NVMe interfaces.
