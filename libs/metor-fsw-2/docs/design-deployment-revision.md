# Deployments: revision, one ring for everything

Three sections of the deployment design have landed and run
([comms](design-deployment-comms.md), [gateway](design-deployment-gateway.md),
and the two before them). Using them surfaced a simpler shape. This document
states it, names what it reverses, and lists the decisions the reviewer makes
before the design documents are edited and the plans written.

## Design

Every telemetered output on a target is already a ring
(`libs/metor-fsw-2/ring`). The revision makes the ring the one structure data
moves through, on every member and in the db:

1. **A mirror carries a subset, chosen by its consumers.** `Subscribe` mints
   only the frames the subscriber's edges read, plus any it names. The link
   filters per connection, so a publisher serves one wide set and each
   subscriber pays for its subset.
2. **The gateway ingests into rings, like any member.** `Ingest` is a mirror
   of every instance of the source member, the coordinator included. The db
   is fed from rings by `Record`, never from the wire. Alarms, presets, and
   ordinary systems run on the gateway unchanged, because its data is rings.
3. **The db is fed from rings, and only from rings, on the gateway.**
   `Record` reads each frame ring once, maps the record to components
   through the frame's vtable, and pushes into the db's heads; the db
   persists from there. Rings are the transport unit, components the
   storage unit, and the vtable maps one to the other.
4. **Presets live on the gateway.** It has every member's components, and a
   preset names them through port handles, typed, from any member.

The link keeps one wire format, one announce, one identity check. What
changes is that the gateway holds no copy of a member's data outside its
rings and the db's own storage.

## Changes, and what each reverses

### 1. Subscription filtering on the link

A subscriber sends one `Subscribe` request after the identity check, listing
the table and message ids it wants. Table ids are content hashes of the
prefixed vtable, so the subscriber computes them from its own mirror
descriptor and the peer's namespace and instance, before the announce
arrives. The server keeps a per-connection allow-set and copies selected
packets into a per-connection buffer; a connection that sends no request
keeps the shared batch, so the panel, the db, and the gateway's ingest are
unchanged. Retained snapshots filter the same way.

Reverses: comms "Non-goals", server-side per-client filtering, and the
"Alternatives" entry rejecting it for now.

### 2. Frame subsets on `Subscribe`, inferred from edges

At emission the recorder walks the target's `connect` and `route` edges,
collects every mirror port they read, and emits that set as the mirror's
frames. `Subscribe(…, frames=[…])` unions an explicit list for readers that
are not edges. Alarms resolve against the registry and fail loudly on a
missing frame. Presets move to the gateway (change 4), so nothing on a
member reads a mirror silently. Both lists empty means every frame, today's
behaviour.

`PeerSpec` gains `frames: Vec<String>` (serde default, empty). The mirror
arm at resolve filters the type's outputs to it, and the client requests
those ids.

Reverses: nothing; comms rejected a mirror *typed* by its consumers, and
this keeps the type.

### 3. The gateway ingests into rings

`Ingest(db, plant_link)` mints one mirror per instance of the source member:
pack instances from their manifests, Python `@system` instances from the
member's `program.wasm`, the coordinator's host ports and built-ins from the
registry. One connection per member routes packets into all of them, the
subscribe client generalised from one instance to a member. A gateway
bundle carries each source member's pack manifest sidecars and program,
never its dylibs, so a ground host never loads a flight pack.

Ingested frame rings use log delivery, so every received record is kept and
drained; `Subscribe` on a member keeps snapshot delivery. That is one
delivery override on the mirror descriptor, on by default for `Ingest`.

`Record` taps every ring on the gateway, so the db sees the mirrored
members and the gateway's own telemetry through one path. The db-direct
ingest (`fsw_stream` into the gateway db) is deleted from the gateway; it
stays in the db crate for the panel's direct-link connections.

