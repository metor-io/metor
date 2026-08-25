# Phase 1 — mDNS/DNS-SD discovery: implementation plan

*2026-07-21 · companion to `service-discovery-report.md`*

## Goal

An fsw link **advertises itself over mDNS/DNS-SD** under a human name; metor-panel
**browses** for those advertisements and drops discovered instances into its connection
picker automatically. No central registry, no new transport, no changes to the picker/
persistence/connection lifecycle — discovery only *produces* `ConnectionTarget`s through the
seam that already exists.

Scope is L2/local-link and a **trusted network** (see Security below). Cross-subnet reach and
liveness are Phase 2 (SWIM gossip) and explicitly out of scope here.

## What's already in place (no work needed)

- **Panel discovery seam.** `ConnectionsStore::init` hands back a `RegistryHandle`
  (`connections/mod.rs:53`) with `upsert(ConnectionTarget)` / `remove(TargetId)`
  (`:57-65`), drained on the gpui thread every 250ms (`:322`). Discovered targets land in
  the picker's `discovered` section (`:249`) with no UI plumbing.
- **The wiring point for a discovery source.** `PanelApp::connection_source(impl
  FnOnce(RegistryHandle))` (`app.rs:930`), fired at boot with a live handle (`app.rs:1120`).
- **The target constructor.** `ConnectionTarget::tcp(name, addr)` (`target.rs:222`) — builds
  a `DiscoverBackend` and, crucially, ids it `tcp:<addr>` (`:224`). So a discovered target
  and a manually-typed one for the same address **coalesce** (share layout + recents), and a
  goodbye maps to `TargetId(format!("tcp:{addr}"))`.
- **The bound port is known.** `LinkState::local_addr()` (`link.rs:159`) returns the real
  address even when the config said port 0 — the port to advertise.

So the fsw needs to learn a **name** and run an **advertiser**; the panel needs a **browser**
that calls `upsert`/`remove`. That's the whole of Phase 1.

---

## Design decisions

1. **Crate: `mdns-sd`** (keepsimple1) on both sides. Pure Rust (no Bonjour/Avahi native
   dep), does responder *and* querier, and runs its own daemon thread exposing a
   `flume::Receiver<ServiceEvent>` — so it composes with `stellarator` (fsw) and the gpui
   thread (panel) without a runtime dependency. Pin the exact version against docs.rs when
   adding; the API sketch below matches the current `ServiceDaemon`/`ServiceInfo`/`browse`
   shape.
2. **Service type: `_metor-fsw._tcp.local.`** DNS-SD instance name = the human node name;
   TXT records carry protocol version + role + optional namespace.
3. **Name is configured on the fsw**, defaulting to the OS hostname when unset — zero-config
   still produces a usable name. The advertiser is the single source of truth for the mDNS
   instance name.
4. **`LinkInfo` gains the name too (WP4), but it's separable.** Discovery names come from
   mDNS, so the core loop works without touching the wire protocol. Adding `name` to
   `LinkInfo` is what makes a *manually*-connected fsw (raw address, no mDNS) show its real
   name, and keeps one source of truth. Do it, but it can land after WP1–WP3/WP5 are green.
   **No `LINK_PROTOCOL_VERSION` bump** — `LinkInfo` is internal, no external client is pinned
   to a wire version, and both encoder and decoder ship together, so the field is just
   appended (postcard append-at-end) and the version left at 1.
5. **Don't advertise loopback.** Binding `127.0.0.1:2240` (the dev default) is not on a
   multicast link; advertising it is noise and misleads. Advertise only when bound to a
   non-loopback address (or `0.0.0.0`, where `mdns-sd` enumerates real interfaces). Skip +
   log otherwise. This means the pure-localhost dev path is unaffected and undiscovered — by
   design.

---

## Work packages

### WP0 — Shared discovery constants *(`metor-proto-wkt`)*

One home both sides import, so the service type and TXT keys can't drift.

- Add (next to `LinkInfo`, `wkt/src/msgs.rs`, or a new `wkt/src/discovery.rs`):
  ```rust
  /// DNS-SD service type for an fsw telemetry link.
  pub const FSW_SERVICE_TYPE: &str = "_metor-fsw._tcp.local.";
  /// TXT record keys.
  pub const TXT_PROTOCOL_VERSION: &str = "pv"; // = LINK_PROTOCOL_VERSION
  pub const TXT_NAMESPACE: &str = "ns";        // optional, mirrors the telemetry namespace
  pub const TXT_ROLE: &str = "role";           // "fsw"
  ```
- No behavior; pure constants. **Acceptance:** both crates compile against them.

### WP1 — fsw node-name config plumbing *(`metor-fsw-2`)*

Expose the name **declaratively in the wiring IDL** — the Python `metor_config` surface and
the Rust builder — flowing through `LinkParams` → `LinkState`. **No CLI flag** (`--serve`
stays address-only). Distinct from the existing `StateSpec.name` (`"link"`, a diagnostic key
— do **not** overload it).

