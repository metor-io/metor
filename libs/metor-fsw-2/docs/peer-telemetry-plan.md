# Peer telemetry implementation plan

Implements the [peer telemetry design](peer-telemetry-design.md). Proposed scope:
start with frame channels, including snapshot/log delivery and bounded dynamic
fields. Add postcard message channels afterward; existing ground command uplink
continues to serve commands. Use explicit addresses, exact ICD matching, and one
publisher per listener. Discovery, distributed lockstep, voting, and portable
cross-layout encoding remain separate work.

Progress: the [offline contract API](peer-contracts.md) now provides derived
layout/bounds, distinct channel keys, JSON export/import, canonical hashing,
and exact schema comparison. This completes the contract foundation of milestone
1. Recursive dynamic layouts, target/bundle integration, port aliases, and the
typed validity interface remain pending; no peer network data flows yet.

## 1. Define and export endpoint contracts

Primary areas: `core/src/descriptor.rs`, frame derives, pack/contract export,
and `python/metor-config`.

- Add endpoint/channel descriptors generated from shared frame definitions,
  carrying layout, bounds, delivery, semantic metadata, and clock domain.
- Export a data-only manifest with a versioned canonical hash. Include the
  manifest in target bundles so offline validation needs no remote connection.
- Separate channel identity from frame type identity. Resolve named channel
  bindings to distinct ports so one endpoint can carry several channels of the
  same frame type; preserve existing wiring behavior.
- Specify a typed per-channel status output for validity, session, sample
  sequence, and age. This is part of the consumer interface from the start.

Check manifest/hash stability, semantic and layout mismatch rejection, and
multiple channels sharing a frame type.

## 2. Add target declarations and graph bindings

Primary areas: `src/ir.rs`, `src/wiring/`, coordinator binding, Python builtins.

- Add `PeerServer`, `PeerClient`, `Publish`, and `Consume` declarations to Python
  and Rust configuration. Record endpoint contracts and bindings in wiring IR;
  update its version and consumers as needed.
- Generate publisher inputs and consumer outputs from the contract. Validate
  exact layouts, complete bindings, single transport ownership, and rejection
  of direct re-export of imported peer channels.
- Register both systems in ordinary cycle order without `ReceiveAll`; use
  existing forward/delayed edge validation and reader allocation.
- Keep client connections out of graph construction so targets start offline.

Check configuration round trips, missing/ambiguous bindings, offline startup,
and imports/publications at intermediate cycle positions.

## 3. Implement the peer connection protocol

Primary areas: `metor-proto/wkt`, protocol transport helpers, `src/telemetry/`.

- Define versioned peer identity, manifest, acceptance, heartbeat, and batch
  messages. Include node/endpoint/session identity, channel IDs, source cycle,
  timestamps, and sequence numbers.
- Add listener and reconnecting client tasks. Validate identity, contract, and
  encoding before accepting data; bound handshake time and packet allocation.
- Reuse framing/socket helpers while keeping peer listeners and protocol state
  separate from ground telemetry. Leave existing `LinkInfo` encoding stable.
- Validate complete frame records, including dynamic offsets and bounds, before
  staging them. Isolate old connection generations during reconnect.

Check wrong-peer/ICD rejection, malformed and oversized packets, handshake
timeouts, reconnects, and existing ground-link compatibility.

## 4. Connect transport to cycle execution

Primary areas: new peer systems and queues under `src/telemetry/`.

- `Publish.execute` samples explicit inputs and submits bounded batches.
  `Consume.execute` imports a finite set of ready records into local rings.
- Coalesce snapshots, preserve delivered log order, retain snapshots for new
  clients, and detect gaps/duplicates. Preserve original source timestamps.
- Bound connection counts, queued bytes, and work per execution. Handle full
  queues/rings with counted loss; disconnect persistently lagging clients.
- Drain publisher inputs without subscribers. Exercise queue saturation without
  relying on the existing link's control-queue drain assumption.

Check two-target exchange, cycle visibility, snapshot replay, log ordering/gaps,
and slow or absent subscribers without blocking or panicking the cycle.

## 5. Complete freshness and fault behavior

- Implement heartbeat, disconnect, and per-channel freshness limits using local
  monotonic time and explicit source/session information.
- Invalidate status on failures and reset pending data on session changes.
  Retained old samples must not become fresh merely because of reconnect;
  carry producer-side sample age or require a new publication for validity.
- Publish health, contract mismatch details, loss counters, and reconnect state
  through ordinary ground telemetry.
- Wire validity into example controllers/voters so stale ring contents are
  never silently treated as current input.

Check stopped producers, quiet connections, publisher restarts, unrelated
clocks, stale replay, and recovery after fresh data arrives.

## 6. Add Panel visibility and runnable examples

Primary areas: `metor-panel` system graph, FSW examples, integration tests/docs.

- Show remote endpoints and directed contract bindings from the wiring manifest,
  with health/freshness available through normal telemetry.
- Add a paced FSW/plant example with both directions, a three-string example
  that keeps identical frame types distinct by peer, and a small external
  publisher example using the same manifest.
- Document contract generation, cycle placement, endpoint setup, and failure
  behavior. Keep address resolution behind an interface for later discovery.

Finish with end-to-end disconnect/restart and saturation scenarios, plus the
existing telemetry, wiring, Python configuration, and relevant Panel checks.
Milestones 1–5 form the transport release; milestone 6 makes it reviewable and
usable across all three target scenarios.
