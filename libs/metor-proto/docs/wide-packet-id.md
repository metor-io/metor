# 64-bit packet ids

## Problem

`PacketId` is `[u8; 2]`. A table's id is a 16-bit fold of its vtable's postcard bytes, so ids collide often as table counts grow:

| Id size | Tables at 50% collision odds | Odds at 500 tables | Odds at 5,000 tables |
|---|---|---|---|
| 16 bits | ~300 | 85% | ~100% |
| 64 bits | ~5 billion | ~0 | ~0 |

A collision is reported, not silent: `DefError::TableIdCollision` at definition time and `Error::VTableConflict` in the DB. It still stops a valid system from running.

Publishers and subscribers each derive the id from the vtable, so ids cannot be handed out by a central allocator.

## Decisions

- `PacketId` becomes `[u8; 8]`.
- The frame prefix grows from 8 to 16 bytes. The extra bytes are sent on the wire, so the body stays 8-aligned.
- Table ids use the full 64-bit hash.
- Message ids keep their current 16-bit values, zero-extended.
- The header has no version field. `ty` has unused values that can carry a version later.
- There is no compatibility with the old format. All peers upgrade together.
- A mismatched peer is not detected. The length prefix is unchanged, so its packets still frame. Any old-format message with at least 8 body bytes parses under a wrong id, and the DB records it as a message log named for that id.

## Wire format

| Bytes | Size | Field |
|---|---|---|
| 0–3 | 4 | length of header and body, `u32` LE |
| 4 | 1 | `ty` |
| 5–12 | 8 | `id` |
| 13 | 1 | `req_id` |
| 14–15 | 2 | padding, sent as zero, ignored on receive |
| 16+ | n | body |

```rust
pub type PacketId = [u8; 8];
pub const PACKET_HEADER_LEN: usize = 12;

#[derive(TryFromBytes, Unaligned, Immutable, KnownLayout, Debug, IntoBytes)]
#[repr(C)]
pub struct PacketHeader {
    pub packet_ty: PacketTy,
    pub id: PacketId,
    pub req_id: RequestId,
    pub _pad: [u8; 2],
}
```

`PacketId` stays a byte array so `PacketHeader` stays `Unaligned`, and so old-width literals and conversions fail to compile. The field order matches the old header, with `_pad` added last to align the body.

Each packet costs 8 more bytes.

### Alignment

Receivers keep the length prefix at the start of an 8-aligned buffer (`LengthDelReader`, `bbq`). `LenPacket` keeps it at the start of its `Vec`. The body moves from offset 8 to offset 16, so both remain 8-aligned and these stay correct unchanged:

- `ComponentView::try_from_bytes_shape`, including the view over a `LenPacket` body at `db/src/vtable_stream.rs:456`
- padding from `inner.len()` in `LenPacket::push_aligned` and `extend_aligned`
- the `MsgWithTimestamp` and `TimeSeries` bodies

fsw3's `Deframer` hands out bodies at arbitrary offsets and copies them into rings, so it does not depend on alignment.

## Ids

```rust
pub const fn short_id(bytes: [u8; 2]) -> PacketId {
    [bytes[0], bytes[1], 0, 0, 0, 0, 0, 0]
}
```

| Source | Value |
|---|---|
| `table_id` | `fnv1a_hash_64` of the vtable's postcard bytes, with `off_reserved` applied |
| `msg_id`, blanket `Msg::ID` | current 16-bit fold, zero-extended |
| Hand-allocated `[224, n]`, `[225, n]` | zero-extended through `short_id` |
| Fixed-rate stream tables | full `stream_id` (`db/src/lib.rs:2596` truncates it to 2 bytes today) |

`off_reserved` takes a `PacketId` and keeps remapping a first byte of 224 to 223.

Tables and messages are separate namespaces on the wire, keyed by `PacketTy` in fsw3 and by separate maps in the DB's table paths. Frame commands are the exception: the DB stores them in `msg_logs` under their table id (`db/src/lib.rs:1790`), so full-width and short ids share that map. A collision there needs a table hash with six zero upper bytes.