- `LinkParams` (`telemetry/link.rs:60`): add
  ```rust
  /// Human node name advertised over mDNS. `None` → OS hostname at advertise.
  #[serde(default)]
  pub name: Option<String>,
  ```
  This *is* the IDL param — `LinkParams` is `Serialize`/`Schema`, so the field is the wire
  contract the Python surface emits into.
- `StateSpec::tcp_server` (`ir.rs:305`): keep `tcp_server(name, addr)` (name → `None`) and add
  `tcp_server_named(name, addr, node_name: Option<&str>)` that folds `name` into the params
  json only when set (so an unnamed server emits identical params — golden-IR-stable).
- **Python IDL** (`python/metor-config/metor_config/__init__.py:351`): `TcpServer(addr, name=None)`
  → `static_system("TcpServer", **_drop_none({"addr": addr, "name": name}))`. Usage becomes
  `m.state("link", TcpServer(addr="0.0.0.0:2240", name="Orbital Sim 3"))`. `_drop_none` keeps
  the field absent when unset, so `test_golden.py` / `test_recorder.py` stay green.
- `WiringBuilder` (`builder.rs:251`): keep `serve(addr)` (name `None` → hostname) and add
  `serve_named(name, addr)`.
- CLI `--serve` (`cli.rs:685`): the existing "state already declared" branch **replaces**
  params wholesale (`:693`) — change it to overwrite only `addr`, preserving a target-set
  `name`.
- `LinkState`: store `name: Option<String>` (add `with_name(self, Option<String>)`; keep
  `bind(addr)` unchanged — 4 test callers stay untouched). Add `node_name(&self) -> String`
  resolving the config name or the OS hostname (`gethostname`, already in-tree). Registry
  factory (`registry.rs:319`): `|p| LinkState::bind(p.addr).map(|s| s.with_name(p.name))`.
- **Acceptance:** `TcpServer(addr=…, name="Orbital Sim 3")` round-trips into `LinkParams.name`;
  unset falls back to the hostname; `LinkState::node_name()` returns the effective name.

### WP2 — fsw mDNS advertiser *(`metor-fsw-2`)*

Register the service when the link is up; unregister on shutdown.

- Add `mdns-sd` to `metor-fsw-2/Cargo.toml`.
- New module `telemetry/discovery.rs`: `fn advertise(name: &str, addr: SocketAddr, txt: &[(…)]) -> Option<ServiceDaemon>`:
  - Bail (log) if `addr.ip().is_loopback()`.
  - `ServiceDaemon::new()`; build `ServiceInfo` for `FSW_SERVICE_TYPE`, instance = `name`,
    host = hostname, port = `addr.port()`, TXT = `{pv, role, ns?}`, letting `mdns-sd`
    enumerate interface IPs (or pass the bound non-loopback IP).
  - `daemon.register(info)`; return the daemon (dropping it unregisters + sends goodbye).
- Wire into `LinkState`: hold `advertiser: Option<ServiceDaemon>`. Register in
  `SharedLifecycle::start` (`link.rs:345`) — `local_addr` and `node_name` are both known by
  then — and drop it in `shutdown` (`link.rs:353`) alongside the accept guard.
- **Acceptance:** with a non-loopback bind, `dns-sd -B _metor-fsw._tcp` (macOS) /
  `avahi-browse -r _metor-fsw._tcp` (Linux) shows the instance with correct name/port/TXT;
  killing the fsw removes it.

### WP3 — panel mDNS browser *(`metor-panel`)*

A discovery source that turns `ServiceEvent`s into registry ops.

- Add `mdns-sd` to `metor-panel/Cargo.toml`.
- New module `connections/discovery.rs`: `pub fn mdns_source() -> impl FnOnce(RegistryHandle)`:
  - Spawn a plain OS thread (the browser is sync + channel-driven; keep it off gpui and off
    stellar). `ServiceDaemon::new()`, `daemon.browse(FSW_SERVICE_TYPE)` → `flume::Receiver`.
  - Loop on events:
    - `ServiceResolved(info)`: pick one `SocketAddr` (first non-loopback IPv4; deterministic
      so the `tcp:<addr>` id is stable), read the instance name (strip the `._metor-fsw…`
      suffix) for the display name, `handle.upsert(ConnectionTarget::tcp(name, addr))`. Keep
      a `fullname → SocketAddr` map for removals.
    - `ServiceRemoved(fullname)`: look up the addr, `handle.remove(TargetId(format!("tcp:{addr}")))`.
  - Ignore `SearchStarted`/`SearchStopped`.
- **Acceptance:** a browser unit/integration test asserts a resolved event yields an upsert
  with the right name and `tcp:<addr>` id; a removed event yields the matching remove.

### WP4 *(recommended, separable)* — `LinkInfo` carries the name

So identity is consistent whether reached by mDNS or by raw address.

- `LinkInfo` (`wkt/msgs.rs:1048`): **append** `pub name: String,` (postcard is
  non-self-describing — append at the end, never reorder). **Leave `LINK_PROTOCOL_VERSION` at
  1** — internal msg, both ends ship together, no external bump (per direction).
