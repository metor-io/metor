# WP8 (dl-open) — implementation plan

Short plan onto `docs/dl-open.md` (reviewed, decisions locked 2026-06-28). Locked: **same-process v1**
(`RawBacking` over the host's `BoxBacking` rings, bare `{base,len,role}` handle); **in-document
`artifact` node**; **one canonical postcard `Params` encoding** via `postcard-schema` + `postcard-dyn`
(host stays schema-agnostic). Cyclic-only; async / multi-process / wake reserved.

Each wave is one or more agents; verify (`cargo test`/`cargo build`) and **commit per verified slice**
before the next wave (memory: commit at task boundaries). Existing suites stay green at every step.

## Wave 1 — foundations (parallel; additive / mechanical)

- **1A · `RawBacking` + `attach_raw`** (`ring/src/lib.rs`). Non-owning `Backing` over `(base,len)`;
  `attach_raw` = `attach_mmap` minus the mmap step (validate header via `read_header`, build `Inner`).
  Tests: attach over a `BoxBacking` region in the same process; writer-in-region / view-in-region
  round-trip; arch-tag mismatch rejected. Miri over the attach/read path.
- **1B · make the `System` stack `Backing`-generic** (the reviewed zero-copy decision; doc §1.2).
  Cross-cutting but mechanical: `System<B = BoxBacking>`/`CyclicSystem<B = BoxBacking>`; generic
  bundles (`PlantOut<B>`); a `RingSource` trait abstracting bind with `Binder`(BoxBacking) +
  `RawBinder`(RawBacking) impls; `BindPorts<B>::bind<S: RingSource<B=B>>`; generic `Output/Input::bind`;
  `CyclicRunner<S,O,B>`; thread `B` through `Out<O,B>`/`HealthPort<B>`. Extend the
  `SystemInput`/`SystemOutput` derives to emit the `BindPorts<B>` impl. Static call sites stay
  source-compatible via the `B = BoxBacking` default. **Spike the trait/derive shape to a compiling
  minimal slice first**, then thread it through. Update the `adcs-fsw2` bundles to `<B>`. All WP3–WP7
  tests + the example stay green. (Separately, **broaden `PortDesc.announce` to a boxed closure** —
  `Arc<dyn Fn(&str)->(VTable, Vec<ComponentMetadata>)>` — so a dl registry entry can carry its
  metadata-derived announce; needed by §7 regardless of model.)

## Wave 2 — the ABI (needs Wave 1)

- **2A · serializable descriptor mirrors** (`metor-proto` or `metor-fsw-2`): `PortDescMsg` /
  `SystemDescriptorMsg { …, params_schema: OwnedNamedType }`; postcard. Lower a `SystemDescriptor` →
  msg (drop the `announce` fn, carry unprefixed `vtable` + `metadata`) and back. Round-trip test.
- **2B · `export_system!` macro + C-ABI exports** (`metor-fsw-macros` + a `metor-fsw-2::abi` module):
  `fsw_abi_version`, `fsw_describe` (descriptor + params schema via host sink), `fsw_create`
  (postcard-decode `Params`), `fsw_bind_init` (`RawBinder` over the `FswRing` array → typed bundle,
  then `System::init`), `fsw_execute` (`CyclicRunner::step`), `fsw_shutdown`, `fsw_destroy`. Every
  export `catch_unwind`-wrapped → `FswStatus`. `RawBinder` mirrors `Binder`'s positional walk over
  `attach_raw`. Needs 1A/1B/2A.
- **2C · host `DlSystem` loader + `DlSlot : CyclicSlot` + `add_dl_cyclic`** (`src/` new module +
  `coordinator.rs`): `libloading` open + symbol bind + version check; `fsw_describe` →
  `SystemDescriptor` (synthesize `announce` from metadata); `DlSlot` forwards init/step/shutdown and
  maps `FswStatus`→`SlotState`; `add_dl_cyclic` registers it via a `Reg::Dl` branch so connect / build
  / sizing / telemetry are all reuse. Needs 1A/2A.
- Integration test for Wave 2: a tiny in-crate cdylib system, `dlopen`'d, wired to a static consumer,
  one cycle, output read back + descriptor-validated.

## Wave 3 — build / wiring data model (needs Wave 2)

- **3A · `Wiring` data model + `WiringBuilder`** (`src/wiring/` split): `Wiring { coordinator,
  artifacts, systems, edges, telemetry }`; `Artifact { id, crate_name, cdylib, exports, path }`;
  `SystemSpec { name, ty, artifact: Option, params: postcard bytes }`. Builder constructs it in Rust;
  `.param(..)` encodes typed `Params`.
- **3B · refactor `wiring.rs::load` → KDL→`Wiring`→resolve** + the `artifact` node / per-system `lib=`
  surface. KDL params encoded via `postcard-dyn` guided by the `.so`'s exported `Params` schema.
  Static (`Registry`) and dl resolution share one resolver. WP6 KDL tests still pass; add
  builder≡KDL equivalence tests.
- **3C · build driver**: read `artifacts`, `cargo build -p <crate>` each, resolve `.so` path; content
  hash for changed-only redeploy. Small.

## Wave 4 — integration milestone (needs Wave 3)

- **4 · refactor `examples/adcs-fsw2`**: split `Plant`/`Nav`/`Ctrl` into an `adcs-systems` cdylib
  (impls unchanged + one `export_system!` each); frames + `Params` move to a shared `adcs-contracts`
  crate (shared among cdylibs, not host-linked); mission host builds a `Wiring` (builder + KDL with
  `artifact`/`lib=`), runs the build driver, dlopen-resolves, runs the closed loop. **Acceptance:
  `tests/closed_loop.rs` converges identically to the static mission**; live telemetry `All` still
  streams to metor-panel.

## Cross-cutting checks
- No unwilling host-side knowledge of frame/param Rust types (schema-agnostic host) — assert by the
  host crate not depending on `adcs-contracts`.
- `panic = "abort"` on the cdylib + `catch_unwind` at every export (Q-panic belt-and-suspenders).
- Teardown order: `fsw_destroy` every dl slot **before** the host `RingTable` drop (no `RawBacking`
  outlives its region).
