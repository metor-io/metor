# Peer telemetry between FSW instances

Status: proposed design for discussion; no implementation changes.

Add explicitly contracted publishers and consumers to the telemetry transport.
A publisher listens on a configured TCP port. A consumer connects to a configured
publisher and exposes its data as ordinary local outputs. Each side declares
its own position in its local cycle. Panel observes these connections and their
health; data exchange runs without Panel or a database in the path.

## What exists today

The relevant implementation spans FSW, the protocol crates, and Panel's database
client:

| Code | Current behavior and implication |
| --- | --- |
| [FSW telemetry](../src/telemetry/mod.rs), [link](../src/telemetry/link/mod.rs), [uplink](../src/telemetry/uplink.rs) | `TcpServer` owns the listener and socket tasks. `Downlink` taps selected outputs; `Uplink` imports configured command messages. Incoming tables are ignored. Downlink's instance/frame filters use OR matching, which is unsuitable as an exact endpoint contract. |
| [Resolver](../src/wiring/resolve.rs), [graph validation](../src/coordinator/init/validate.rs) | Cyclic execution follows registration order. `ReceiveAll` systems, including downlink, are deferred to the tail. Ordinary wired systems can run at explicit positions, with forward and delayed frame edges checked. |
| [Port descriptors](../core/src/descriptor.rs), [frames](../core/src/frame.rs) | Descriptors carry table layouts, metadata, size bounds, and snapshot/log delivery. Frame IDs derive from names. Local compatibility permits component subsets; neither names nor that check alone establish an exact remote binary contract. |
| [Panel connection lifecycle](../../metor-panel/src/connections/target.rs), [FSW database client](../../db/src/remote/fsw.rs) | Panel dials, identifies, reconnects, and imports the stream into a local DB. Its client forwards advertised commands upstream. There is no corresponding contracted frame importer into an FSW graph. |
| [Protocol messages](../../metor-proto/wkt/src/msgs.rs), [FSW advertising](../src/telemetry/discovery.rs), [Panel discovery](../../metor-panel/src/connections/discovery.rs) | `LinkInfo` identifies an FSW link and lists command IDs. Vtables and metadata precede data. Local mDNS already exists, but there is no peer ICD handshake or stable node/session identity. |

The link already keeps socket I/O outside the cycle and drops whole outbound
batches for a slow client. It retains snapshot messages, but not table snapshots.
It also assumes one downlink announcement set per server and that its small
control queue drains between cycles. Peer publishing must address those
assumptions explicitly.

## Endpoint contracts

Introduce a versioned **endpoint ICD**: a named, finite set of channels that a
publisher promises to produce. Generate its schema from shared Rust frame/message
definitions and export a data-only manifest for target configuration. Consumers
need the manifest and local port types, not the remote system's implementation.

Each channel has a stable contract-local key and specifies:

- Frame or message identity, complete schema, delivery (`Snapshot` or `Log`),
  and maximum record size.
- For frames: field offsets, fixed size, alignment, scalar encoding/byte order,
  timestamp representation, and dynamic list/map/key bounds.
- For messages: the complete postcard payload schema, not just the packet ID.
- Semantic metadata needed to interpret values, such as units and coordinate
  conventions, plus a declared clock domain.

Give the ICD a human-readable name/version and a hash of a versioned canonical
representation. Exclude local instance names, network addresses, documentation,
and namespace-prefixed component IDs from the hash. Include semantic and layout
changes. Initially require an exact contract match; schema projection and
automatic conversion can come later. A version label alone is insufficient.

At target build time, the publisher binds every ICD channel to exactly one
explicit local output and validates the complete schema and layout. No wildcards
or implicit inclusion of logs/status. The consumer declares the expected ICD
before connecting, so the coordinator can allocate bounded rings and validate
local wiring while the remote node is offline. Reject ambiguous channel keys,
missing bindings, and incompatible local types. Ordinary local compatibility
checks still apply; raw byte transfer additionally requires layout compatibility
at both ends.

A network handshake validates the expected node, endpoint, ICD hash, and wire
encoding before any received data enters local output rings. Report a mismatch
with expected/actual identities and schema details. Native frame bytes are usable
only between compatible layouts and byte orders; other targets need an explicit
adapter or a later portable encoding.

## Configuration and cycle placement

Use shared transport states and separate cyclic systems, following the existing
link/state pattern. Proposed names below are illustrative API, not existing code:

```python
# Contracts are generated artifacts shared by both target configurations.
from vehicle_icd import plant_sensors, actuator_requests

# FSW target; add systems in the order they should execute.
plant_link = m.state("plant_link", PeerClient(
    addr="127.0.0.1:2251", expected_node="plant",
    endpoint="sensors", contract=plant_sensors,
))
actuator_link = m.state("actuator_link", PeerServer(
    addr="0.0.0.0:2252", node="fsw-a",
    endpoint="actuators", contract=actuator_requests,
))

rx = m.add("plant_rx", Consume(plant_link))
control = m.add("control", Controller())
tx = m.add("actuator_tx", Publish(actuator_link))

m.connect(rx.sensors, control.sensors)
m.connect(control.actuators, tx.actuators)
```

On the plant target, reverse the roles: consume `fsw-a/actuators`, execute the
plant, then publish `plant/sensors`. Either role can exist alone. Bidirectional
exchange uses two independently configured publication endpoints; subscribers
never need to listen for telemetry.

