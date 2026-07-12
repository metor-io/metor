# Phase 1 plan — `metor_config` recorder + host eval path

> **Status: PLANNED.** Implements Phase 1 of `docs/design-python-config.md`
> (§4 config API, §7 evaluation flow, §11 phasing). Phase 0 is landed
> (WP1-WP7, through merge `8bf384d2`): the IR is versioned, carries
> `SourceRef`/scope metadata, and `ParamSource::Value` works uniformly for
> static and dl systems. This phase adds the Python front-end that emits it.

Goal: `metor-fsw build|run|package mission.py` works end-to-end — a
subprocess CPython evaluates the mission file against a small recorder
library, writes serialized `Wiring` JSON, and the host consumes it exactly
where it consumes parsed KDL today. Acceptance: an adcs `mission.py` whose
resolved graph is equivalent to `mission.kdl`'s.

Scope discipline: no stubgen (Phase 2), no bundle-format change or
`WiringManifest` telemetry (Phase 3), no KDL retirement (Phase 4). The doc
sweep deferred from Phase 0 stays deferred.

## Milestones

Three milestones, one commit each (plus fixups), in order. Rust and Python
halves are deliberately batched — do not split further.

### M1 — IR egress/ingress on the Rust side

1. **Un-gate the IR.** `wiring::model` is behind the `kdl` feature
   (Phase 0 preamble #6) though it contains no `kdl::` types. Introduce a
   `wiring-model` (name at implementer's discretion) always-on-in-`kdl`
   feature or move the module out of the gate so `Wiring` + serde +
   `serde_json` are available without the KDL parser. `ParamSource::Kdl` is
   just a `String` — it stays in the enum regardless of features.
2. **JSON round-trip contract test.** A Rust test that serializes a maximal
   `Wiring` (systems with all three `ParamSource` variants, slot with allows
   and initial, frame + delayed + msg edges, scopes, `SourceRef`s) to JSON
   and back, and snapshots the JSON. This snapshot is the reference the
   Python emitter is written against — serde's representation (externally
   tagged enums, field names, `Option` handling) is the source of truth; the
   Python side conforms to it, never the reverse.
3. **CLI eval path.** `cmd_build`/`cmd_package`/`load_run_wiring` accept a
   `.py` mission (extension-dispatched; `.kdl` unchanged):
   - Resolve the interpreter: `$METOR_PYTHON` → `$VIRTUAL_ENV/bin/python` →
     `python3` on PATH. Require ≥ 3.10 (`sys.version_info` probe); clean
     error otherwise.
   - Materialize the embedded `metor_config` package (see M2) to a per-run
     temp dir; spawn `python mission.py` with that dir prepended to
     `PYTHONPATH` and `METOR_IR_OUT` pointing at a temp file.
     `$METOR_CONFIG_PY` overrides the materialized copy with a live checkout
     for development.
   - Non-zero exit: pass stderr through verbatim (the native traceback IS
     the tier-1 error surface) and fail the command.
   - Zero exit: read the IR file, `serde_json::from_str::<Wiring>`, check
     `ir_version` (existing `resolve()` check also fires — the CLI check
     exists only to produce a friendlier message naming the two versions),
     then proceed into the existing `resolve()` → build/package/run flow
     unchanged. Tier-2 errors already anchor via the `src` fields the
     recorder fills.
   - A `metor_config_version` sanity check: the emitted IR carries the
     library version (see M2 emission); the CLI warns on mismatch with its
     embedded copy's version (warn, not error — `$METOR_CONFIG_PY` makes
     skew legitimate in development).
4. Tests: extension dispatch; a fixture `mission.py` (trivial two-system
   static mission) evaluated by the real subprocess in an integration test
   (skip gracefully with a clear message if no `python3` ≥ 3.10 is on PATH —
   CI has one); stderr passthrough on a deliberately-raising fixture;
   `$METOR_PYTHON` honored.

### M2 — the `metor_config` package

Location: `libs/metor-fsw-2/python/metor_config/` (plain package, stdlib
only, no pip dependencies, works on 3.10+). Embedded into the `metor-fsw`
binary via `include_str!` of each source file, materialized at eval time
(M1.3). `__version__` in `metor_config/__init__.py`, stamped into the
emitted IR.

Public surface (design doc §4 — follow it exactly; deviations get reported,
not improvised):

- `Mission(cycle_rate=..., sim_dt=None, ...)` — also accepts the
  no-KDL-surface knobs `CoordinatorSpec` exposes (check `model.rs` for the
  real field set; expose what the spec carries, nothing more). Exactly one
  `Mission` may exist at emission time: the module tracks instances; zero or
  two+ is a hard eval-time error.
- `m.artifact(id, crate=..., lib=...)` → artifact handle; attribute access
  (`adcs.Plant`) and item access (`adcs["Plant"]`) both yield entry
  callables; calling one records a spec `(entry, artifact, params-dict)`.
  Params values must be JSON-representable (numbers, strings, bools, None,
  lists/tuples, dicts); reject anything else at record time with the
  offending key named.
- `m.add(name, spec, process=False)` → `SystemHandle`; `__getattr__` on the
  handle yields `PortRef(instance, port)` (no eval-time port validation —
  resolve owns that); `.port(name)` as the explicit spelling.
