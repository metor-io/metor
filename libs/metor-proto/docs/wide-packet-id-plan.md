# 64-bit packet ids: implementation plan

Design: [wide-packet-id.md](wide-packet-id.md). Worktree: `metor-wide-packet-id`, branch `sphw/wide-packet-id`, off `sphw/db-sync-better-2`.

The type change breaks every dependent crate at once, so the workspace does not build between steps 1 and 5. Each step ends with `cargo check` and tests for the crates finished so far. Commit after each step.

## 1. `metor-proto` core

`libs/metor-proto/src/types.rs`:

- `PacketId = [u8; 8]`, `PACKET_HEADER_LEN = 12`, `PacketHeader` as in the design.
- Add `short_id([u8; 2]) -> PacketId` as a `const fn`, zero-extending.
- `off_reserved` takes and returns `PacketId`.
- `msg_id` and blanket `Msg::ID`: the same 16-bit fold, through `short_id`. The blanket impl still skips `off_reserved`.
- `table_id`: `fnv1a_hash_64` of the postcard bytes, through `off_reserved`.
- `LenPacket::new`: `with_capacity(cap + 4 + PACKET_HEADER_LEN)`, then write `ty`, `id`, `req_id`, `_pad`.

Tests in `types.rs`:

- Nominal: every `PacketTy` round-trips all 8 id bytes. `size_of::<PacketHeader>() == 12`. `offset_of!(Packet, body) == 12`. A `LenPacket` body starts at byte 16.
- Nominal: `extend_aligned` of `f64` after a `u8` keeps body-relative offsets aligned.
- Off-nominal: `msg_id` low bytes equal the old fold and the high bytes are zero. `table_id` differs for two vtables with the same 16-bit fold. Non-zero `_pad` is ignored.
- Failure: a frame of 11 bytes after the prefix is `InvalidPacket`. An unknown `ty` is rejected.

## 2. `wkt`, `stellar`, `bbq`, `kdl`, `frame`

- `wkt/src/msgs.rs`: every `const ID: PacketId = [a, b]` becomes `short_id` of the same value. Keep the retired-id checks and pins, rewritten through `short_id`.
- Bump `LINK_PROTOCOL_VERSION` to 4 and `NODE_PROTOCOL_VERSION` to 9. Update compat tests that pin old encodings.
- `stellar/src/tests.rs`, `bbq`: fix literals.

New tests:

- `stellar`: a received table's body is 8-aligned and an `f64` field decodes through `PacketStream`.
- `bbq`: the same through `try_recv_pkt`.
- `wkt`: `VTableMsg` and `LinkInfo` round-trip with a full-width id.

`Client::recv` filtering on `next_req_id` is out of scope.

## 3. `db`

- `src/lib.rs:296`: parse `msgs/<id>` as `u64`. A name that does not parse stays `InvalidMsgId`.
- `src/lib.rs:897`, `src/snapshot.rs:181`: name logs with `u64::from_le_bytes(id)`.
- `src/lib.rs:159`, `vtable_stream.rs:30`, `:195`: use `PacketId`.
- `src/lib.rs:2596`: `let id: PacketId = state.stream_id.to_le_bytes();`, removing the `unwrap`.
- `cli/src/lib.rs:222`: `fastrand::u64(..).to_le_bytes()`.
- Tests: replace `1u16.to_le_bytes()`-style ids and `[a, b]` literals through `short_id`. `remote/fsw.rs:122` keeps `id[0]`, since that helper names test commands.

New tests:

- Nominal: a directory named with a 16-bit decimal reopens to the same id as `short_id`. A full-width frame command log round-trips across reopen and snapshot.
- Off-nominal: two fixed-rate streams whose ids share their low 2 bytes get distinct table ids.
- Failure: a non-numeric `msgs/` entry is `InvalidMsgId`.

## 4. `metor-fsw-3`

- `link/wire.rs:196`: `packet_head` writes the new layout. `:270`, `:321`: read the id from `body[1..9]`.
- `link/peer.rs:119`, `link/subscribe.rs:243`: use one shared test helper that builds a body, instead of writing bytes by position.
- `link/batch.rs:165`, `:284`: test offsets.
- `channel/tests.rs:13`, `tests/link.rs:455`, `:644` and others: update literals through `short_id`.
- Check `def.rs:200` and tests with tight `conn_cap` values against the 16-byte overhead.

New test: `packet_head` equals `LenPacket::new` byte for byte for every `PacketTy`.

Run `tests/io_uring.rs` and the `deploy` and `link` tests in OrbStack as well.

## 5. `metor-panel` and the rest of the workspace

- `plot_events/mod.rs:74`, `kind_key_from_string`: write 4 hex characters when the upper six bytes are zero, otherwise 16. Parse both.
- `plot_events/mod.rs:482`, `inspector/registry/builders.rs:376`: format short ids as `aa:bb` and wide ids in full, through one helper.
- `plot_events/mod.rs:552`: `u64::from_le_bytes(id) as usize % len`.
- `views/image_view/source.rs:191` is pixel data, not an id. Leave it.
- `commands/tests.rs:172`, `:348`, `:406`, `tiles/panels.rs:509`, `plot_events/tests.rs`: fix literals.
- `fsw/video-streamer` and `fsw/gstreamer` only call `msg_id` and `LenPacket`. They need no edits, only a build.
- `examples/` build check. `sysstats-fsw3/.../smart.rs` hits on `u16` matched unrelated code; confirm while building.

New tests: `msg:` keys round-trip in both widths, and an old 4-character key parses.

`cargo check --workspace --all-targets` passes at the end of this step.

## 6. C and C++

- `libs/db/examples/client.c`, `libs/db/cpp/helpers.hpp`: new header, `id[8]`, `_pad[2]`. `msg_id` returns 8 zero-extended bytes.
- `libs/db/cpp/vtable.hpp`: check for id use.
- `libs/db/examples/db.hpp`: regenerate if there is a generator, per `libs/db/README.md`. Otherwise apply the same edit.
- Add `static_assert(sizeof(header) == 16)` beside each struct.
- Build `libs/db/examples` and run the clients against a DB built from this branch.

## 7. Verification and review

- `cargo test --workspace` on the Mac and in OrbStack.
- Run `examples/sysstats-fsw3` end to end with the panel on the Mac GPU, checking telemetry, a command, and a reopened recording.
- Antagonistic review by a separate agent, covering the wire layout and every site that the compiler did not catch.

## Risks

- **Silent byte builders.** Sites from the design's list still compile. Step 4's byte-equality test and step 2's alignment tests catch them.
- **Old recordings' frame commands.** Their logs stay under the old 16-bit id. They open and read, but a new frame command id starts a new log. The design records this as accepted.
- **Rebase onto `db-sync-better-2`.** Step 3 touches code that branch is changing. Literal rewrites are the likely conflicts, and they are mechanical to redo.
- **Stale artifacts.** Missing-symbol errors after the rebase are likely stale rlibs. Touch the sources before debugging.
