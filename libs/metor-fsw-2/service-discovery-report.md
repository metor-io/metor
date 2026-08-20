# Service Discovery for metor-fsw — Prior Art & Recommendation

*2026-07-21*

## Problem

Today metor-fsw is almost always **one instance on one host**, addressed by a hardcoded
`SocketAddr`. metor-panel connects by typing or persisting a `host:port` string; the fsw
link identifies itself with a `LinkInfo` capability blob that carries **no name**. We want
peers — other metor-fsw instances, and the panel — to **auto-discover each other by a
human name** on an L2-routable network, ideally **with no central registry** (purely P2P /
gossip). This report surveys how the field solves this and recommends a path.

The good news up front: the panel already has the right seam. `connections/mod.rs`
documents a push-based `RegistryHandle` whose doc literally names *"an mDNS scan, a cloud
poll"* as intended producers — a discovery thread just `upsert`s named `ConnectionTarget`s
and the picker/persistence consume them unchanged. So most of the work is the discovery
*mechanism* plus adding a name to the fsw side.

---

## The design space, in one picture

Decentralized discovery splits into three mechanisms, each answering "how does a name
become an address" differently:

| Mechanism | How it works | Name → address | Central component | Scope | Liveness |
|---|---|---|---|---|---|
| **Multicast query/announce** (mDNS/DNS-SD, SSDP, WS-Discovery, DDS SPDP) | Ask/announce on a well-known multicast group; every peer answers for itself | Query a service type → get records naming instances → resolve to host:port | **None** | **L2 only** (multicast is link-local) | Announce + TTL expiry; no true failure detector |
| **Gossip membership** (SWIM: Serf/Consul/memberlist/foca) | Each node probes a random peer per round; membership + metadata spread epidemically | Names/tags ride in the membership record, gossiped to all | **None** (seed = any existing peer) | **Any** (unicast, crosses subnets) | **First-class** — SWIM *is* a failure detector |
| **DHT / overlay** (Kademlia, libp2p, BitTorrent) | Structured key-space; find the peer whose ID is XOR-closest to a key | Hash a name → key → route to responsible peer | Bootstrap nodes (optional) | Internet-scale | Refresh + eviction |

metor-fsw's constraints ("L2-routable, no central registry, small today, more nodes
later") point squarely at **row 1 for now, row 2 for later** — and notably **not** row 3,
whose payoff is internet-scale routing metor-fsw doesn't need.

---

## Survey by family

### 1. mDNS / DNS-SD (Zeroconf) — Bonjour, Avahi *(verified against RFCs)*

The reference answer for zero-config LAN discovery, and a fully **serverless** one. RFC
6762 (Multicast DNS): *"the ability to perform DNS-like operations on the local link in the
absence of any conventional Unicast DNS server."* Each host is authoritative for its own
records and answers multicast queries about itself. Name conflicts are resolved
peer-to-peer by a **probe-then-announce** protocol with lexicographic tie-breaking (RFC
6762 §8) — no coordinator ever adjudicates.

- **Wire mechanism:** UDP 5353 to fixed link-local multicast groups **224.0.0.251**
  (IPv4) / **FF02::FB** (IPv6). Queries and answers are ordinary DNS records.
- **Name → address (DNS-SD, RFC 6763):** a service instance is the three-part
  `<Instance>.<Service>.<Domain>`, e.g. `Orbital Sim 3._metor-fsw._tcp.local`.
  `<Instance>` is **arbitrary human-friendly Net-Unicode** (spaces, punctuation allowed) —
  exactly the "discover by name" the panel wants. A **PTR** query on
  `_metor-fsw._tcp.local` enumerates instances; each instance's **SRV** record gives
  host+port and its **TXT** record carries key/value metadata (protocol version, node role,
  namespace, git hash — a natural home for what `LinkInfo` carries today).
- **Central component:** none. DNS-SD adds *no* new DNS machinery (RFC 6763 §1) and can be
  populated by mDNS, manual config, or Dynamic DNS Update.
- **Liveness:** weak. Records have TTLs and goodbye packets (TTL=0), but there is no active
  failure detector — a crashed peer lingers until its TTL expires.
- **Security:** **none built in.** mDNS is unauthenticated and spoofable; DNSSEC is
  explicitly incompatible with its zero-config nature (IETF mDNS threat-model draft). Any
  host on the link can answer for any name. *(This is the load-bearing security caveat
  below.)*