- `metor_config.static_system(type, **params)` (name per design: exported
  builtin helpers `Alarms`, `TcpUplink`, `TcpDownlink` are thin wrappers
  over it) for registry systems. `TcpUplink(addr=..., msgs=[...])` /
  `TcpDownlink(addr=...)` must emit params matching the Rust
  `UplinkParams`/`DownlinkParams` serde shapes (read the Rust structs;
  don't guess).
- Alarm helpers: `Alarm`, `Target`, `band` frozen dataclasses per design §4,
  rejecting unknown kwargs at eval time, emitting the exact params tree
  `AlarmSystem`'s `Params` deserializes (read the Rust struct).
- `m.connect(src_ref, dst_ref, delayed=False)`; `m.route(src, dst, msg=...)`
  where src/dst are handles (or `m.coordinator`, the reserved handle) —
  `route` has no `delayed` kwarg by construction.
- `m.slot(name, inputs=[...], outputs=[...], allow=[...], initial=...,
  initial_state=...)` — occupant specs are the same call convention as
  system specs (`seqs.commissioning(**params)`); `initial_state` maps to the
  IR's `SlotInitState` representation (check serde).
- `m.scope(name)` context manager: prefixes `add`/`slot` instance names with
  `name + "."`, nests, and records entries in the IR scope table with
  correct `parent` indices; systems/slots created inside carry the scope
  index.
- `SourceRef` capture: every recorded node walks `sys._getframe` outward
  past `metor_config` frames to the first user frame and stores
  `{file, line, col=1}` (Python frames have no column pre-3.11
  consistently; col 1 is honest). File paths relative to CWD when possible.
- Emission: at interpreter exit (`atexit`) or an explicit
  `metor_config.emit()` — write the JSON `Wiring` (ir_version = the value
  from M1's snapshot, `metor_config_version` in a top-level field only if
  the Rust struct grows one; otherwise stamp it in an `x-metor-config`
  comment-equivalent — check what `serde_json::from_str::<Wiring>` tolerates;
  if unknown top-level fields error, add `#[serde(default)]`
  `metor_config_version: Option<String>` to `Wiring` in M1) to
  `$METOR_IR_OUT` (stdout if unset, for debuggability). Duplicate instance
  names, unbound nothing (there are no placeholders), unknown-occupant
  initial, and the one-Mission rule all raise ordinary exceptions before
  emission.
- Tests: stdlib `unittest` suite under `libs/metor-fsw-2/python/tests/`
  covering the recorder surface, scope nesting/parent indices, SourceRef
  plausibility, error cases, and a golden-emission test asserting the exact
  JSON structure of a small mission against a checked-in snapshot that the
  M1 Rust round-trip test also consumes (one fixture, two consumers — the
  cross-language contract test). Wire the suite into CI-visible reach with a
  `cargo test`-adjacent runner only if trivial; otherwise document
  `python -m unittest` invocation in the package README.

### M3 — adcs `mission.py` + equivalence acceptance

1. `examples/adcs-fsw2/mission.py` re-expressing `mission.kdl` in full using
   the M2 surface (the design doc §4.1 sketch is the target text, modulo the
   real params).
2. **Equivalence test** (`examples/adcs-fsw2/tests/` or crate integration
   test): evaluate `mission.py` via the M1 subprocess path, parse
   `mission.kdl` via the existing front-end, and assert the two `Wiring`s
   are equivalent:
   - Strip/ignore `src` anchors and the scope table (KDL has none).
   - Params compare as canonical bytes: dl params through the existing
     encode machinery (WP4's byte-parity helpers), static params by
     decoding both sides into `serde_json::Value` through their respective
     paths and comparing.
   - Everything else (`coordinator`, artifacts, system order, edges incl.
     `delayed`, slot spec, initial) compares structurally.
3. Run the tracked adcs integration suite once with the mission source
   swapped to `mission.py` (a test or a manually-verified run — whichever is
   cheaper; report which). `mission.kdl` stays the committed default for the
   example until Phase 4.
4. Update `examples/adcs-fsw2/README` (or the example's doc header) with the
   two-front-end status.

## Known constraints and traps

- **serde JSON representation is the contract.** Externally-tagged enums
  (`{"Value": {...}}`), exact field names, `Option` as `null`/absent — the
  Python emitter must match the M1 snapshot byte-structure, and any
  divergence is a Python-side bug. Never "fix" the Rust representation to
  match Python convenience without flagging it.
- **`resolve()` pre-checks** (`IrVersionMismatch`, `check_scope_refs`) run
  before the systems pass — the Python-emitted scope indices must be valid
  or the mission fails there with `BadScopeRef`.
- The `commands`/coordinator special case: `m.coordinator` maps to the
  reserved `"coordinator"` instance name in msg edges, nothing more
  (design §4.1).
- `tests/{swap_repro,abort_repro}.rs` in the adcs example are untracked user
  scratch files that don't compile — never run bare `cargo test -p
  adcs-fsw2`; use the tracked `--test` targets.
- Pre-existing `cargo fmt --check` drift — no blanket formatting.
- Style: crate house style in Rust (design rationale in module docs, short
  function docs, no change-narrating comments); PEP 8 + concise docstrings
  in Python, same no-obvious-comments discipline.

## Acceptance for the phase

- `cargo test -p metor-fsw-2` + tracked adcs targets green; clippy adds no
  warnings; the Python `unittest` suite passes on 3.10 and the newest local
  CPython.
- `metor-fsw build examples/adcs-fsw2/mission.py` succeeds on a machine with
  only stock `python3`.
- The equivalence test is green and committed.
- A deliberately-broken mission (bad port name) evaluated end-to-end shows:
  native traceback for a Python-level error, and an anchored
  `mission.py:NN` `LoadError` for a resolve-level error.
