# Phase 0 plan — IR promotion (Rust only)

> **Status: PLANNED.** Implements Phase 0 of `docs/design-python-config.md`
> (§6 IR promotion, §10 companion changes, §11 phasing). Rust-only; the KDL
> front-end keeps working byte-for-byte throughout, and the phase is invisible
> to mission authors. Each work package below is independently landable.

Goal: promote `Wiring` from an internal parse product to the versioned,
provenance-carrying contract Phase 1's Python recorder will emit, and clean up
the seams (params representation, occupant resolution, slot descriptor
bookkeeping, defaults, manifest access) that would otherwise force the Python
layer to reproduce KDL-era accidents.

## Where the design doc and the current code disagree

Checked against the tree at `sphw/metor-fsw-2` (HEAD `c6ef1f14`). Reported
here rather than silently deviated from:

1. **§8 "the pack macro emits the manifest as a compile-time sidecar" is not
   implementable as stated.** The manifest is a *runtime* value: `export_pack!`
   (`src/abi/mod.rs:825`) only wires ABI symbols; the bytes come from
   `run_pack_describe` executing the crate's `pack()` — descriptors are built
   by running `CyclicSystem::descriptor()` / `DeclSink` walks over closures,
   with vtables and `MAX_SIZE` consts that no proc macro or const-eval can see
   syntactically. WP7 therefore generates the sidecar from the **build
   driver** (describe a host-runnable build of the artifact), not from the
   macro. This is the honest version of "describe doesn't require running the
   artifact": the *consumers* (stubgen, cross-arch resolve) don't run it; the
   build step still does, once, on the host.
2. **§6 "ir_version checked on every consumption path" — no path deserializes
   a serialized `Wiring` today.** Bundles (`src/wiring/bundle.rs`) carry
   verbatim `mission.kdl` text and re-parse it; telemetry emission of the IR
   doesn't exist yet. Phase 0 can check at `resolve()` and record/check the
   version in `meta.kdl`; the wiring.json-bundle and `WiringManifest`
   telemetry checks are Phase 3 by the design's own phasing.
3. **§6 `resolve_occupant(pack, entry, params: &Value)` under-counts the
   duplication.** There are *four* near-duplicate open→select→encode sites,
   not two: `resolve_dl` (`src/wiring/mod.rs:700`), `resolve_proc` (`:753`),
   the dl loop in `resolve_slot` (`:954`), and `describe_occupants` (`:1073`)
   — across two entry-source kinds (opened `DlPack` vs describe-worker
   `PackEntryMeta`). And params are not yet `&Value`: while KDL lives, the
   unified function must still take a `&ParamSource` and the
   reserved-keys/skip-args parameters (`SYSTEM_RESERVED`/1 vs
   `ALLOW_RESERVED`/0). Those parameters die with KDL in Phase 4, not here.
4. **`serde_json::from_value` into `S::Params` does not reject unknown keys**,
   but the KDL deserializer (`de.rs`) rejects them generically — a parity gap
   the design doc doesn't mention. WP4 closes it with `serde_ignored` (new
   small dependency) on the static `Value` decode path.
5. **The slot model is three concepts, not two.** The design says "named
   occupant-contract + framework-tail", but the registered lists actually
   interleave: (a) the occupant's *user* ports, (b) the **mount-appended
   occupant tail** (`SlotControlIn` cancel input, `SequenceStatus` output —
   part of the ABI prefix the occupant binds, `coordinator/mod.rs:1293-1328`),
   and (c) the **runner tail** (`commands` fan-in, self-tap, `SlotStatus`,
   `"sequences"` — never crosses to the occupant). The named model in WP5 must
   keep all three distinct, and the flattened registered descriptor must stay
   byte-identical (edge resolution and registry keys depend on it).
6. **`wiring` (including `model.rs`) is feature-gated behind `kdl`**, and
   `serde_json`/`postcard-dyn` are optional deps under that feature
   (`Cargo.toml:36-38`, `src/lib.rs:128`). `ParamSource::Value` makes
   `serde_json` load-bearing for the IR itself. Fine for Phase 0, but Phase 1
   needs the IR outside the `kdl` gate; do not entangle new code with
   KDL-only items unnecessarily (keep `model.rs` free of `kdl::` types, as it
   already is).