- Encoder: `set_announces` (`link.rs:194`) fills `name` from `LinkState::node_name()`.
- Decoder + tests: `identify` already `postcard::from_bytes`es it (`fsw.rs:62`); the literal
  `LinkInfo { … }` constructions in `fsw.rs` tests (`:215`, `:279`) and any other must add
  `name`. Grep `LinkInfo {` workspace-wide.
- Panel display: surface `info.name` on the connected row / titlebar so a raw-address fsw
  shows its real name post-connect.
- All producers/consumers are in-workspace and version-gated — a coordinated bump is safe;
  no external-compat shim (matches the "no opt-out shims for legacy id spaces" convention).
- **Acceptance:** round-trip test asserts the name survives `identify`; `--name` shows in the
  panel after a manual `fsw://` connect with no mDNS involved.

### WP5 — wire the panel source

- Where `PanelApp` is constructed (the panel binary / lib entry near `main.rs`), add
  `.connection_source(connections::discovery::mdns_source())`. One line; the seam does the
  rest.
- **Acceptance:** launching the panel with an fsw advertising on the LAN shows it in the
  picker's discovered section by name, click-to-connect works, and quitting the fsw removes
  the row within a poll interval.

### WP6 — verification

- Unit: WP3 browser event→op mapping; WP4 `LinkInfo` round-trip.
- Manual end-to-end: fsw on host A (`--serve 0.0.0.0:2240 --name "Sat A"`), panel on host B
  on the same L2 → discovered by name → connect → telemetry flows → kill fsw → row drops.
- Negative: loopback bind advertises nothing (dev path unchanged).

---

## Suggested order & sizing

`WP0 → WP1 → WP2` (fsw advertises) ‖ `WP3` (panel browses, testable against `dns-sd`
register or the WP2 fsw) `→ WP5` (wire it) `→ WP6`. **WP4 last** — it's the only wire-format
change and the rest doesn't depend on it. Rough sizing: WP0 trivial; WP1 ~½ day; WP2 ~½ day;
WP3 ~1 day (event mapping + dedup); WP4 ~½ day; WP5 trivial; WP6 ~½ day. ~3 days total.

Commit at WP boundaries (advertiser, browser, wiring, LinkInfo) rather than one mega-diff.

## Risks & notes

- **Same-host dev.** Panel + fsw on one machine over loopback won't discover each other (by
  the loopback rule) — dev keeps using the hardcoded sandbox (`main.rs:35`). Discovery is for
  the multi-host case. Call this out so it's not read as a bug.
- **Address selection.** An fsw with several interfaces advertises several IPs; the browser
  must pick **deterministically** or the `tcp:<addr>` id (hence layout/recents) flaps. First
  non-loopback IPv4, documented.
- **mDNS reliability.** Multicast can be lossy on Wi-Fi; `mdns-sd` re-queries, so a missed
  announce self-heals within seconds. Acceptable for Phase 1; Phase 2 gossip is the real
  liveness answer.
- **Security (the one real caveat).** mDNS is unauthenticated and spoofable, and the fsw link
  is plaintext TCP with no auth today. On a trusted lab L2 the immediate risk is low, but
  discovery shouldn't be read as a trust boundary. **Do not** advertise on untrusted networks
  until the link gets an authenticated handshake — track that as the Phase-1.5 follow-up noted
  in the report. Keep advertising opt-in-per-bind (loopback-excluded) so it can't surprise a
  deployment onto a hostile LAN.

## Touch list

| File | Change |
|---|---|
| `metor-proto/wkt/src/msgs.rs` | WP0 constants; WP4 `LinkInfo.name` (no version bump) |
| `metor-fsw-2/src/telemetry/link.rs` | WP1 `LinkParams.name` + `LinkState` name/`with_name`/`node_name`; WP2 advertiser register/unregister; WP4 encode |
| `metor-fsw-2/src/telemetry/discovery.rs` *(new)* | WP2 advertiser |
| `metor-fsw-2/src/ir.rs` | WP1 `tcp_server_named` params |
| `metor-fsw-2/src/wiring/builder.rs` | WP1 `serve`/`serve_named` |
| `metor-fsw-2/src/wiring/registry.rs` | WP1 factory threads `p.name` |
| `metor-fsw-2/src/cli.rs` | WP1 `--serve` preserves a target-set name |
| `metor-fsw-2/python/metor-config/metor_config/__init__.py` | WP1 `TcpServer(addr, name=None)` |
| `metor-fsw-2/Cargo.toml` | WP2 `mdns-sd`; WP1/WP2 `gethostname` direct dep |
| `db/src/remote/fsw.rs` | WP4 decode + test updates |
| `metor-panel/src/connections/discovery.rs` *(new)* | WP3 browser |
| `metor-panel/src/connections/mod.rs` | WP3 `pub mod discovery;` re-export |
| `metor-panel/src/app.rs` (or panel entry) | WP5 `.connection_source(…)` |
| `metor-panel/Cargo.toml` | WP3 `mdns-sd` |
