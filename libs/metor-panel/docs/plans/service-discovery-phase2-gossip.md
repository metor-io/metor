# Phase 2 — SWIM gossip mesh for service discovery

*2026-07-21 · follows Phase 1 (mDNS/DNS-SD), which is landed on `sphw/logging`*

> Companion docs (in `libs/metor-fsw-2/`): `service-discovery-report.md` (prior-art
> survey) and `service-discovery-phase1-plan.md` (the mDNS work already shipped).

## Where Phase 1 left us

An fsw link advertises `_metor-fsw._tcp.local.` under a human name; the panel browses and
upserts a `ConnectionTarget` per resolved instance through `RegistryHandle`. That gives
**zero-config local discovery** but nothing more: mDNS is **L2-only** (no cross-subnet), has
**no real failure detector** (a crashed node lingers until its TTL expires), and doesn't let
fsw nodes know about *each other* — only the panel learns of them, and only on its own link.

## What Phase 2 adds

A **SWIM gossip membership mesh across the fsw nodes**, giving the three things mDNS can't:

1. **Cross-subnet reach** — gossip is unicast, so once a node knows *one* peer it converges
   on the whole mesh regardless of routers.
2. **Real liveness** — SWIM actively probes; a dead node is detected in seconds (mDNS waits
   out a TTL), and a node's departure is a first-class event.
3. **Fleet awareness** — every fsw knows every other fsw's name/address/namespace, and the
   panel learns the **whole fleet from any one node it's connected to**.

Still **no central registry**: seeding is peer-to-peer (mDNS on the link + a static seed list
across subnets), and membership is **authenticated by a pre-shared key**.

---

## Design overview

```
   fsw node A ──gossip(UDP,PSK)── fsw node B ──gossip── fsw node C
      │  (foca: SWIM membership + failure detection)        │
      │  seeded by mDNS (local) + static seeds (cross-subnet)
      │
      └── publishes its live Membership view as a *retained snapshot*
          on its TCP link ──────────────► panel (connected to A)
                                            upserts a ConnectionTarget
                                            per member → same RegistryHandle
```

**Gossip is server-side (fsw only).** Each fsw runs a foca SWIM agent on a UDP socket. The
panel does **not** join the UDP mesh; it learns the fleet from the `Membership` snapshot on
whatever link it's already connected to (found via Phase-1 mDNS, or typed manually), and
upserts those peers as discovered targets through the *same* `RegistryHandle` the mDNS
browser uses. So the panel's discovery seam stays uniform and the GUI never speaks UDP
gossip. (Panel-as-gossip-member is a considered alternative — see below — deferred.)

**Why this shape.** The multi-node value (membership + liveness across subnets) is entirely an
fsw-fleet concern; putting gossip there and surfacing it over the existing TCP link reuses the
retained-snapshot pipeline (`LinkState::set_retained_slots`/`retain`) that already replays
latest-wins state to new connections. The panel gets cross-subnet discovery **for free** once
connected to any node, with no new UDP socket, no foca dependency, and no new failure mode in
the GUI.

### Crate: `foca` (not `al8n/memberlist`)

`foca` is transport-, identity-, and codec-agnostic and **performs no I/O itself** — you feed
it datagrams and drive its timers, owning the socket. That fits `stellarator` exactly: we run
it on `stellarator::net::UdpSocket` (already exists — `libs/stellarator/src/net/udp.rs`),
encode with `postcard` (already the wire codec), and identify members however we like.
`memberlist` ships its own transport/runtime assumptions and would fight the custom runtime.
foca also implements the SWIM Suspicion+Infection extensions (the Lifeguard-class robustness
that matters under a busy flight scheduler).

### Identity

foca gossips member **identities**, so put the metadata *in* the identity and it propagates
for free:

```rust
struct NodeId {
    gossip: SocketAddr,   // the UDP addr — the dedup key (foca Identity::Addr)
    link: SocketAddr,     // the TCP link addr a panel connects to
    name: String,         // the human node name (Phase-1 node_name)
    namespace: Option<String>, // the target namespace, for grouping/prefixing
    bump: u16,            // renewal counter for clean auto-rejoin
}
```

foca's `Identity` trait keys membership on `Addr` (`gossip`); `name`/`link`/`namespace` ride
along and reach every peer through normal membership dissemination. `renew()` bumps `bump` so
a restarted node supersedes its own stale entry.

### Transport & port