Reverses: gateway decision 3 (ingest as N copies of the panel's stream) and
the comms "Alternatives" entry rejecting ingest-as-mirrors. Its two
objections, `@system` descriptors and coordinator ports, are answered above.

### 4. Presets on the gateway, referenced by handle

Preset, dashboard, and outline builders accept port handles from any member
and qualify the component path from the handle's target namespace.
`Trace(plant_sim.sensors, element=1)` renders `plant.plant.sensors.gyro_b`.
Strings stay as an escape hatch and must be fully qualified. `PresetDefs`
reaches the panel through `Record` and the mirror's message sync, which
landed; only one member publishes presets, so the fold collision named in
the gateway design goes away.

Alarms stay two-tier: onboard alarms on members for autonomy, fleet alarms
on the gateway over mirrored rings, both the same `Alarms` system.

Reverses: gateway decision 3 (snapshot folds out of scope) becomes moot for
presets; comms "Seams" on `Presets` qualification is extended, not changed.

### Out of scope: the ring as the db's log

The db's message-log disruptor is the ring's algorithm, and replacing it
was considered. Without frames in the write-ahead log it is a like-for-like
swap of a working structure, so it is left out. The step that would earn
it, a file-backed frame ring as a durable log with the db's heads derived
from it, needs two things this revision does not add: a gateway ring
region on disk, and an unpinned read on the ring so a connection can take
the newest record without holding a position the writer must respect.
Both are named here as the follow-on.

One rule from that analysis applies now and fixes an existing hazard: the
db's message-sync tails and forwarder hold cursors on the message WAL, so
a stalled but live mirror connection fills the WAL and ingest refuses
every message for everyone. Connection tails should follow the persisted
message nodes with their own positions, the way frame streams follow the
head, and the persister should be the WAL's only reader. That is a small
db change and lands with plan C, where the gateway's connections start to
matter.

## Decisions for the reviewer

1. **Dynamic readers never hold ring or WAL positions.** The ring is
   lossless: a lagging reader stalls the writer, which then drops at the
   source. `Record` is each ring's one reader on the gateway, and
   connections are served from the db's heads and sealed nodes, frames and
   messages alike. Recommendation: keep this rule, apply it to the message
   WAL's connection tails in plan C, and state it in the db docs.
2. **Ingest delivery default.** Log delivery for every ingested frame ring,
   sized from the source's rate and the gateway's cycle; a gateway that
   cycles slower than a source never drops samples. Recommendation: yes,
   with the ring depth as an `Ingest(depth=)` param defaulting to a second
   of the source's rate.
3. **`Publish` survives as sugar.** With filtering, a member's ground
   `Downlink` serves any subscriber. `Publish(state, instances)` stays as
   "a second server so peers never share the ground link", and the
   publisher rule already prefers it. Recommendation: keep it, document the
   common case as one link.
4. **Frames inference scope.** Edges only, with `frames=` for the rest, and
   no string scan of presets. Recommendation: yes; presets move off members.
5. **The durable-ring follow-on.** File-backed frame rings on the gateway
   with heads derived from them, plus an unpinned ring read for
   connections. Recommendation: a separate design after this revision
   lands; it is where a disruptor replacement would pay for itself.

## Plans

Three plans; each ends with a green tree and a commit.

- **B. Link filtering and frame subsets.** The `Subscribe` request and
  per-connection allow-set on the link; `PeerSpec.frames`; edge inference
  and `frames=` in Python; the mirror arm and client request; the example's
  fsw mirror narrowed with no annotation.
- **C. Ring-backed gateway.** `Ingest` as mirrors of every instance with the
  three descriptor sources; log-delivery override; the gateway bundle
  carrying manifests and programs; `Record` unchanged; db-direct ingest
  removed from the gateway; message-sync tails moved off the WAL onto the
  persisted nodes; the gateway test and example updated.
- **D. Presets on the gateway.** Handle-based preset references from any
  member; the adcs example's dashboards, fleet-level alarms, and outline
  move to `gw`; the panel needs nothing.

B, C, and D are independent of each other. C is the largest; B and D can
run alongside it.