- **Scaling / scope:** **strictly L2** — RFC 7558 states *"mDNS (by design) does not work
  across routers,"* and its scalability requirement (REQ11: thousands of nodes at roughly
  constant per-link traffic) is a target baseline mDNS **does not meet**. Fine for a lab
  subnet; not for a large flat fleet, and not across subnets without a reflector.

**Verdict:** the lowest-effort, best-fit answer for metor-fsw *today*. Human names, metadata
records, zero infrastructure, mature Rust crates.

### 2. SSDP / UPnP and WS-Discovery *(domain knowledge)*

The same "multicast query + self-announce" shape as mDNS but HTTP-flavored. **SSDP** (UPnP's
discovery layer) multicasts `M-SEARCH` / `NOTIFY` over HTTPU to **239.255.255.250:1900**;
responders return a URL to an XML device-description document. **WS-Discovery** (used by
ONVIF cameras, Windows network browsing) multicasts SOAP `Probe` messages to
**239.255.255.250:3702**.

- **Name → address:** SSDP identifies by device/service *type* (URN) and a UUID, not a
  friendly name; the friendly name lives one hop away in the fetched XML. WS-Discovery
  matches on types/scopes.
- **Central component:** none, but both are **noisier and heavier** than mDNS (XML/SOAP
  payloads, chatty refresh), and WS-Discovery is a notorious **DDoS reflection amplifier**
  (~300× on the open internet — a reason to bind it to trusted links only).
- **Verdict:** no advantage over mDNS for our case, and more overhead. Skip. Worth knowing
  only because Windows and IoT gear speak it.

### 3. Gossip membership — SWIM / Serf / Consul / Lifeguard *(verified)*

Where mDNS is "ask on demand," gossip is "maintain a live membership set." **SWIM**
(Scalable Weakly-consistent Infection-style Membership) is the dominant decentralized design
and, unlike mDNS, is a **real failure detector**. HashiCorp's `memberlist` (embedded in
Serf, Consul, Nomad) is the canonical production implementation.

- **Wire mechanism:** each node periodically **probes one random peer**; on timeout it asks
  *k* other peers to probe indirectly (routing around one-off packet loss). Membership,
  joins, and leaves spread **epidemically** by piggybacking on those probes, plus push/pull
  anti-entropy to reconcile full state. Suspected-dead nodes can **refute** via monotonic
  incarnation numbers before being declared dead.
- **Name → address:** a node's identity and arbitrary **tags/metadata** are part of its
  membership record and gossiped to everyone — so "names" (and namespaces, roles, versions)
  propagate for free. Consul's LAN gossip pool gives *"automatic discovery of servers and
  distributed failure detection"* with no central registry; the only bootstrap input is the
  address of **any one** existing member.
- **Central component:** **none.** Seeding is peer-to-peer.
- **Liveness:** **first-class and fast** — this is the whole point. **Lifeguard**
  extensions (HashiCorp, DSN 2018) have each node monitor its own "local health" and slow
  its own accusations when it's the one that's degraded, cutting false-positive failure
  reports to **<2% of baseline SWIM (>50×)** without slowing genuine detection — important
  under CPU starvation, which a busy flight-software scheduler can induce.
- **Security:** memberlist/Serf support a **symmetric gossip encryption key**; SWIM itself
  is otherwise unauthenticated. A pre-shared key gives you an authenticated membership mesh.