Message ids stay 16-bit because their low bytes name data on disk, and because there are far fewer message types than tables.

## Persistence

Message logs live in `msgs/<u16>` and `db/msgs/<u16>`. These become `u64::from_le_bytes(id).to_string()`, and opening a database parses the name as `u64` (`db/src/lib.rs:296`). A zero-extended id formats to the same decimal, so existing message logs keep resolving. A build from before this change cannot open a database holding a full-width log.

The vtable registry is not restored on open. Tables re-announce, so changing table ids needs no migration for telemetry.

Frame command logs from existing recordings stay under their 16-bit table id. They open and read, but the same command gets a new 64-bit id, so its history splits across two logs. This is accepted rather than migrated.

Postcard messages that embed a `PacketId` change encoding (`VTableMsg`, `CommandInfo`, `LinkInfo`, and others in `wkt/src/msgs.rs`). These are wire messages. The structs the DB writes to disk (`DbConfig`, `MsgMetadata`, component schema and metadata, seals) hold no id. `LINK_PROTOCOL_VERSION` and `NODE_PROTOCOL_VERSION` are bumped.

Panel layouts store `msg:<4 hex>`. Parsing accepts 4 hex characters as a zero-extended id and 16 as a full id. Writing emits 4 when the upper six bytes are zero, so existing layouts and their output are unchanged.

## Sites the compiler will not catch

These use `id[0]`, `id[1]` or fixed offsets and still compile after the type change:

- `metor-fsw-3/src/link/peer.rs:119` and `subscribe.rs:243`: `vec![ty as u8, id[0], id[1], 0]`
- `metor-fsw-3/src/link/batch.rs:165`, `:284`: test offsets
- `metor-panel/src/plot_events/mod.rs:74`, `:482`, and `inspector/registry/builders.rs:376`: id formatting
- `metor-proto/src/types.rs`: `LenPacket::new` writes fields by position and hardcodes `cap + 8`

`fsw/video-streamer` and `fsw/gstreamer` only call `msg_id` and `LenPacket`, so they need no edits.

Sites the compiler does catch include `link/wire.rs:196`, `:270`, `:321`, `db/src/lib.rs:159`, `vtable_stream.rs:30`, `:195`, and `db/cli/src/lib.rs:222`.

Tests that pin 16-bit ids (`channel/tests.rs:13`, `wkt/src/msgs.rs`) need new expected values only where the id's width appears in the assertion. Message values do not change.

## C and C++

`db/examples/client.c`, `db/cpp/helpers.hpp` and `db/examples/db.hpp` hardcode the header and need the new layout. `db.hpp` embeds `helpers.hpp`, so check how it is produced before editing it by hand.

The new header struct is 16 bytes with 4-byte alignment. A payload with 8-byte alignment lands at offset 16 with no compiler-inserted padding.

`msg_id` in `helpers.hpp` returns the zero-extended value, and `PacketType` gains the missing `MSG_WITH_TIMESTAMP` only if a client uses it.

## Testing

Unit tests in `metor-proto`:

- Nominal: packets of each `PacketTy` round-trip with all 8 id bytes. `size_of::<PacketHeader>() + 4 == 16`.
- Nominal: the body of a received packet is 8-aligned through `PacketStream` and `bbq`, and f64 fields decode.
- Nominal: `table_id` differs for vtables that collide under the 16-bit fold.
- Off-nominal: `msg_id` and `Msg::ID` match their old values in the low bytes. Non-zero padding bytes are ignored.
- Failure: a frame shorter than `PACKET_HEADER_LEN` is `InvalidPacket`, and an unknown `ty` is rejected.

Other crates:

- `db`: a log written under the old directory name is read back by id. Two fixed-rate streams whose ids share low bytes get distinct table ids.
- `metor-panel`: `msg:` keys round-trip in both widths.
- `metor-fsw-3`: `packet_head` matches `LenPacket` byte for byte. The link integration tests pass unchanged.
- C and C++: `libs/db/examples` clients build and exchange packets with a current DB.
- Throughput: `PACKET_OVERHEAD` growth still fits existing `conn_cap` budgets in tests with small buffers.