7. **Confirmed as the doc describes**: `WiringBuilder::telemetry()`/`uplink()`
   fabricate KDL text (`model.rs:151-159`); `StaticPostcardParams` is the
   rejection at `wiring/mod.rs:573-581`; `add_slot` re-asserts what
   `resolve_slot` validated (`coordinator/mod.rs:1238-1286` vs
   `wiring/mod.rs:918-999`); `Pack::system_type` sets `params_default: None`
   unconditionally (`pack.rs:267`); `PackSystemMsg.params_default` already
   crosses the ABI so no version bump is needed (`abi/mod.rs:517-526`).
8. **The `#[system]` macro cannot see whether `Params: Default`.** WP6's
   automatic blob needs a provided `BuildSystem` hook plus autoref
   specialization in the macro expansion (where the params type is spelled
   concretely). Hand-written `BuildSystem` impls and generic systems fall back
   to `None` and use the explicit `system_type_with_defaults`.
9. **`parse()` doesn't know the document's file name** (`parse(kdl: &str)`),
   so `SourceRef.file` needs an origin plumbed from `cli.rs`
   (WP2 adds `parse_with_origin`).

## WP1 — `ir_version` on `Wiring`

Small, independent; land first so every later WP's serde tests exercise it.

- `src/wiring/model.rs`: `pub const IR_VERSION: u32 = 1;` and a required (no
  `#[serde(default)]` — a missing version must be an error, and nothing
  serialized persists today) `pub ir_version: u32` field on `Wiring`.
- `src/wiring/parse.rs` `parse()` and `src/wiring/builder.rs`
  `WiringBuilder::build()` stamp `IR_VERSION`.
- `src/wiring/mod.rs` `resolve()`: first check
  `wiring.ir_version == IR_VERSION`, else a new spanless
  `LoadError::IrVersionMismatch { found, expected }` (`error.rs`).
- `src/wiring/bundle.rs`: write `ir_version` into `meta.kdl`; `load_bundle`
  reads it when present and rejects a mismatch with a new
  `BundleError::IrMismatch` (absent field = pre-versioning bundle, also
  rejected — bundles are rebuildable).
- Tests: `src/wiring/tests.rs` — JSON round-trip of a full `Wiring`
  (systems + slot + edges) preserving `ir_version`; `resolve` rejects a
  mutated version; bundle write/load carries it
  (`examples/adcs-fsw2/tests/bundle.rs` as the integration check).
- Risk: churn in every literal `Wiring`/builder construction in
  `src/wiring/tests.rs`, `tests/wiring_resolve.rs`, `tests/slot_wiring.rs`.
  Mechanical.

## WP2 — `SourceRef` provenance + scope table

Independent of WP1 (trivial merge overlap in `model.rs`).