`Publish` has explicit input ports and `Consume` has explicit output ports
generated from the contract. Both are ordinary cyclic systems, without
`ReceiveAll`. Placement follows target declaration order and existing edge
validation. Publishing before a producer requires an explicit delayed snapshot
edge. An intermediate publication point therefore needs no global scheduler
change. Contract channel keys map to ports explicitly; the implementation must
also handle distinct channels of the same frame type despite today's ports being
keyed by frame/message ID.

For the first implementation, use one publisher and one publication point per
peer listener, and one consumer per client state. Reject duplicate ownership at
build time. Multiple endpoints may use different ports and positions. This
avoids inheriting the current server's conflicting announcement ownership;
multiplexing publication points on one port can follow later.

## Transport and visibility

Reuse the protocol framing and asynchronous socket machinery, with a dedicated
peer protocol capability and versioned manifest/data messages. Keep existing
`LinkInfo` encoding and ordinary ground telemetry behavior stable. Use separate
peer listeners initially; old ground clients need not understand peer data.
Freeze the peer manifest during graph construction, independently of cyclic
`init` order.

Connection establishment is bounded: identify protocol and node/session, receive
the endpoint manifest, validate it, acknowledge acceptance, then stream data.
Use stable node and endpoint IDs, a new session ID on publisher restart, and
contract-local channel IDs. Existing downlink table packet IDs are assigned per
announcement set and are not stable channel identity. Frame an explicit batch
with source cycle/time and channel records carrying sequence information; the
current concatenation of packets does not expose cycle boundaries to receivers.

Socket tasks validate packet lengths, schemas, dynamic offsets and bounds, and
stage complete records in bounded storage. They never write graph rings directly.
At `Consume.execute`, capture a finite set of ready data and import it. Later
arrivals wait for its next execution. `Publish.execute` samples its inputs at
that point and hands a bounded batch to the socket task without waiting.

Snapshots keep the latest received value per channel; log delivery preserves
per-channel order among delivered records. Retain table and message snapshots
for new connections, tagged with their original timestamp/session/sequence.
Do not replay historical logs on reconnect. Detect log gaps and duplicates;
snapshot coalescing is expected and counted separately from transport loss.
Records are indivisible, but separate channels are not an atomic transaction.
Values that must stay coherent should share one frame.

Bound connections, handshake/packet sizes, queued bytes, and work per cycle.
Continue draining publisher inputs when no client is attached. Queue saturation
or a full destination ring must report loss and continue; it must not block or
panic. Disconnect persistently lagging peers to discard old TCP backlog and
reconnect to current snapshots. Thresholds and queue budgets belong in target
configuration.

## Time, failure, and the three use cases

Preserve source timestamps. Track local monotonic receive age, local import
cycle, connection state, and session separately. A heartbeat detects a quiet or
dead connection; per-channel age detects a producer that stopped updating.
Configured freshness limits determine validity. Do not infer age by subtracting
timestamps from unrelated FSW clocks.

On disconnect, timeout, schema failure, or session change, mark imported data
invalid and clear pending data from the old connection. Existing rings may still
contain the last sample: expose typed validity/freshness status alongside the
data, and require the consuming control/voting logic to gate its use. Never
manufacture a fresh timestamp or zero sample. Reconnect with bounded backoff;
repeat validation and restore validity only with acceptable data from the current
session. Node IDs distinguish configured peers but do not authenticate them;
the initial trusted-network transport retains the existing security model.

- **Plant model:** replace hardware outputs with the importer through ordinary
  target wiring. Independent paced processes can exchange samples, but network
  latency adds variable delay. Local cycle placement guarantees local visibility
  only. Faster-than-real-time simulation and deterministic distributed lockstep
  require a separate step/barrier protocol; existing `process=True` workers already
  provide coordinator-controlled stepping for a local process boundary.
- **Triple redundancy:** each string publishes its local state and independently
  consumes each other string under separate identities and output instances.
  Keep peer samples separate for explicit voting and freshness checks. This
  transport supplies no consensus, quorum, or clock synchronization. Peer
  publishing rejects re-export of imported channels initially to prevent loops.
- **External telemetry:** the remote producer implements the same publication
  contract or uses a gateway to translate its native data into it. Translation,
  units, and time conversion are explicit adapter responsibilities.

## Panel and future discovery

Publish peer connection health, expected/actual ICD identity, freshness, gaps,
drops, and reconnect counters through ordinary ground telemetry. Imported data
can also be downlinked under the local consumer instance name. Extend Panel's
system graph to show remote endpoints and directed contract bindings from the
wiring manifest. Panel is an observer/configuration aid, not the relay.

Initially require explicit addresses and expected identities. Keep endpoint
resolution separate from the client so a later resolver can map a logical
node/endpoint to candidate addresses. Existing `_metor-fsw._tcp.local.` mDNS
support is a starting point: advertise a peer capability, node/endpoint identity,
and contract hash as hints. Static DNS or a registry could handle routed networks.
Discovery must still pass the same connection/ICD checks and must not silently
substitute another redundant string. Identity authentication is a separate future
transport concern. No new discovery mechanism is part of the initial work.

## Decisions for the implementation plan

The recommended baseline is exact ICD matching, independently scheduled
publish/consume systems, explicit TCP endpoints, bounded asynchronous delivery,
and observable freshness. Before detailing implementation, settle the manifest
authoring/export API, channel-to-port aliases, and validity API. Decide whether
the first increment covers both frames and postcard messages or starts with
frames while retaining the existing command uplink.

Validation should cover a two-target round trip at declared cycle positions,
contract/layout rejection, offline startup, reconnect/session changes, malformed
and oversized data, slow consumers, snapshot replay, log gaps, and three peers
using the same frame types without mixing identities. These are implementation
acceptance criteria, not tests added by this design change.
