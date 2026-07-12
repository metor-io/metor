# Design: Python as the mission configuration language

Status: accepted design, pre-implementation. Supersedes the KDL wiring grammar
(`design-kdl-serde.md` stays as historical record). Companion plan docs will be
written per phase (§11).

## 1. Summary

Replace the KDL wiring front-end with mission configs written in real Python,
evaluated once at build/package time by the user's own CPython in a subprocess.
Evaluation records a graph and emits the existing `Wiring` IR, serialized and
versioned. The host consumes IR exactly where it consumes parsed KDL today —
nothing downstream of `Wiring` changes. The flight/run path consumes frozen IR
from a bundle and needs no Python, no KDL parser, and no interpreter of any
kind.

Three pillars:

1. **A small recorder library (`metor_config`)** — an explicit builder API in
   the Drake `DiagramBuilder` shape: construct systems, then draw edges,
   finalize once. Port references are first-class values, which is what makes
   user-defined functional blocks compose.
2. **Generated typed pack modules (`metor stubgen`)** — real `py.typed` Python
   generated from pack manifests, giving pyright/IDE checking of params, ports,
   and cross-system frame compatibility at keystroke time.
3. **The `Wiring` IR promoted to a versioned public contract** — with source
   anchors, scope hierarchy, and value-tree params. It becomes the bundle's
   manifest and is emitted by the running FSW as telemetry for visualization.

## 2. Goals and non-goals

Goals, in priority order (from the project kickoff):

1. Allow more complex configuration by using a real programming language.
2. Be more ergonomic and easier to understand than the KDL version.
3. Be grokable to non software engineers (aerospace/controls engineers who
   know some Python/MATLAB).
4. Reduce complexity in the coordinator build step.

Non-goals:

- Embedding an interpreter in the host binary. Rejected after survey:
  RustPython is actively developed but self-declares non-production status,
  lost its flagship embedder (GreptimeDB, Jan 2025), and would make us own
  every CPython behavioral divergence while users lose their venv, numpy, and
  debugger. PyO3-linked CPython was rejected for deployment weight (libpython
  in a flight-software CLI) and in-process crash exposure. Starlark
  (starlark-rust) is the documented fallback if hermeticity-by-construction
  ever becomes a hard requirement.
- Sandboxing config evaluation. Config runs at build time on the ground with
  the same trust model as `build.rs`. Determinism is enforced operationally:
  the bundle records the IR hash and CI re-evaluates and diffs.
- Changing the semantic core. The 9-pass `CoordinatorBuilder::build()`
  pipeline and `resolve()` are untouched; the Python layer's whole job is to
  arrive at `resolve()` with a better-validated, better-provenanced `Wiring`.

## 3. Precedent grounding

Design lessons taken as established (from the Basilisk/Drake/Amaranth/Dyad
survey):

- **Two-phase, always.** Eager Python records a graph; one finalize step
  validates with the whole graph in hand. Every surviving wiring DSL converged
  here (Drake `Build()`, Amaranth elaborate, Nengo `Simulator(model)`).
- **Objects-then-edges.** Handles exist before edges, so feedback cycles are
  two ordinary connect calls. Kwargs-at-construction cannot express a cycle
  without placeholder machinery (the pattern our own first sketch ran into).
- **Explicit staleness.** Basilisk's implicit one-cycle-stale reads make
  execution order invisible semantics — the anti-pattern. Our `delayed`
  edges plus `StaleFrameEdge`/`FeedbackCycle` validation are the honest model
  and survive unchanged.