Gossip binds **UDP on the same port number as the link's TCP** (`local_addr.port()`) — one
port for both. Peers gossip to `peer_link_ip:port` over UDP. Every datagram is wrapped in an
**AEAD (ChaCha20-Poly1305 via `ring`, already in-tree) keyed by a pre-shared key**: a node
without the key can neither join nor forge membership, which is what authenticates the mesh.
(Key distribution is the operator's job — out of scope here; sourced from config/env.)

### Seeding

- **Local link:** the fsw **also browses mDNS** (Phase 1 only browses from the panel). On
  discovering a peer it reads the peer's gossip port from a new TXT record and `announce`s to
  it — zero-config local mesh formation.
- **Cross-subnet:** a small **static seed list** in config (a few known `host:port`s). One
  reachable seed is enough; SWIM converges from there.

### Surfacing membership to the panel

The gossip agent maintains a live membership set. On any change it frames a **`Membership`
snapshot message** and publishes it on the link via the retained-snapshot path, so:
- **new** panel connections get the current fleet in the announce replay, and
- **live** connections get updates as they happen.

The panel decodes `Membership`, and for each member other than the node it's connected to,
upserts `ConnectionTarget::tcp(name, link_addr)`; a member that drops out is removed — the
same `RegistryHandle` ops the mDNS browser already emits.

---

## Work packages

### WP-A — Gossip identity, codec, and the `Membership` message *(`wkt` + `metor-fsw-2`)*

- `wkt`: a `Membership` msg (a new `PacketId`) carrying `Vec<NodeDescriptor>` where
  `NodeDescriptor { name, link_addr, namespace, alive }`. This is the panel-facing contract;
  version it the same internal-only way Phase-1 constants live in `wkt`.
- `metor-fsw-2`: the `NodeId` identity + its foca `Identity` impl + `postcard` codec glue.
- **Acceptance:** identity round-trips through postcard; `Membership` decodes on the panel
  side.

### WP-B — PSK datagram encryption *(`metor-fsw-2`)*

- A thin AEAD wrapper over `ring` (ChaCha20-Poly1305): `seal(key, nonce, plaintext)` /
  `open(key, ciphertext) -> Option<plaintext>`, nonce per datagram. Wrap every foca datagram
  in/out.
- **Acceptance:** a datagram sealed with key K fails to open under K′; tampered ciphertext is
  rejected.

### WP-C — foca agent on the fsw *(`metor-fsw-2`, new `telemetry/gossip.rs`)*

- Add `foca` to `metor-fsw-2/Cargo.toml`.
- `GossipAgent`: binds `stellarator::net::UdpSocket` on the link port, runs foca with a
  `Runtime` impl that (a) sends datagrams (PSK-sealed) to peers, (b) schedules SWIM timers on
  the stellarator runtime, and (c) on member up/down updates a shared `Membership` set.
  Driven by two raced loops: recv→`handle_data`, timer→`handle_timer`.
- Owned/started by `LinkState` (it already has `local_addr` + `node_name()`), spawned in
  `SharedLifecycle::start` next to the accept loop and advertiser; torn down in `shutdown`.
  Runs only when gossip config is present (opt-in).
- **Acceptance:** two in-process agents seeded to each other converge to a 2-member set;
  killing one drops it from the other within the failure-detector window.

### WP-D — Seeding *(`metor-fsw-2`)*

- Add a **gossip-port TXT record** to the Phase-1 advertisement (`telemetry/discovery.rs`).
- Give the fsw an **mDNS browser** (mirror of the panel's Phase-1 browse; consider extracting
  a shared helper): on resolve, `announce` to the peer's gossip addr.
- Honor a **static seed list** from config: announce to each at startup.
- **Acceptance:** two fsw on one link mesh with no seed config; a seeded pair across subnets
  meshes with one static seed.

### WP-E — Publish `Membership` as a retained snapshot *(`metor-fsw-2`)*

- On membership change, frame the `Membership` msg and push it through the link's
  retained-snapshot path (`LinkState::retain` into a reserved slot + `broadcast` to live
  connections), so late joiners get it in the replay. (Decide: direct-publish from the gossip
  task via a small `LinkState` method, vs. a dedicated cyclic `GossipSystem` output tapped as
  `Snapshot` — the latter is more idiomatic with the tap model; the former is less plumbing.
  Lean toward the small `LinkState` method unless membership needs to be a first-class tapped
  registry entry.)
- **Acceptance:** a panel connecting after the mesh formed receives the full membership in its
  first replay; a later change arrives live.

### WP-F — Panel consumes `Membership` *(`metor-panel`)*

- A consumer (near `connections/discovery.rs`) decodes the `Membership` msg from a connected
  link and upserts/removes `ConnectionTarget`s through `RegistryHandle`, skipping the node the
  panel is already connected to. Dedup is automatic: `tcp:<addr>` ids coalesce a
  gossip-discovered peer with an mDNS-discovered or manually-typed one.
- **Acceptance:** connect the panel to one node of a 3-node mesh → the other two appear in the
  picker by name; kill one → it drops within a poll interval.

### WP-G — Config surface *(`metor-fsw-2` Python IDL + Rust builder)*

- Extend `LinkParams` (and `TcpServer(...)` / `serve_named`) with optional gossip config:
  `gossip` enable, `seeds: [host:port]`, `gossip_key` (or an env-var reference — avoid
  baking secrets into the IR; prefer `gossip_key_env="METOR_GOSSIP_KEY"`). No KDL (dead); no
  CLI flag unless asked. Absent config = gossip off, so existing targets are unchanged and
  the golden IR is stable.
- **Acceptance:** `TcpServer(addr=..., name=..., gossip=True, seeds=[...])` round-trips into
  `LinkParams`; unset emits identical params to today.

### WP-H — Tests & manual verification

- Unit/integration: identity+codec round-trip (WP-A), AEAD reject (WP-B), 2-agent convergence
  + failure detection (WP-C), seed paths (WP-D), replay-carries-membership (WP-E), panel
  upsert/remove (WP-F).
- Manual multi-host: 3 fsw across ≥2 subnets, one static seed bridging them; a panel connected
  to one node sees all three by name and reflects a kill.

---

## Security

The PSK-AEAD wrap is the whole authentication story for the mesh: membership can't be joined
or forged without the key. It does **not** by itself secure the **TCP link** the panel then
dials — that's still plaintext and unauthenticated (the Phase-1.5 gap). So:

- Gossip authenticity ≠ link authenticity. A correct membership entry still points at a
  plaintext link. Treat a discovered address as a hint; the link needs its own auth
  (Phase 1.5) before this runs on an untrusted network.
- Key distribution is out of scope — the operator provisions the same PSK to every node
  (config/secret store/env). Rotating it is a fleet-wide restart for now.
- Prefer `gossip_key_env` over an inline key so the secret never lands in the serialized IR
  or a bundle.

---

## Alternatives considered

- **Panel as a full gossip member** (foca + UDP on the GUI, subscribing to membership
  directly). Upside: the panel discovers cross-subnet nodes *without* first connecting to any,
  and survives the loss of any single node's view. Downside: UDP gossip in the GUI, a foca dep
  on the panel, and the panel churning fleet membership as it comes and goes. **Deferred** —
  revisit if "discover the whole fleet before connecting to anyone" becomes a real need.
- **`al8n/memberlist`** instead of foca — heavier, brings its own transport/runtime; rejected
  for the stellarator fit reasons above.
- **DHT / libp2p** — over-scoped (internet-scale routing we don't need on an L2/seeded mesh),
  per the prior-art report.

## Risks & open questions

- **foca API specifics** — the `Identity`/`Runtime`/`Codec`/`BroadcastHandler` surface and its
  timer contract need a careful first read; budget time for a spike before WP-C. (Whether node
  metadata rides in `Identity` vs a custom broadcast is the main modeling call.)
- **Panel view is only as wide as its connections** — with the server-side design, a panel
  connected only to node A loses visibility if A dies. Mitigation: the panel can connect to
  several nodes; membership dedups. The panel-as-member alternative removes this entirely.
- **Gossip-vs-link address** — the mesh gossips the *link* TCP addr for panels to dial. A node
  behind NAT/port-mapping would advertise an unreachable link addr; fine on an L2/routable
  network (our stated assumption), a concern only if this later spans NATs.
- **Same-port UDP+TCP** — binding UDP on the link's port number is clean but assumes the port
  is free for both protocols (normally true); fall back to `link_port` + an offset if not.
- **PSK bootstrap** — no key = no mesh. Decide the "gossip on but no key" behavior (refuse to
  start and log, rather than run unauthenticated).

## Touch list

| File | Change |
|---|---|
| `metor-proto/wkt/src/msgs.rs` | WP-A `Membership` msg + `NodeDescriptor` |
| `metor-fsw-2/src/telemetry/gossip.rs` *(new)* | WP-C foca agent, WP-B AEAD, WP-D seeding |
| `metor-fsw-2/src/telemetry/discovery.rs` | WP-D fsw mDNS browse + gossip-port TXT |
| `metor-fsw-2/src/telemetry/link.rs` | WP-C spawn/teardown; WP-E publish membership snapshot; `LinkParams` gossip fields |
| `metor-fsw-2/src/ir.rs` + `wiring/builder.rs` | WP-G `tcp_server`/`serve` gossip params |
| `metor-fsw-2/python/metor-config/metor_config/__init__.py` | WP-G `TcpServer(..., gossip=, seeds=, gossip_key_env=)` |
| `metor-fsw-2/Cargo.toml` | WP-C `foca`; WP-B `ring` (already in tree) |
| `metor-panel/src/connections/discovery.rs` (or sibling) | WP-F consume `Membership` → `RegistryHandle` |

## Suggested order

`WP-A → WP-B → WP-C` (mesh forms in-process, PSK-secured) `→ WP-D` (real seeding) `→ WP-E/WP-F`
(surface to panel) `→ WP-G` (config) `→ WP-H` (tests + multi-host). WP-C is the spike-and-risk
package; do the foca read first. Commit at each WP boundary.
