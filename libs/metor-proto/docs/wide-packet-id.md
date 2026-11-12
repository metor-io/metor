# 64-bit packet ids

## Problem

`PacketId` was `[u8; 2]`. A table's id is a hash of its vtable's postcard bytes, folded to 16 bits, so ids collided often as table counts grew:

| Id size | Tables at 50% collision odds | Odds at 500 tables | Odds at 5,000 tables |
|---|---|---|---|
| 16 bits | ~300 | 85% | ~100% |
| 64 bits | ~5 billion | ~0 | ~0 |

A collision is reported, not silent: `DefError::TableIdCollision` at definition time and `Error::VTableConflict` in the DB. It still stops a valid system from running.

Publishers and subscribers each derive the id from the vtable, so ids cannot be handed out by a central allocator.

## Decisions

- `PacketId` is `[u8; 8]`.
- The frame prefix is 16 bytes. Two padding bytes are sent so the body stays 8-aligned.
- Table ids use the full 64-bit hash.
- Message ids keep their 16-bit values, zero-extended.
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

`PacketId` stays a byte array so `PacketHeader` stays `Unaligned`. The body is 8-aligned whenever the buffer holding the length prefix is, which table views over the body rely on.

Each packet costs 8 more bytes than before.

## Ids

| Source | Value |
|---|---|
| `table_id` | `fnv1a_hash_64` of the vtable's postcard bytes, with `off_reserved` applied |
| `msg_id`, blanket `Msg::ID` | the 16-bit fold, zero-extended by `short_id` |
| Hand-allocated `[224, n]`, `[225, n]` | zero-extended by `short_id` |
| Fixed-rate stream tables | the full `stream_id` |

Tables and messages are separate namespaces on the wire. Frame commands are the exception: the DB stores them in its message logs under their table id, so full-width and short ids share that map. A collision there needs a table hash whose upper six bytes are zero.

Message ids stay 16-bit because their values name data on disk, and because there are far fewer message types than tables.

## Persistence

Message logs live in `msgs/<id>`, where `<id>` is `u64::from_le_bytes(id)` in decimal. A zero-extended id formats to the same decimal as the old 16-bit name, so existing message logs keep resolving. A build from before this change cannot open a database holding a full-width log.

The vtable registry is not restored on open. Tables re-announce, so changing table ids needs no migration for telemetry.

Frame command logs from existing recordings stay under their 16-bit table id. They open and read, but the same command now has a 64-bit id, so its history splits across two logs. This is accepted rather than migrated.

Postcard messages that embed a `PacketId` (`VTableMsg`, `CommandInfo`, `LinkInfo` and others) change encoding. These are wire messages; the structs the DB writes to disk hold no id. `LINK_PROTOCOL_VERSION` and `NODE_PROTOCOL_VERSION` are bumped.

Panel layouts store message overlays as `msg:<hex>`: 4 hex characters when the upper six bytes are zero, otherwise 16. Existing layouts parse unchanged.