- **The config layer never owns memory the runtime dereferences** (Basilisk
  issue #676: Python GC freeing wiring objects C++ still points at). Python
  holds names and port references; the Rust graph owns everything.
- **Every dynamic-wiring system that survives grows a stub generator**
  (Drake `.pyi` 2022, cocotb Copra 2025). We generate from day one — the
  introspection registry already exists in Rust with real types.
- **Connect-as-data enables diagrams** (Dyad): because edges are recorded
  declaratively, a text↔graph round trip is possible; a fully imperative
  config would foreclose it. This is what §9 (visualization) builds on.

## 4. The config API

### 4.1 Shape

Explicit builder, no global state. A mission file:

```python
from metor import Mission, Alarms, Alarm, Target, band, TcpUplink, TcpDownlink
from packs.adcs import Plant, Nav, Ctrl
from packs.seqs import commissioning, safe_mode

m = Mission(cycle_rate=120.0, sim_dt=1 / 120)

plant = m.add("plant", Plant(
    init_angle=0.5, init_rate=0.15, meas_sigma=0.002, seed=42, disarmed=False,
    rho=3e-12, cd=2.2, area_aero=0.03,
    cp_offset_b=(0.02, 0.0, 0.0),
    m_res_b=(0.002, 0.002, 0.002),
    area_srp=0.03, cr=1.5, mtq_max_dipole=0.2,
    init_wheel_h=0.0, init_orbit_phase=0.0,
), process=True)
nav  = m.add("nav",  Nav(meas_sigma=0.02))
ctrl = m.add("ctrl", Ctrl(q_weight=5.0, r_weight=8.0,
                          k_desat=0.0005, k_detumble=0.00005))

alarms = m.add("alarms", Alarms(alarms=[
    Alarm(id="ADCS_RATE_HIGH", name="Body Rate High",
          description="Measured body-Y rate exceeds the detumbled envelope",
          target=Target("plant.sensors.gyro_b", element=1),
          warning=band(above=0.05, below=-0.05),
          critical=band(above=0.15, below=-0.15),
          debounce=2, hysteresis=0.005),
    ...
]))

mode = m.slot("mode",
    inputs=["attitude_estimate", "gps"], outputs=["mode_cmd"],
    allow=[commissioning(rate_detumble_enter=1.0, ...), safe_mode()],
    initial="commissioning", initial_state="running")

m.connect(plant.sensors, nav.sensors)
m.connect(plant.gps,     nav.gps)
m.connect(nav.attitude_estimate, ctrl.attitude_estimate)
m.connect(nav.attitude_estimate, mode.attitude_estimate)
...
m.connect(mode.mode_cmd,   ctrl.mode_cmd,    delayed=True)
m.connect(ctrl.torque_cmd, plant.torque_cmd, delayed=True)
m.connect(ctrl.mtq_cmd,    plant.mtq_cmd,    delayed=True)

uplink   = m.add("uplink", TcpUplink(addr="127.0.0.1:2240",
                 msgs=["SequenceCommand", "AlarmAck", "ReloadSequences"]))
downlink = m.add("downlink", TcpDownlink(addr="127.0.0.1:2240"))

m.route(uplink, mode, msg="SequenceCommand")
m.route(m.coordinator, mode, msg="SequenceCommand")
m.route(uplink, alarms, msg="AlarmAck")
m.route(uplink, m.coordinator, msg="ReloadSequences")
```

Vocabulary and semantics:

- `Plant(...)` records a spec (entry, artifact, params value tree); it does
  not construct a system. `m.add(name, spec, process=False)` registers it and
  returns a handle. Authoring concerns (params) live on the spec; placement
  concerns (`process=`) live on `add`.
- `plant.sensors` is a typed port reference (`OutPort[Sensors]`) — a name
  pair plus provenance, nothing more. Escape hatch for programmatic
  generation: `plant.port("sensors")`, untyped, kept out of examples.
- `m.connect(src_out, dst_in, delayed=False)` records a frame edge.
  `delayed=True` is the one-cycle-stale marker, 1:1 with KDL's
  `delayed=#true`. It subsumes both KDL connect forms (same-name and
  renamed ports) with one spelling.
- `m.route(src, dst, msg=...)` records a message edge. A separate verb on
  purpose: message routing is log-delivery pub/sub, and `route` having no
  `delayed=` kwarg makes the illegal delayed-log edge unrepresentable.
- `m.coordinator` is the reserved handle for the coordinator instance
  (command plane). The type-keyed command-plane special case (review finding
  A2) is deliberately not fixed here; the Python surface must not front-run a
  Rust-side model change.
- `m.add` order is registration order is step order — the same load-bearing
  document order as KDL, kept visible. Finalize does not reorder; the
  `StaleFrameEdge` error instead prescribes the fix ("mark `delayed=True` or
  move this `add` above that one").
- Alarms stay **data**: frozen-dataclass helpers (`Alarm`, `Target`, `band`)
  over the same value trees, rejecting unknown kwargs at eval time with exact
  lines. Cross-field validation (band containment) stays on the Rust
  deserialize path — one source of truth.
- Mission discovery: exactly one `Mission` instance must exist after the
  module runs; zero or two is a hard error. No magic names.
- `Mission(...)` also exposes the config knobs KDL never surfaced
  (`reader_slack`, `worker_exe`, `shm_dir`).

### 4.2 Functional blocks

Blocks are plain Python functions — zero framework machinery — because port
references are values. The blessed pattern returns an interface object rather
than taking ports as parameters, so blocks can close feedback loops over each
other (a block that takes an `OutPort` argument requires its producer to
exist first; block-level cycles then need placeholders again):

```python
@dataclass
class ThrusterBlock:
    cmd: InPort[ThrustCmd]     # exposed, unconnected — caller wires it
    tlm: OutPort[ThrustTlm]

def thruster(m: Mission, name: str) -> ThrusterBlock:
    with m.scope(name):
        driver = m.add("driver", ValveDriver(rate_limit=0.2))
        valve  = m.add("valve",  Valve(response_ms=8.0))
        m.connect(driver.drive, valve.drive)
    return ThrusterBlock(cmd=driver.cmd, tlm=valve.tlm)

thr = thruster(m, "thr_a")               # systems: thr_a.driver, thr_a.valve
m.connect(ctrl.thrust_cmd_a, thr.cmd)
m.connect(thr.tlm, fdir.thr_a_tlm)
```

- A block's interface looks exactly like a system's interface (named ports),
  so composition nests: blocks instantiate blocks and re-export ports, and
  callers cannot tell the difference. This is Drake's Diagram-is-a-System
  property.
- Taking a port as a function argument remains fine sugar for acyclic
  single-consumer helpers; it desugars to a connect inside the block. Connect
  is the primitive; args are never the primitive.
- `m.scope(name)` prefixes instance names (`thr_a.driver`) and records the
  scope path in the IR (§6). Scopes give collision-free reuse, a hierarchical
  telemetry tree, and collapsible blocks in the graph view (§9).

## 5. Generated packs and static analysis

### 5.1 stubgen pipeline

```
cargo build -p adcs-systems
        │
        ▼  describe-worker subprocess (existing proc/host.rs machinery —
        │  stubgen never dlopens into its own process)
        ▼
metor stubgen  →  packs/adcs.py   (generated, py.typed, checked in)
```

- Which crates to stubgen comes from a `[tool.metor.artifacts]` table in
  `pyproject.toml` (build metadata, not wiring). `metor-fsw build` regenerates
  stubs after building artifacts; `metor stubgen --check` is the CI gate.
- **Real `.py`, not bare `.pyi`.** The generated classes are simultaneously
  the pyright surface and the runtime recorder — one artifact, two consumers,
  no drift. Generated code is small and diff-reviewable: a schema change is a
  visible diff in the mission PR.
- The artifact declaration lives inside the generated module (`ARTIFACT =
  Artifact(id=..., crate=..., lib=..., manifest_hash=...)`); using `Plant`
  implicitly records the artifact node. Artifact-id typos become structurally
  impossible, and `from packs.adcs import Plant` is a real import of a real
  file — no import hooks, no `pack()` string indexing in the blessed path.
  An untyped `pack(crate=...)` handle remains for artifacts without stubs.

### 5.2 Type mapping (postcard-schema → Python)

| schema shape | Python type | note |
|---|---|---|
| bool / ints / floats / String | `bool` / `int` / `float` / `str` | int width range-checked at record time |
| `Option<T>` | `T \| None = None` | |
| `Vec<T>` | `Sequence[T]` | |
| `[T; N]` | exact tuple, e.g. `tuple[float, float, float]` | arity typos are pyright errors |
| nested struct | generated frozen kw_only dataclass | |
| fieldless enum | `Literal[...]` | |
| enum with data | union of generated variant dataclasses | first real check on this shape — conform passes it through today |

Ports: attribute access only. Stubgen emits one marker class per distinct
frame schema (keyed by `frame_id`), and the core library declares
`connect(src: OutPort[F], dst: InPort[F], *, delayed: bool = False)` — so a
cross-system frame mismatch is a pyright error before the host ever runs.
Msg names become `Literal`s from the wkt `MsgTable`.

### 5.3 Staleness

Three layers: the generated module embeds a hash of the manifest postcard
bytes (not the `.so` — pure code changes must not churn stubs); the host
compares recorded vs live manifest hashes at resolve time and fails with
`LoadError::StaleStubs` naming the regen command; CI runs `stubgen --check`.

### 5.4 Division of labor

pyright owns node-local shape (param names/types/arity, port names, frame
compatibility, enum strings). The host owns graph semantics (cycles,
staleness ordering, unconnected inputs, fan-in, slot occupant compatibility)
via the unchanged `resolve()`/`build()` passes. Values and cross-field rules
stay on the Rust decode path.

Static registry systems: builtins (`Alarms`, `TcpUplink`, `TcpDownlink`) ship
hand-written typed helpers inside `metor_config`. Application static systems
get stubs in a second phase via a `Schema` bound on `Registry::register` plus
a host-binary manifest-dump hook; until then they use an untyped
`RegistrySystem("MyType", **params)` escape hatch.

## 6. The Wiring IR as a versioned contract

`Wiring` already derives Serialize/Deserialize and is the proven seam between
front-ends and `resolve()`. Promote it deliberately:

- **`ir_version` field**, checked on every consumption path (host eval,
  bundle load, telemetry consumers).
- **`ParamSource::Value(serde_json::Value)`** — the params representation for
  all Python-authored config. Dissolves in one move: the KDL re-parse
  pipeline (`de.rs` KDL deserializer + `parse.rs`, deletable post-migration),
  the `StaticPostcardParams` seam (a self-describing `Value` decodes into
  static systems' `Params` via `serde_json::from_value`, so one params
  representation serves static and dl uniformly), and the built-in hack where
  `WiringBuilder::telemetry()` fabricates KDL text. `conform_to_schema` →
  `postcard_dyn` survives as the single validation for dl params. Python does
  **not** emit postcard bytes directly: the JSON hop against the live schema
  is the safety interlock against stale-stub silent corruption.
- **`src: Option<SourceRef>` per node** (system, slot, allow, edge): captured
  at record time by walking `sys._getframe` past library frames to the first
  user frame; optionally the top few user frames, Drake-style, so config
  built through helpers stays traceable. Replaces KDL's miette spans
  one-for-one.
- **Scope hierarchy**: each system/slot records its scope path (from
  `m.scope`), and the IR carries a scope table. Instance names remain the
  dotted full path (collision-checked flat, as today); the table is what
  lets consumers reconstruct the block tree without parsing names.
- **Per-artifact manifest hashes** for the staleness check (§5.3).
- Slot descriptors get modeled as named occupant-contract + framework-tail
  concepts rather than the positional prefix+tail encoding (review finding
  A3), and the resolve_slot-validates-then-add_slot-asserts double validation
  (C3a) collapses to one pass while we are in there.
- The `resolve_dl`/`resolve_slot` duplication unifies into one
  `resolve_occupant(pack, entry, params: &Value)` — the differing
  reserved-key sets and skip-arg counts were pure KDL artifacts.
- Wire format: JSON (debuggable, diffable in bundles, self-describing —
  params must be `Value`s anyway). Postcard buys nothing at config scale.

CLI overrides (`--sim-dt`, ...) keep their current pattern: mutate the
deserialized `Wiring` before `resolve()`.

## 7. Evaluation flow and error reporting

`metor-fsw build mission.py`:

1. Resolve a Python: `$METOR_PYTHON` → active venv → `uv run` if `mission.py`
   carries PEP 723 inline metadata → system `python3`. Require ≥ 3.10.
2. Subprocess evaluates the file; `metor_config` records; on success writes
   versioned IR JSON to `$METOR_IR_OUT` (or fd 3).
3. Host checks `ir_version` + `metor_config` version + manifest hashes, then
   runs the existing `resolve()` → `build()`.

Errors, three tiers, each printing a clickable `mission.py:NN`:

1. **Record time (Python)**: duplicate instance names, unknown occupants,
   range violations, unbound-everything — native CPython tracebacks, `pdb`
   and IDE debuggers work because evaluation is just running a script.
2. **Resolve/build time (host)**: `LoadError`/`WireError` join instance and
   port names back to IR nodes and print their `src` anchors.
   `FeedbackCycle` prints one anchored line per loop member — a guided tour
   of the loop. The E5d branches that print raw indices get fixed to instance
   names as part of this work.
3. **Skew**: stale stubs and version mismatches fail before any dlopen, each
   naming the one command that fixes them.

Record-time discipline mirrors "refuse loudly": no silent coercions in
`metor_config`, warnings print with anchors.

## 8. Bundles

Target state (replaces the verbatim-KDL bundle in `wiring/bundle.rs`):

```
mission.bundle/            (directory, or a single-file tar: `.metor`)
  wiring.json              frozen versioned IR (src anchors, scopes, hashes)
  meta.json                abi_version, ir_version, target triple, profile,
                           built_at, IR content hash, metor_config version
  adcs_systems.so
  adcs_sequences.so
  mission.py               optional provenance copy, never consumed
```

- `metor-fsw package mission.py` = evaluate → IR → copy built `.so`s →
  freeze. `load_bundle` checks ABI + IR version + **target triple** (new —
  today an arch mismatch surfaces as a dlopen failure instead of a clean
  load error), verifies manifest hashes, hands `Wiring` to `resolve()`.
- The run path needs no Python and no KDL: strictly more hermetic than
  today's parse-mission.kdl-on-target.
- The IR content hash is the determinism backstop: CI re-evaluates the config
  and diffs — accidental nondeterminism becomes a visible diff, not a
  mystery.
- Cross-compilation wrinkle: manifest hashes come from *describing* an
  artifact, and `fsw_pack_describe` means running the `.so`, which a dev
  machine cannot do for a foreign-arch build. Resolution: the **build driver**
  writes a manifest sidecar (`<cdylib>.manifest`, raw postcard
  `PackManifestMsg` bytes so sidecar-hash ≡ describe-hash) next to the target
  `.so`, sourced by describing a host-runnable build of the same crate. The
  manifest cannot be a proc-macro product — descriptors are runtime values
  (vtables, `MAX_SIZE` consts, `DeclSink` walks). Consumers (stubgen,
  cross-arch resolve) read the sidecar and never run the artifact. Note
  arch-independence of manifests is verified, not assumed: cross builds
  compare host and target sidecars when both exist.

## 9. Visualization: the IR as a graph artifact

Decided: the FSW emits the **full IR** as a well-known `WiringManifest`
message at startup and on reload — the same pattern `SequenceRegistry` /
`AlarmDef` already use (unmatched wkt messages recorded as telemetry are the
pub/sub plane). Consequences:

- Panel discovers live topology by connecting to a running FSW — no bundle
  access needed — and because the manifest is recorded telemetry, the graph
  is historical: scrub back to see the topology (and slot occupancy, via the
  already-flowing slot state messages) as it was during an incident.
- The scope table (§6) is what makes the graph readable: panel renders a
  block as one box and expands it on demand. Src anchors make nodes
  deep-linkable to `mission.py` lines.
- Live edge activity needs no new FSW surface: edges carry port identities
  that join against ordinary telemetry components.
- Panel-side this is one new tile kind (node-graph with auto-layout;
  positions user-adjustable, persisted panel-side like other tile state).
  Out of scope for this crate; the contract is the versioned IR + the
  `WiringManifest` wkt message.

## 10. Companion Rust-side changes

Adopted alongside (not blocked on) the front-end swap:

- **Per-field defaults on the dl path**: `Pack::system_type_with_defaults`
  (the missing sibling of `task_with_defaults`; no ABI bump — the
  whole-struct `params_default` blob already crosses). The `#[system]` macro
  emits the blob automatically when `Params: Default`. Stubgen splits it into
  per-field kwarg defaults, ending the spell-out-every-param era.
- **ABI v6: doc strings in the manifest** (`docs: Option<String>` per entry
  and per param field, extracted by the existing derive machinery that
  already walks doc attributes for frames). Without it, generated stubs lose
  the units/prose that make params usable; a sidecar file would rot.
- **Uplink `msgs=` derived from routes** (allowlist = union of msgs routed
  out of the uplink), with explicit `msgs=` as override. Deletes exact
  duplication in every mission file; flagged in review because it couples
  ground-command surface to wiring lines.
- KDL retirement: `parse.rs`/`de.rs` behind a feature flag for one release,
  with a mechanical `metor migrate` KDL→Python converter (same IR), then
  deleted. Two grammars indefinitely is how the `lib=` two-meanings bug class
  happens.

## 11. Phasing

Each phase gets its own plan doc before implementation.

- **Phase 0 — IR promotion (Rust only).** `ir_version`,
  `ParamSource::Value`, `SourceRef` + scope table, unified
  `resolve_occupant`, slot descriptor de-positionalization, defaults
  (`system_type_with_defaults` + macro blob), compile-time manifest sidecar.
  KDL front-end keeps working throughout; this phase is invisible to users.
- **Phase 1 — `metor_config` + host eval path.** The recorder library
  (Mission/handles/ports/scopes/blocks, alarm helpers, builtin system
  helpers), subprocess evaluation in `metor-fsw build`, tiered error
  reporting. adcs example gains a `mission.py` that byte-identically
  reproduces the KDL mission's resolved graph (the migration acceptance
  test).
- **Phase 2 — stubgen.** Manifest→module codegen, `pyproject.toml` artifact
  table, `--check` CI gate, staleness enforcement at resolve. ABI v6 doc
  strings land here if approved.
- **Phase 3 — bundles + telemetry.** `wiring.json` bundles with target-triple
  check and single-file form, `WiringManifest` wkt emission. Panel graph tile
  proceeds independently against the frozen contract.
- **Phase 4 — migration + retirement.** `metor migrate`, KDL behind a feature
  flag, docs sweep, then deletion.

## 12. Decisions log

Settled in design review (2026-07-12):

| decision | choice |
|---|---|
| Language | Python (dialect via library, not interpreter fork) |
| Execution | subprocess CPython at build time; no interpreter on target |
| API shape | explicit `Mission` builder, objects-then-edges |
| Cycles | `delayed=True` kwarg on `connect`; no placeholders |
| Blocks | plain functions returning port-interface dataclasses; `m.scope` |
| Imports | generated real modules (`from packs.adcs import Plant`); no import hooks |
| Ports | attribute access, typed; `.port(name)` escape hatch |
| Params | value trees (`ParamSource::Value`), JSON hop, conform against live schema |
| Alarms | stay data (frozen dataclass helpers) |
| Stubs | real `py.typed` package, checked in, manifest-hash staleness |
| IR | versioned JSON contract with src anchors and scope hierarchy |
| Scope/block hierarchy in IR | yes |
| `WiringManifest` telemetry payload | full IR |
| Bundle | IR + meta + `.so`s (+ optional provenance `mission.py`); target-triple check |
| Hermeticity | no sandbox; IR-hash re-eval diff in CI |

Open (each has a leaning, none blocks Phase 0):

1. ABI v6 doc strings — leaning yes (Phase 2).
2. Uplink `msgs=` derivation from routes — leaning yes, needs a security-eyes
   pass.
3. `delayed=True` kwarg vs a dedicated verb — kwarg; revisit if delayed edges
   get missed in review diffs.
4. Command-plane special case (`m.coordinator`) — keep reserved handle;
   revisit with the Rust-side model.
5. Single-file bundle format details (tar vs zip, extension) — Phase 3.