- `src/wiring/model.rs`:
  - `pub struct SourceRef { pub file: Option<String>, pub line: u32, pub col: u32 }`
    (serde, Clone, PartialEq). One anchor now; Phase 1 may add a
    `#[serde(default)]` stack of caller frames without breaking this shape.
  - `#[serde(default)] pub src: Option<SourceRef>` on `SystemSpec`,
    `SlotSpec`, `AllowedOccupantSpec`, `EdgeSpec` (the design's list), and —
    cheap and useful for `ArtifactNotBuilt`-class errors — `Artifact`.
  - Scope table: `#[serde(default)] pub scopes: Vec<ScopeSpec>` on `Wiring`,
    `pub struct ScopeSpec { pub path: String, pub parent: Option<usize>, pub src: Option<SourceRef> }`;
    `#[serde(default)] pub scope: Option<usize>` on `SystemSpec` and
    `SlotSpec`. The KDL front-end leaves the table empty and every `scope`
    `None`; `resolve()` only range-checks indices (new
    `LoadError::BadScopeRef`). Instance names stay flat and
    collision-checked exactly as today — the table is consumer metadata.
- `src/wiring/parse.rs`: a `line_col(src: &str, offset: usize)` helper
  (1-based, matching miette's rendering); fill `src` from each node's / allow
  child's `span()`. Add `pub fn parse_with_origin(kdl: &str, origin: Option<&str>)`
  (existing `parse` delegates with `None`); `src/cli.rs` passes the KDL path
  in `cmd_build`/`cmd_package`/`load_run_wiring`.
- `src/wiring/mod.rs`: the fabricated-snippet helpers `system_src`/`slot_src`/
  `edge_src` gain the anchor — render
  `system "plant" type="Plant"  @ mission.kdl:14:1` when `spec.src` is
  present. This unifies error *anchoring* without rewriting `LoadError`'s
  miette plumbing; full SourceRef-native rendering (design §7 tier 2,
  including the E5d raw-index branches in `WireError`) is Phase 1 work and
  should not be pulled in here.
- `src/wiring/builder.rs`: no new surface required in Phase 0 (the builder is
  a Rust-side front-end; Phase 1's evaluator sets these fields directly on
  the specs). Optionally a `SystemSpecBuilder::src(SourceRef)` for tests.
- Tests: parse fills expected line/col for a known document (assert against
  a literal mission snippet); deserializing old-shape JSON (fields absent)
  yields `None`/empty via the defaults; anchored error text appears in a
  resolve failure for a builder-origin vs parse-origin `Wiring`.
- Risk: low. Watch 0- vs 1-based line/col conventions, and multi-line
  `system` nodes (anchor to the node start).

## WP3 — unify occupant resolution (`resolve_occupant`)

Pure refactor of `src/wiring/mod.rs`; prerequisite for WP4's dl arm (so the
`Value` params arm is added in one place, not four).

- Introduce an entry-source abstraction over the two ways an artifact
  self-describes:
  ```rust
  enum EntrySource<'a> {
      Opened(&'a crate::dl::DlPack),                      // in-process dl
      #[cfg(any(target_os = "linux", target_os = "macos"))]
      Described(&'a [crate::dl::PackEntryMeta]),          // describe worker
  }
  ```
  with one `select` (folding today's `select_entry` and `select_proc_entry`,
  keeping their exact error variants: `PackTypeRequired`, `PackSystem`) and
  accessors for `descriptor`/`params_schema`/`params_default`/`reloadable`.
- One
  `fn resolve_occupant(source: &EntrySource, entry: Option<&str>, params: &ParamSource, owner: &str, reserved: &'static [&'static str], skip_args: usize, require_reloadable: bool, src: &str, span: SourceSpan) -> Result<OccupantParts, LoadError>`
  returning `{ name, descriptor, params: Vec<u8> }`. Note `require_reloadable`
  is a flag, not unconditional: `resolve_dl` today does **not** check
  reloadable (a wired non-reloadable single-instance entry is legal); only
  slot occupants require it.
- Rewrite the four callers (`resolve_dl`, `resolve_proc`, `resolve_slot`'s dl
  loop, `describe_occupants`) over it; `encode_occupant_params` becomes the
  shared params arm inside it (the proc path's inline duplicate at
  `wiring/mod.rs:775-786` and `:1159-1170` folds in).
- Tests: no new behavior — `tests/dl_integration.rs`, `tests/slot_wiring.rs`,
  `tests/slot_integration.rs`, `tests/proc_integration.rs`, and
  `src/wiring/tests.rs` are the regression net. Add one unit asserting the
  reloadable flag is *not* enforced for a wired dl system and *is* for a slot
  occupant.
- Risk: error-variant drift between the four paths (each has slightly
  different owner naming); diff the rendered errors in the existing negative
  tests before/after. `cfg` discipline: `Described` and its plumbing stay
  Linux/macOS-gated like `resolve_proc`.

## WP4 — `ParamSource::Value(serde_json::Value)`

The core seam change. Depends on WP3; WP2's anchors improve its diagnostics
but are not a hard prerequisite.

**Model and dl path**

- `src/wiring/model.rs`: add `Value(serde_json::Value)` to `ParamSource`;
  rewrite the variant docs (three sources: KDL text → re-decoded, Value tree →
  conform+encode for dl / serde-deserialize for static, Postcard → verbatim
  dl-only). Replace `SystemSpec::tcp_builtin`'s fabricated KDL with
  `ParamSource::Value(serde_json::json!({ "addr": addr.to_string() }))` —
  `DownlinkParams.addr: SocketAddr` deserializes from a JSON string, and its
  `#[serde(default)]` `instances`/`frames` are honored by serde (unlike the
  schema-conform path, serde field defaults work here — this is exactly why
  the static path decodes with serde rather than conform).
- `src/wiring/kdl_params.rs`: split the pipeline. New
  `pub fn encode_value_params(value: &serde_json::Value, schema: &OwnedNamedType, system: &str, defaults: Option<&[u8]>) -> Result<Vec<u8>, LoadError>`
  = `merge_onto_defaults` → `conform_to_schema` (empty span table; errors
  anchor to the whole surface) → `postcard_dyn::to_stdvec_dyn`.
  `encode_kdl_params_with_defaults` becomes KDL-parse + `de::params_value` +
  a call into the shared tail so the two paths cannot drift.
- `resolve_occupant` (WP3) gains the `ParamSource::Value` arm calling
  `encode_value_params`. `conform_to_schema` → `postcard_dyn` stays the single
  dl validation, per the design.

**Static path (closes `StaticPostcardParams` for value trees)**

- `src/wiring/mod.rs`: `LoadCtx` loses `node`/`src` in favor of
  `params: StaticParams<'a>`:
  ```rust
  pub enum StaticParams<'a> {
      Kdl { node: &'a kdl::KdlNode, src: &'a str },
      Value(&'a serde_json::Value),
      None,
  }
  ```
  `factory::<S, K>`: `Kdl` → `de::from_kdl_node` (unchanged); `Value` →
  `serde_ignored`-wrapped `serde_json::from_value::<S::Params>`, with an
  ignored path surfacing as `LoadError::UnknownParam` and a decode failure as
  a new `LoadError::ValueParams { system, reason }` (anchored via the spec's
  `SourceRef`); `None` → keep today's synthesized minimal node so the
  `MissingParam` diagnostics for required fields are unchanged. `configure()`
  still runs on every static arm.
- `resolve_static`: `ParamSource::Value` maps to `StaticParams::Value`;
  `ParamSource::Postcard` keeps the `StaticPostcardParams` rejection
  (the typed-builder seam is closed by `Value`, not by teaching statics
  postcard).
- `src/pack.rs`: `EntryParams` gains
  `Value { value: &'a serde_json::Value, msgs: &'a MsgTable }` (kdl-feature
  gated alongside the `Kdl` arm for now, see disagreement #6);
  `decode_params` adds the serde_ignored arm; `resolve_defaults` adds a
  `Value` arm through `encode_value_params`. `Registry::register_pack`'s
  factory closure passes it through, so pack entries served statically accept
  `Value` too.
- `src/wiring/builder.rs`: add `SystemSpecBuilder::params_value(serde_json::Value)`
  and `SlotSpecBuilder::allow_with_value(occupant, serde_json::Value)` — the
  Rust front-end needs a way to produce the new variant, and the tests need
  it.
- `Cargo.toml`: add `serde_ignored` (optional, under `kdl` with
  `serde_json`).
- Tests (`src/wiring/tests.rs`, `tests/wiring_resolve.rs`,
  `tests/dl_integration.rs`, `tests/slot_wiring.rs`):
  - **Byte-parity**: for the dl fixture pack, `Value` params encode to the
    identical postcard bytes as the equivalent KDL node (the design's core
    equivalence claim).
  - Static system resolves and runs from `Value` params; unknown key in a
    `Value` is an error (serde_ignored); missing required field errors;
    `#[serde(default)]` fields may be omitted.
  - `Value` + declared entry defaults overlays top-level keys only (mirror
    the existing KDL-defaults test).
  - `telemetry()`/`uplink()` builder specs now carry `Value` — adapt any test
    matching `ParamSource::Kdl` on the builtins, and keep
    `telemetry_node_loads_and_runs` (KDL text path) green.
  - `StaticPostcardParams` negative test (`src/wiring/tests.rs:1329`) stays.
- Risks: medium. Error-message churn on the static path (serde_json's
  messages differ from `de.rs`'s spanned ones — acceptable for the
  builder/Value origin, which never had spans); JSON number edge cases (u64
  vs i64 vs f64) on the *static* serde path differ subtly from
  `conform_value`'s width checks — add a test for an out-of-range int; the
  `LoadCtx` signature change ripples to every registered factory (all
  in-crate).

## WP5 — slot descriptor de-positionalization + single-pass validation

Coordinator-side; logically independent, but textually collides with WP3/WP4
in `resolve_slot`, so schedule after WP4. **This and WP7 are the two flagged
unknowns; scope findings below.**

- `src/coordinator/slot.rs`: replace `SlotReg`'s `n_occ_inputs`/
  `n_occ_outputs` with a named plan, keeping the three concepts distinct
  (disagreement #5):
  ```rust
  pub(crate) struct SlotPorts {
      /// The occupant's ABI prefix: its user ports plus the mount-appended
      /// SlotControlIn input and SequenceStatus output, in bind order.
      pub occupant_inputs: Vec<PortDesc>,
      pub occupant_outputs: Vec<PortDesc>,
      /// The runner tail: commands fan-in, SequenceStatus self-tap;
      /// SlotStatus and the "sequences" events channel.
      pub tail_inputs: Vec<PortDesc>,
      pub tail_outputs: Vec<PortDesc>,
  }
  ```
  with `fn registered(&self, name: &'static str) -> SystemDescriptor`
  flattening prefix-then-tail. **Invariant to state and test: the flattened
  registered descriptor is byte-identical to today's** — edge resolution,
  registry keys (`"<slot>.sequences"` etc.), and the occupant-side positional
  bind ABI all depend on it.
- `src/coordinator/mod.rs`:
  - `add_slot` builds `SlotPorts` instead of pushing onto one list and
    recording split indices; `shared_outputs` (`:1775`), `bind_slot`
    (`:2448`), and `slot_proc_parts` (`:2597`) read
    `slot_ports.occupant_inputs.len()` (or iterate the named lists) instead
    of raw counts. The tail-port location-by-shape searches in `bind_slot`
    (`control_in_idx`, `cmd_in_idx`) can become direct references into the
    named plan — a real simplification.
  - Collapse the double validation. Split it by what it needs:
    - **Pure-spec checks** (allow set non-empty, `initial` ∈ allow set):
      extract one `fn validate_slot_spec(allowed_names, initial) ->
      Result<(), SlotConfigError>` in `coordinator` and call it from
      `resolve_slot` **before any artifact is opened** (preserving today's
      fail-before-dlopen property, which a naive "move it all into
      `add_slot`" would regress) and from `add_slot`.
    - **Descriptor checks** (mutual occupant compatibility, mixed backing,
      occupant illegally declaring `SlotControlIn`/`SequenceStatus`): these
      move solely into `add_slot`, which becomes fallible —
      `pub fn add_slot(...) -> Result<SystemHandle, SlotConfigError>` — and
      `resolve_slot` **deletes** its duplicate `ports_match` loop, mapping
      `SlotConfigError` onto the existing `LoadError` variants
      (`EmptySlot`, `SlotOccupantMismatch`, `UnknownInitialOccupant`) so
      diagnostics and tests keep their shapes.
  - The reloadable check stays in `resolve_occupant` (WP3) — it needs entry
    metadata `add_slot` never sees.
- Tests: `src/coordinator/tests.rs:1477` (literal `SlotReg`) updated; the
  `add_slot` panic tests become `Err` tests; **add a registered-descriptor
  snapshot test** (exact port name/conn/id sequence for a representative
  slot) guarding the flattening invariant; `tests/slot_wiring.rs`,
  `tests/slot_integration.rs`, `tests/proc_integration.rs`, and the adcs
  example's `sequences.rs`/`swap_repro.rs`/`abort_repro.rs` as the behavioral
  net.
- **Bigger-than-assumed findings to expect** (verify during implementation):
  - The process-slot manifest writer (`slot_proc_parts`) and
    `shared_outputs` both encode the "occupant prefix crosses, tail stays
    host-side" rule via the counts; converting them is mechanical but
    mistakes here corrupt worker ring manifests silently — lean on
    `proc_integration.rs`.
  - `add_slot` returning `Result` is a public API change; the direct
    (non-wiring) builder path in doc examples and `coordinator/tests.rs`
    needs `?`/`unwrap` sweeps.
  - Do **not** attempt to make `SystemDescriptor` itself slot-aware; the flat
    descriptor is what edges, telemetry, and the registry consume. The named
    model lives in `SlotReg`/`add_slot` only.

## WP6 — defaults: `Pack::system_type_with_defaults` + macro blob

Independent; can land any time (touches `pack.rs`, `macros/`,
`src/system/mod.rs` only).

- `src/pack.rs`: `pub fn system_type_with_defaults<T, O>(self, name: &'static str, defaults: T::Params) -> Self`
  with `T::Params: Serialize` added to the existing bounds — postcard-encode
  the blob, set `params_default`, call `wrap_create_with_defaults()`
  (mirroring `task_with_defaults` at `pack.rs:347-368`, including the schema
  identity assert). No ABI change: `PackSystemMsg.params_default` already
  crosses.
- Automatic emission: add a provided hook to `BuildSystem`
  (`src/system/mod.rs:193`):
  `fn params_default_blob() -> Option<Vec<u8>> { None }`. `Pack::system_type`
  consults it: `Some(blob)` → set + wrap (empty blob treated as none,
  matching the existing `!bytes.is_empty()` guards). The `#[system]` macro
  (`macros/src/system_attr.rs:624-658`, the `Some(Some(p))` arm) emits an
  override using **autoref specialization** over the concrete `#p`
  (inherent `impl<..> Probe<P> where P: Default + Serialize` beats the
  blanket trait fallback), returning `Some(postcard(P::default()))` when the
  bound holds. Unit-params arms keep the `None` default.
- Documented limits (from disagreement #8): a generic `#[system]` type
  resolves the probe against unsubstituted generics and silently yields
  `None`; hand-written `BuildSystem` impls yield `None` — both use the
  explicit `system_type_with_defaults`. Also document that the blob affects
  the **dl/pack path only**; static `Registry::register` decoding still
  honors `#[serde(default)]` field attributes instead (the two mechanisms
  coexist; stubgen unifies presentation in Phase 2).
- Tests: `src/abi/tests.rs` — a `system_type` entry with `Params: Default`
  reports `params_default` in the described manifest; `src/wiring/tests.rs` —
  a dl `system` node with *no* params runs on the defaults, and one spelling
  a single override merges top-level; a trybuild pass case
  (`tests/ui`) for a `Params` without `Default` still compiling. In the adcs
  example, `Ctrl` (`system_type`, `adcs-systems/src/lib.rs:31`) is the
  natural demo: give `CtrlParams` a `Default` and shrink the
  `mission.kdl:9-13` "no serde defaults on the dl path" comment — but keep
  the mission's spelled-out params working either way (both forms tested).
- Risks: the autoref trick is subtle and macro-expanded — keep it in one
  helper emitted verbatim, with an expansion unit test in
  `macros/src/system_attr.rs`'s test module; the schema-vs-defaults type
  mismatch assert must fire for wrong `defaults` types like
  `task_with_defaults`'s does.

## WP7 — compile-time pack manifest sidecar

Independent. **Redesigned from the doc's phrasing** (disagreement #1): the
sidecar is a **build-driver product**, generated by describing a
host-runnable build of the artifact, written next to the target `.so`.

- Format: the manifest contains binary vtable/metadata blobs
  (`PortSchemaMsg::Table`), so "JSON next to the .so" would mean
  byte-array-laden JSON. The canonical sidecar is the **raw postcard
  `PackManifestMsg` bytes** — `<cdylib>.manifest` — because §5.3's staleness
  hash is defined over the manifest postcard bytes and this keeps
  sidecar-hash ≡ describe-hash by construction. Stubgen (Phase 2) renders
  JSON from it.
- `src/wiring/build_driver.rs`:
  - `BuildOptions` gains `manifest_sidecar: bool` (default `true`) and the
    driver learns whether `extra_args` carries a `--target` differing from
    the host.
  - Native build: after locating the cdylib, obtain manifest bytes and write
    `<path>.manifest`. Sourcing: prefer `proc::host::describe_via_worker`
    (never dlopens into the driver process — the same isolation §5.1 demands
    of stubgen), falling back to in-process `DlPack::open` + describe where
    the worker machinery is unavailable (non-Linux/macOS, and test binaries
    that can't re-exec as workers — the adcs harness already documents that
    constraint).
  - Cross build: additionally run `cargo build -p <crate>` **without**
    `--target` (host arch), describe *that* artifact, write the sidecar next
    to the **target** `.so`. Hard error (not silent skip) if the host build
    fails while `manifest_sidecar` is on — a missing sidecar would surface
    much later as a stubgen/staleness mystery.
- `src/dl.rs`: `pub fn read_manifest_sidecar(so_path: &Path) -> Option<Result<Vec<PackEntryMeta>, DlError>>`
  reusing `decode_pack_manifest`. Phase 0 wires **verification, not
  consumption**: `resolve_proc`/`describe_occupants` keep the describe
  worker; switching them to prefer the sidecar is a one-line follow-up once
  trust is established (leave a TODO referencing Phase 2).
- `src/wiring/bundle.rs`: copy sidecars into bundles when present
  (forward-compat for Phase 3's manifest-hash checks; cheap).
- Tests: `tests/dl_integration.rs` (the fixture pack) — build via
  `build_artifacts`, assert the sidecar exists and its bytes decode to the
  same entries `fsw_pack_describe` reports (byte-equality of the postcard
  bodies); a `manifest_sidecar: false` opt-out test.
- **Bigger-than-assumed findings to expect**:
  - "Arch-independent manifest" is an *assumption*, not a guarantee: schema
    shapes include `Usize`/`Isize`, and `PortDesc.max_size` comes from
    type-level `MAX_SIZE` consts that could differ across pointer widths if
    a frame ever carries `usize`. Add a CI-shaped check (compare host and
    target sidecars whenever a cross build produces both) rather than
    assuming; if divergence appears, that's a design escalation, not
    something to paper over here.
  - The host-arch second build assumes every pack crate compiles for the
    host (target-specific deps break it). The opt-out flag is the escape
    hatch; document it.
  - Generation executes `pack()` at build time — same trust model as
    `build.rs`, worth one sentence in the docs.

## Order and checkpoints

1. **WP1** (`ir_version`) → crate tests green → commit.
2. **WP2** (`SourceRef` + scopes) → parse/serde tests green → commit.
   Parallelizable with WP1 apart from trivial `model.rs` merges.
3. **WP3** (occupant unification) → full dl/slot/proc test net green →
   commit. Prerequisite of WP4.
4. **WP4** (`ParamSource::Value`) → byte-parity + static-value tests green,
   adcs example tests green → commit. The phase's centerpiece.
5. **WP5** (slot de-positionalization) after WP4 (shared `resolve_slot`
   text) → descriptor snapshot + slot/proc integration green → commit.
6. **WP6** (defaults) and **WP7** (sidecar) are independent of everything
   above and of each other; parallelize freely.
7. Final sweep: `docs/wiring.md` / `docs/coordinator.md` /
   `docs/design-packs-authoring.md` touch-ups for the new variant, defaults,
   and sidecar; full workspace build + clippy; adcs-fsw2 suite
   (`closed_loop.rs`, `sequences.rs`, `bundle.rs`, `alarms.rs`, …) as the
   end-to-end acceptance that Phase 0 was invisible to the KDL front-end.
