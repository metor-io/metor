# Phase 2 plan — stubgen: generated typed pack modules

> **Status: PLANNED.** Implements Phase 2 of `docs/design-python-config.md`
> (§5 generated packs, §10 ABI v6 doc strings, §11 phasing). Phases 0-1 are
> landed (through `b7244ce1`): manifest sidecars sit next to every built
> `.so` (WP7), per-entry defaults cross the manifest (WP6), and the
> `metor_config` recorder evaluates `.py` missions end-to-end.

Goal: `metor-fsw stubgen` reads pack manifests and generates a real,
`py.typed`, checked-in Python module per artifact — typed constructors from
the params schema (with real kwarg defaults split from the WP6 blob), typed
port attributes with per-frame marker classes, and the artifact declaration
embedded so using an entry implicitly records the artifact. pyright/IDE
checking of missions becomes possible; the runtime behavior is identical to
Phase 1's untyped path.

Scope discipline: no bundle/telemetry work (Phase 3), no KDL retirement
(Phase 4), no stubs for *application* static-registry systems (explicitly
deferred by the design; the untyped escape hatches remain).

## Phase 1 internals this builds on (from the M1-M3 report)

- Entry callables return `Spec(ty, artifact, params)`; `m.add`/`m.slot`
  depend only on that shape plus `._param_source()`. Generated classes must
  produce the same `Spec` (subclassing is fine).
- `Mission.to_ir()` is the single source of truth for IR emission; the
  emitted envelope carries `metor_config_version` + `ir_version`.
- The recorder is embedded via `EMBEDDED_PACKAGE` in `src/wiring/py.rs`
  (a `&[(&str, &str)]` file list; materialization creates parent dirs).
  Generated packs are NOT embedded — they are checked into the mission
  directory (`packs/` next to `mission.py`; the script dir is on `sys.path`,
  so `from packs.adcs import Plant` just works).
- `param_value_tree`/`ParamNode` render any `ParamSource` to a JSON value.

## Milestones

Four milestones, one commit each, in order.

### M1 — `metor-fsw stubgen` (Rust)

1. **Inputs.** A `[tool.metor.artifacts]` table in the mission directory's
   `pyproject.toml` maps artifact id → `{ crate, lib }` (same fields the
   `artifact` node carries). `stubgen` takes the mission dir (default `.`),
   builds the listed crates via the existing `build_artifacts` driver (or
   `--no-build` to require them prebuilt), and reads each artifact's
   **manifest sidecar** (`<cdylib>.manifest`, WP7) — falling back to the
   describe worker only when the sidecar is absent. No dlopen into the CLI
   process.
2. **Manifest hash.** Hash = SHA-256 (or the strongest hash already in the
   dependency tree — check `Cargo.lock` before adding a dep; note the
   design pins "hash of the manifest postcard bytes", i.e. the sidecar body)
   rendered `sha256:<hex>`. Embedded in the generated module's `ARTIFACT`
   constant.
3. **Defaults splitting.** When an entry carries `params_default` bytes,
   decode them against `params_schema` via `postcard_dyn::from_slice_dyn`
   and render per-field Python default values. Fields absent from the blob
   (no defaults declared) are required kwargs. `Option<T>` fields default to
   `None` regardless.
4. **Codegen.** Emit `packs/<artifact_id>.py` (+ an empty `packs/__init__.py`
   and `packs/py.typed`) per the design §5 shape — the exact generated text
   is M2's contract; M1 owns the data flow and the writer. Deterministic
   output (stable ordering) so `--check` is a byte diff. Header comment:
   generated-by line, regenerate + verify commands, no timestamps.
5. **`--check` mode**: regenerate to a temp dir, byte-compare, non-zero exit
   with a per-file diff summary on mismatch.
6. **Staleness enforcement at resolve.** `ir::Artifact` gains
   `#[serde(default)] pub manifest_hash: Option<String>`; the recorder
   passes it through from `ARTIFACT` (M2). After artifact load, `resolve`
   compares the recorded hash against the live manifest bytes and fails with
   a new `LoadError::StaleStubs { artifact }` naming the regen command.
   `None` (KDL front-end, hand-written `pack()` handles) skips the check.
7. Tests: stubgen over the dl fixture pack (build, generate, snapshot the
   generated module text); `--check` clean/dirty; defaults splitting against
   a fixture entry with a defaults blob; `StaleStubs` negative test
   (tampered hash); sidecar-absent fallback.

### M2 — the generated module shape + typed `metor_config` core

1. **`metor_config` grows the typed core**: `Frame` base, `System` base
   (subclass of the Phase 1 spec machinery), generic `InPort[F]`/
   `OutPort[F]` (`typing.Generic`), `connect(src: OutPort[F], dst:
   InPort[F], *, delayed: bool = False)` / `route(...)` signatures on
   `Mission`, an `Artifact` dataclass (id, crate, lib, manifest_hash), and
   `py.typed` for `metor_config` itself. Runtime behavior unchanged — the
   generics are erased annotations; `.port(name)` stays the untyped hatch.
   Using a generated entry auto-registers its `ARTIFACT` on the mission
   (dedupe by id; conflicting definitions for the same id is an eval-time
   error). Explicit `m.artifact(...)` remains for stub-less packs.