- **Scaling:** designed for **thousands of nodes**, crosses subnets (it's unicast), constant
  per-node load. This is the answer for the multi-node future.
- **Rust:** **`foca`** (`no_std`+alloc, transport- and identity-agnostic, does no I/O
  itself — you feed it datagrams and identities, so a human name or namespace *is* the
  identity) and **`al8n/memberlist`** (runtime-agnostic Rust port of HashiCorp memberlist
  with full Lifeguard).

**Verdict:** the right substrate when workloads span multiple nodes or need real liveness.
It does not need multicast at all — so it also solves the cross-subnet case mDNS can't.

### 4. DHTs and P2P overlays — Kademlia, libp2p, BitTorrent *(verified for libp2p)*

The internet-scale end of the spectrum. **Kademlia** assigns every peer and every content
key an ID in a shared space and routes by **XOR distance** to the numerically closest peer;
BitTorrent's Mainline DHT and IPFS use this to find peers for a hash with no central tracker.
**libp2p** is the interesting one for us because it **bundles all three mechanisms**: mDNS
for zero-config *local* discovery (service `_p2p._udp.local`), a Kademlia DHT for global
routing, and a **rendezvous** protocol for namespaced grouping — all keyed on cryptographic
**peer IDs** (hash of the public key), so identity is authenticated by construction.

- **Name → address:** names map to **hashes/peer IDs**, not human strings — you'd need a
  separate naming layer (IPNS-style) to get "Orbital Sim 3." Bootstrap nodes are optional
  aids, not required coordinators.
- **Verdict:** **more than metor-fsw needs.** The DHT's payoff is finding peers across the
  open internet without a coordinator — a problem we don't have on an L2 network. libp2p is
  worth keeping in mind *only* if metor-fsw later wants one batteries-included stack
  (authenticated identity + NAT traversal + local + global discovery); its **mDNS
  sub-component** alone is essentially option 1 with peer-ID identity bolted on.

### 5. IoT — Matter/Thread, CoAP, Bluetooth mesh *(domain knowledge)*

Instructive because it shows mDNS is the industry's converged answer even for constrained
devices. **Matter** (the smart-home standard over Thread + Wi-Fi) uses **mDNS/DNS-SD** for
*operational discovery* — commissioned devices advertise `_matter._tcp.local` with the
fabric/node ID in the instance name and TXT records; discovery is the same PTR→SRV/TXT dance
as everything else. **CoAP** (RFC 6690) does resource discovery via a well-known unicast
endpoint `GET /.well-known/core` returning a link-format list — a *resource* discovery
pattern (what does this node offer) rather than *peer* discovery. **Bluetooth mesh** uses
managed flooding with provisioning, a different world (no IP).

- **Verdict:** the takeaway is a **design pattern to copy**: put stable identity (node/fabric
  ID) *and* a friendly name in the DNS-SD instance name + TXT, exactly as Matter does. It
  validates the mDNS choice at industrial scale.

### 6. Mesh-VPN naming — Tailscale, ZeroTier, WireGuard *(domain knowledge)*

These give the nicest *name→peer* UX (`ping my-laptop` just works via **MagicDNS**), but
they earn it with a **central coordination server**: Tailscale's control plane brokers keys
and identity, ZeroTier has its network controllers, and both fall back to **relays** (DERP)
when direct connections fail. Plain **WireGuard** is peerless but has *no* discovery — you
configure endpoints by hand.

- **Verdict:** this model **violates the no-central-registry requirement** by design. The
  only decentralized-friendly middle ground is **self-hosted Headscale** (open-source
  Tailscale control server), but that's still a coordinator you run. Relevant only if
  metor-fsw ever needs to span the *internet* between sites — at which point a self-hosted
  coordinator + WireGuard mesh is a reasonable, if heavier, answer. Not for L2.

### 7. Robotics / real-time — ROS 2 / DDS *(domain knowledge, source-backed)*

Directly relevant because it's the closest analog to metor-fsw's domain, and it's a
**cautionary tale**. ROS 2's default DDS **Simple Discovery Protocol (SPDP/SEDP)** is exactly
the multicast self-announce pattern: every participant multicasts its presence, then
endpoints match by unicast. It's zero-config and decentralized — but it **famously does not
scale**: discovery traffic grows roughly O(n²) as every participant learns about every
other, and it leans on multicast that's **unreliable over Wi-Fi**. eProsima's fix is the
**Discovery Server** — an *optional* client-server hub that participants register with,
cutting the n² flood to n. Fast DDS also supports static/discovery-server hybrids.

- **Verdict:** two lessons. (a) Naive multicast discovery **rots at scale** — plan an
  escape hatch before you have 50 nodes. (b) The industry's escape hatch is *either* a hub
  (Discovery Server — a central component we're avoiding) *or* gossip (what Serf does). For
  a no-registry constraint, **gossip is the scaling answer, not a hub.**

### 8. LAN broadcast — consumer / gaming *(domain knowledge)*

The simplest possible thing: games and tools like Chromecast setup, LAN game lobbies, and
`dig`-free device finders just **UDP-broadcast a small "I'm here" packet** (or subnet
broadcast `255.255.255.255`) on a fixed port every few seconds, and clients listen. No
records, no schema — a name and address in a datagram.

- **Verdict:** viable as a **~50-line fallback** if we ever want zero dependencies, but it
  reinvents a worse mDNS (no conflict resolution, no metadata schema, no browser tooling).
  Prefer real DNS-SD unless dependency-freeness is paramount.

---

## Security of unauthenticated multicast discovery

This is the one caveat that matters regardless of choice. **Every multicast discovery
protocol above (mDNS, SSDP, WS-Discovery, DDS SPDP) is unauthenticated by default.** Any host
on the link can:

- **Impersonate** a service — answer for `Orbital Sim 3._metor-fsw._tcp.local` with its own
  address (mDNS spoofing / cache poisoning), so the panel connects to an attacker.
- **Harvest** the whole topology — discovery is a broadcast inventory of what's running.
- **Amplify** — SSDP/WS-Discovery are known reflection-DDoS vectors.

The correct posture, and the one every serious user of these protocols adopts: **treat
discovery as an untrusted hint, and authenticate the actual connection at the application
layer.** Discovery tells you *a* candidate address for a name; a mutual handshake proves it's
*the* peer. Concretely for metor-fsw:

- **Short term:** since discovery would run on a trusted lab L2, the immediate risk is low —
  but the fsw link is **plaintext TCP with no auth today**, so *anything* that lets a
  connection be redirected is worth a follow-up. At minimum, don't let discovery *widen* the
  trust boundary silently.
- **Medium term:** add an authenticated handshake to the link protocol (a pre-shared key or
  a TLS/peer-ID layer). Then discovery spoofing is downgraded from "connect to attacker" to
  "fail to connect," which is a denial-of-service, not a breach.
- **Gossip:** if/when we adopt SWIM, use memberlist/foca's **symmetric encryption key** so
  the membership mesh itself is authenticated — a node without the key can't join or forge
  membership.

---

## Recommendation for metor-fsw

**Adopt a phased hybrid: mDNS/DNS-SD now, SWIM gossip later, seeded from mDNS. Never a
central registry.** This matches the "single-host today, multi-node tomorrow" arc exactly and
each phase is independently shippable.

### Phase 1 — mDNS/DNS-SD (do this now)

Zero-config, human names, no infrastructure, and the panel seam already exists.

1. **Advertise from the fsw link.** When `LinkState::bind` succeeds (`telemetry/link.rs:135`),
   register a DNS-SD service `_metor-fsw._tcp.local` with:
   - **instance name** = the human name (new — see below),
   - **port** = the bound port (already tracked via `local_addr`, so port-0 works),
   - **TXT records** = what `LinkInfo` carries today (protocol version, features/role,
     namespace, maybe git hash). This lets the panel filter/label *before* connecting.
2. **Add a name to identity.** `LinkInfo` (`wkt/msgs.rs:1048`) has no name field. Add a
   `name`/`namespace` field, bumped via `LINK_PROTOCOL_VERSION`, so the instance is the
   source of truth for its own name (the mDNS instance name is derived from it, and the panel
   can still show it post-connect). Source the name from target wiring / a `--name` CLI flag
   next to `--serve`.
3. **Browse from the panel.** A small discovery thread runs an mDNS browser for
   `_metor-fsw._tcp.local` and, on each found/lost instance, calls
   `RegistryHandle::upsert` / `remove` with a `ConnectionTarget::tcp(name, addr)`
   (`connections/mod.rs` — this is the documented intended path). **No changes to the picker,
   persistence, or connection lifecycle.**
4. **Rust crate: `mdns-sd`** (`keepsimple1/mdns-sd`). Pure-Rust (no Bonjour/Avahi native
   dep, unlike `zeroconf`/`astro-dnssd`), does both responder and querier, and — importantly
   for us — **spawns its own daemon thread with a channel API**, so it drops into either the
   panel's gpui thread or the fsw's `stellarator` runtime without a runtime dependency. Pin
   it behind a `Discovery` trait so it's swappable.

*Cost: ~a few days. Delivers "open panel → see every metor-fsw on the LAN by name."*

### Phase 2 — SWIM gossip (when nodes multiply or cross subnets)

Add when any of these become true: multiple fsw nodes need to know about **each other**
(not just the panel), you need **real liveness** (mDNS TTL expiry is too slow/coarse), or
workloads **cross subnets** (mDNS can't).

1. Run a **`foca`** (or `al8n/memberlist`) gossip instance per fsw node; the **member
   identity is the human name/namespace** (foca is identity-agnostic, so this is natural).
2. **Seed the gossip mesh from mDNS** — mDNS finds the first peer on the link, gossip takes
   over for liveness, metadata, and cross-subnet reach. This is the standard hybrid and it
   means no bootstrap config.
3. Turn on the **symmetric encryption key** so membership is authenticated.
4. The panel subscribes to membership changes and `upsert`s the *same* `ConnectionTarget`s
   through the *same* `RegistryHandle` — Phase 2 is invisible to the panel UI.

### Explicitly not recommended

- **DHT/libp2p** as the primary mechanism — its value (internet-scale, no-coordinator
  routing) is a problem metor-fsw doesn't have on L2; it's real complexity for no local
  payoff. (Revisit *only* if metor-fsw needs authenticated identity + NAT traversal across
  the internet, where libp2p's all-in-one stack earns its keep.)
- **Mesh-VPN (Tailscale/ZeroTier)** — violates no-central-registry. Self-hosted Headscale +
  WireGuard is the fallback *only* for cross-site internet spanning.
- **A DDS-style Discovery Server or any hub** — reintroduces the central component we're
  avoiding; gossip is the decentralized scaling answer instead.

### Why this shape

mDNS and SWIM are **complementary, not competing**: mDNS is on-demand *name resolution* with
zero setup but no liveness and L2-only reach; SWIM is a live *membership + failure detector*
that crosses subnets but needs a seed. Layering them — mDNS seeds gossip — gives zero-config
*and* scale *and* liveness, and **at no point is there a central registry**. Both have mature,
I/O-agnostic Rust crates that fit metor-fsw's runtimes, and both feed the panel through the
discovery seam that already exists.

---

## Rust crate cheat-sheet

| Crate | Role | Notes |
|---|---|---|
| **`mdns-sd`** (keepsimple1) | Phase-1 mDNS/DNS-SD | Pure Rust, responder+querier, daemon-thread + channel API, no async-runtime dep. **First choice.** |
| `zeroconf` | mDNS via native | Wraps Bonjour/Avahi — native dep, platform-variable. Avoid unless you need OS integration. |
| `astro-dnssd` | mDNS via native | Same native-dep caveat. |
| `hickory-dns` (ex-`trust-dns`) | DNS toolkit | Lower-level; use if you want to build DNS-SD by hand. Overkill vs `mdns-sd`. |
| **`foca`** (caio) | Phase-2 SWIM | `no_std`+alloc, transport/identity-agnostic, does no I/O — you own the socket and identity. **First choice for gossip.** |
| `al8n/memberlist` | Phase-2 SWIM | Fuller HashiCorp port with Lifeguard + encryption batteries included; heavier. |
| `rust-libp2p` / `libp2p-mdns` | Only if going all-in on P2P | mDNS + Kademlia + peer IDs bundled; more than L2 needs. |

---

## Sources

Primary sources, verified with unanimous (3-0) adversarial agreement during research:

- **RFC 6762** (Multicast DNS) — serverless operation, `.local` scope, conflict resolution.
- **RFC 6763** (DNS-SD) — `<Instance>.<Service>.<Domain>` naming, PTR/SRV/TXT, transport-agnostic.
- **RFC 7558** (Requirements for Scalable DNS-SD) — mDNS is router-bounded and non-scaling by design.
- **SWIM / Lifeguard** — hashicorp/memberlist, al8n/memberlist, Consul gossip architecture docs, Lifeguard paper (arXiv 1707.00788, DSN 2018; >50× false-positive reduction).
- **libp2p** — discovery/routing overview, mDNS spec (`_p2p._udp.local`), rendezvous.
- **Rust crates** — `mdns-sd`, `foca` (docs.rs + repos), `zeroconf`.

Additional families (SSDP/UPnP, WS-Discovery, Matter/Thread, CoAP, Bluetooth mesh,
Tailscale/ZeroTier, ROS 2/DDS, LAN broadcast) are drawn from established domain knowledge;
sources were located during research (e.g. eProsima Fast DDS Discovery Server docs, Matter
operational-discovery docs, Tailscale reference) but fell outside the verification budget's
25-claim cap, so treat their specifics as well-established background rather than
independently re-verified here.

### Open questions worth a follow-up
- Which exact crate carries Phase 1 — confirm `mdns-sd` interop with a future `foca` seed path.
- Whether the fsw link should get an authenticated handshake *before* discovery ships (so spoofing is only a DoS).
- If cross-site (internet) spanning is ever real: self-hosted Headscale + WireGuard vs. libp2p — a separate decision from L2 discovery.