2. **Generated module contents** (design §5.2 mapping table is binding):
   - `ARTIFACT = Artifact(id=..., crate=..., lib=..., manifest_hash=...)`.
   - One `class <FrameName>(Frame)` marker per distinct `frame_id` among the
     artifact's Table ports (name from the frame metadata; collision-suffix
     if needed). Postcard/msg ports use msg marker types (hand-written
     well-known ones live in `metor_config`; artifact-local ones generate).
   - One class per entry: `class Plant(System)` with a typed
     keyword-only `__init__` from the params schema — bool/int/float/str;
     `Option<T>` → `T | None = None`; `Vec<T>` → `Sequence[T]`; `[T; N]` →
     exact `tuple[...]`; nested struct → generated frozen kw_only dataclass;
     fieldless enum → `Literal[...]`; enum-with-data → union of generated
     variant dataclasses; per-field defaults from M1.3. Runtime `__init__`
     validates nothing beyond Phase 1's JSON-representability — pyright is
     the checker, resolve is the enforcer.
   - Class-level port attribute *annotations* (`sensors: OutPort[Sensors]`)
     with a comment noting delivery/fan-in/telemetered; at runtime port
     access goes through the existing handle `__getattr__` (the annotations
     are for the checker — instances returned by `m.add` are handles, so
     generated classes exist purely as constructors + annotation carriers;
     make `m.add` return type hint the spec's class for checker purposes,
     via a `TYPE_CHECKING` overload or a cast — implementer's discretion,
     report the mechanism).
   - Occupant entries (sequence packs) generate module-level callables with
     typed kwargs returning occupant specs — same calling convention as
     Phase 1's `seqs.commissioning(...)`.
3. Python tests: generated-module import + record + emit round-trip on a
   hand-frozen fixture module (checked in as test data, so the Python suite
   doesn't need cargo); typed-core unit tests; a smoke assertion that the
   fixture module text matches what M1's Rust snapshot test generates (one
   fixture, two consumers — same pattern as the Phase 1 golden test).
4. If `pyright` (or `uv run pyright`) is available on PATH, add an optional
   test/CI hook that type-checks the adcs mission; skip cleanly when absent.
   Do not make the build depend on it.

### M3 — adcs integration

1. `examples/adcs-fsw2/pyproject.toml` with `[tool.metor.artifacts]` for
   `adcs` + `seqs`; run stubgen; check in `packs/adcs.py`, `packs/seqs.py`,
   `packs/py.typed`.
2. Rewrite `examples/adcs-fsw2/mission.py` to
   `from packs.adcs import Plant, Nav, Ctrl` / `from packs.seqs import
   commissioning, safe_mode`, dropping the explicit `m.artifact(...)` lines
   (implicit registration from `ARTIFACT`). Alarms/links stay on the
   `metor_config` builtins.
3. The equivalence test stays green unchanged (same IR modulo the new
   `manifest_hash`, which the comparison must tolerate — KDL side is
   `None`). `metor-fsw run mission.py --build` re-verified. `stubgen
   --check` wired into the crate test suite for the example (regenerate =
   byte-identical).
4. Staleness end-to-end check: mutate a params struct in the fixture (or
   tamper the checked-in hash), observe `StaleStubs` with the regen hint.

### M4 — ABI v6: doc strings in the manifest

Per the design's recorded leaning (§10, decisions log open item 1):

1. `SystemDescriptorMsg`/`PortDescMsg`/params-schema-adjacent manifest types
   gain optional doc fields (`docs: Option<String>` per entry, per port, and
   per params field — the params docs ride a parallel
   `Vec<(String, String)>` keyed by field path rather than forking
   postcard-schema's types). Bump `FSW_ABI_VERSION` to 6. All in-repo
   artifacts rebuild; that is the accepted cost of an ABI bump (the check in
   `dl.rs` makes skew a clean load error).
2. Extraction: the `#[system]` macro and the pack fn/task builders capture
   `#[doc]` attributes from the entry type and its `Params` fields (the
   derive machinery that walks doc attributes for frames is the model);
   plumb through `Pack` into the described manifest. Missing docs are
   `None` — never required.
3. stubgen renders them: module docstring for the entry class, `"""..."""`
   under each generated dataclass, and per-param docs as trailing comments
   or a docstring parameter list (implementer's discretion; pick what
   pyright/IDE hover actually surfaces and report the choice).
4. Tests: describe round-trip carries docs; stubgen snapshot updated; the
   adcs `PlantParams` docs (units, "~3e-12 at 400 km") visibly appear in the
   regenerated `packs/adcs.py`.
5. This milestone touches the ABI — if any part of the doc-field design
   turns out to require forking postcard-schema itself, STOP and report;
   that trade-off needs a human decision.

## Known constraints and traps

- Never run bare `cargo test -p adcs-fsw2` (untracked scratch files);
  use the tracked `--test` targets (bundle, closed_loop, sequences, alarms,
  eclipse, momentum, equivalence).
- Pre-existing `cargo fmt --check` drift — no blanket formatting; also
  beware rustfmt recursing into child modules (bit Phase 1).
- The generated-code header must NOT contain timestamps or absolute paths —
  `--check` is a byte diff and bundles must be reproducible.
- serde/IR contract discipline as in Phase 1: Rust representation is the
  source of truth.
- Crate house style in Rust; PEP 8, stdlib-only, 3.10-compatible Python
  (`from __future__ import annotations`; no 3.11+ syntax at runtime —
  `Literal`/generics usage must import from `typing`, not rely on newer
  builtins).
- Two pre-existing clippy warnings (telemetry/mod.rs, proc/tests.rs); add
  none.

## Acceptance for the phase

- `cargo test -p metor-fsw-2` + tracked adcs targets (incl. equivalence)
  green; clippy clean of new warnings; Python `unittest` suite green.
- `metor-fsw stubgen` in `examples/adcs-fsw2` is idempotent
  (`--check` passes on the checked-in packs).
- `mission.py` imports generated modules and `metor-fsw run mission.py
  --build` works.
- Tampered hash → `StaleStubs` naming the regen command.
- Regenerated `packs/adcs.py` shows the `PlantParams` doc strings (M4) and
  real kwarg defaults where declared (`CtrlParams` from WP6).
