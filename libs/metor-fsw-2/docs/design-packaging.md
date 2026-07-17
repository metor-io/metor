# Design: pack packaging and distribution

Status: accepted design, pre-implementation. Builds on `design-python-config.md`
(the Python front-end, stubgen, and bundles) and on the build-backend prototype
findings (`findings-python-build-backend.md`, branch `sphw/py-backend`).
Companion plan docs will be written per phase (§12).

## 1. Summary

Packs become publishable artifacts: a publisher compiles a pack crate once for
every supported target, and a mission consumes it as an ordinary dependency —
no cargo, no cross toolchains, full pyright autocomplete. The distribution
format is a Python wheel and the resolver is uv; the "registry" is any
PEP 503 package index (PyPI, devpi, Artifactory, an S3 static index, or a
plain `--find-links` directory), so the design carries no metor-specific
hosting requirement.

Four pillars:

1. **The pack wheel** — a fat `py3-none-any` wheel per pack crate carrying the
   generated typed stub module, the pack manifest sidecar, and cdylibs for all
   supported triples. One artifact, one hash, arch selection at consume time.
2. **A published toolchain** — `metor-fsw` (platform binary wheels),
   `metor-config` (the recorder), `metor-build` (the PEP 517/660 backend
   promoted from the prototype), and `metor-fsw-abi` (an empty marker dist
   whose version *is* the ABI number, turning ABI mismatches into resolver
   errors).
3. **Prebuilt artifacts in the IR** — `Artifact` grows a prebuilt flavor
   (a directory of per-triple libs) beside today's crate flavor;
   `metor-fsw package --target` selects the flight triple's lib from the
   wheel payload and never runs cargo for a published pack.
4. **uv-native local dev** — a local pack is the same dependency with a
   `[tool.uv.sources]` path line; the pack's own build backend regenerates
   host-arch libs and stubs on source change. Deleting the line switches to
   the index build; the import name is identical either way.

## 2. Goals and non-goals

Goals, in priority order:

1. Publish compiled packs somewhere; mission configs consume them by name and
   version.
2. Binary-only consumers: a mission author needs uv and `metor-fsw`, nothing
   else. Publishers do all compilation.
3. Seamless flip between published packs and local in-repo pack crates —
   both present in one mission is the steady state, not an edge case.
4. Multi-target distribution for the supported triples
   (`aarch64-unknown-linux-gnu`, `x86_64-unknown-linux-gnu`,
   `aarch64-apple-darwin`): an author on macOS simulates on the host arch and
   packages flight bundles for a foreign arch from the same installed pack.
5. Keystroke-time autocomplete via the existing stub machinery.
6. Registry-agnostic: the audience (org-internal, customers, public) is
   undecided, so nothing may assume a particular index or auth story.

Non-goals:

- A metor-hosted registry service. Any PEP 503 index works; hosting is an
  operational choice deferred to §11's playbooks.
- Source distribution. There is deliberately no sdist (§7): an sdist would
  silently reintroduce cargo as a consumer requirement, defeating goal 2.
- Imposing Nix on mission authors. Nix is endorsed producer-side (§7.2) and
  never crosses to the consumer.
- Windows. The three-triple matrix is the support surface; the wheel format
  leaves room (`*-windows-*` naming exists in `cdylib_file_name`) but nothing
  is built or tested for it.

## 3. Why wheels

The distribution unit must carry three things to a machine that cannot build
them: compiled cdylibs for several triples, the pack manifest, and the typed
Python module that gives autocomplete. Candidates surveyed:

- **Python wheels + uv (chosen).** The consumer-side problems — resolution,
  version ranges, lockfiles, hashes, caching, index auth, editable local
  overrides, and exposing typed modules to pyright — are exactly the problems
  Python packaging already solves, and the mission environment is already a
  uv project (the Python-config design made `pyproject.toml` the mission's
  build-metadata home and the prototype proved the backend loop end to end).
  Wheels are dumb zips served over dumb HTTP; every org has or can trivially
  run an index.
- **OCI registries (ORAS) + a metor-owned resolver.** Ubiquitous org infra
  and per-arch layer pulls, but metor would own a version solver, lockfile,
  cache, and credential story, plus a second lockfile beside `uv.lock` that
  can skew — and the editor-integration bridge that wheels get for free would
  still have to be built. Strictly more code for a worse consumer UX; kept as
  a documented door (§10), not taken.
- **Nix flakes + binary cache.** The best cross-compilation and
  bit-reproducibility story of the three, but it imposes flakes on
  aerospace/controls engineers whose stated baseline is "some Python/MATLAB"
  (the Python-config design's goal 3), offers exact pins instead of a
  resolver, and would still end up generating the same `.pth`/module layout
  for pyright. Rejected as the consumer layer, endorsed as producer infra
  (§7.2).
- **Cargo registries (source).** Consumers build from source: kills the
  multi-arch hosting problem but requires full Rust cross toolchains on every
  mission machine and forecloses closed-source pack distribution. Fails
  goal 2 outright.

## 4. The published distributions

| dist | contents | tags |
|---|---|---|
| `metor-fsw` | the `metor-fsw` binary in `.data/scripts/` | one wheel per host platform |
| `metor-config` | the recorder library (`python/metor-config/`) | `py3-none-any` |
| `metor-build` | the PEP 517/660 backend | `py3-none-any` |
| `metor-fsw-abi` | empty marker; version == `FSW_ABI_VERSION` | `py3-none-any`, one release per ABI bump |
| `<pack>-pack` | typed stub module + manifest + per-triple cdylibs | `py3-none-any` (fat, §5) |

`metor-fsw` as a binary wheel (the ruff pattern: the executable ships in the
wheel's `.data/scripts/`, installers copy it into the env's `bin/`) is what
makes the whole toolchain uv-bootstrappable: missions depend on it and get
`uv run metor-fsw …`; pack crates name it in `build-system.requires` and the
backend finds it on `PATH` — uv prepends the build environment's scripts
directory to the hook subprocess's `PATH` (verified against uv's
build-frontend source). Fallbacks stay from the prototype: `METOR_FSW_BIN`,
then `cargo run -p metor-fsw-2` for monorepo dev.

`metor-fsw-abi` is the resolver-level ABI gate, §9.1.

## 5. The pack wheel

One pack = one crate = one dist. For dist `adcs-pack` version 1.2.0, module
`adcs_pack`:

```
adcs_pack/
  __init__.py            generated typed stub module (prebuilt flavor, §5.2)
  py.typed
  _libs/
    aarch64-unknown-linux-gnu/libadcs_systems.so
    aarch64-unknown-linux-gnu/libadcs_systems.so.manifest
    x86_64-unknown-linux-gnu/libadcs_systems.so
    x86_64-unknown-linux-gnu/libadcs_systems.so.manifest
    aarch64-apple-darwin/libadcs_systems.dylib
    aarch64-apple-darwin/libadcs_systems.dylib.manifest
adcs_pack-1.2.0.dist-info/
  METADATA               Requires-Dist: metor-config>=X,<X+1; metor-fsw-abi==8
  WHEEL                  Tag: py3-none-any, Root-Is-Purelib: true
  RECORD
```

The per-triple `.manifest` sidecars are byte-identical copies of the one
arch-independent manifest — duplicated next to each lib so the existing
sidecar-adjacent lookup and the whole resolve path stay unchanged. Assembly
verifies the copies are identical (§7.1).

### 5.1 Fat `py3-none-any`, not platform tags

Wheel platform tags select for the **installing** host, not the deploy
target. A platform-tagged `macosx_arm64` wheel would give a macOS author only
the darwin lib — and `metor-fsw package --target aarch64-unknown-linux-gnu`
would have nothing to package. Platform tags are semantically wrong for a
deploy-target artifact, not merely inconvenient; that alone decides it.

The fat wheel also gives the best lockfile story: one artifact, one sha256,
identical on every machine and in CI. Sizes are three release-profile,
stripped cdylibs — typically a few MB each, far under index limits.
`pack publish` forces release + strip and warns near limits (a debug fat
wheel would balloon silently). If a pack ever outgrows this, the documented
escape hatch is sibling lib dists (`adcs-pack-libs-<triple>`) selected via
extras — an evolution, not built now, because it makes missions declare
deploy targets in their dependency list.

Rejected variant: a stubs-only wheel plus a package-time fetch of flight-arch
libs. That reintroduces a second fetcher outside uv's resolver/lock/cache and
breaks air-gapped sync-then-package workflows.

### 5.2 The stub module, prebuilt flavor

Stubgen gains a second flavor. The published module's `ARTIFACT` locates its
own payload and carries provenance:

```python
ARTIFACT = Artifact(
    id="adcs", crate="adcs-systems", lib="adcs_systems",
    manifest_hash="sha256:…",
    prebuilt=str(Path(__file__).resolve().parent / "_libs"),
    abi_version=8, dist="adcs-pack", dist_version="1.2.0",
)
```

By construction the stub is generated from the same manifest shipped in the
wheel, so `StaleStubs` cannot fire for a published pack; it stays meaningful
for local editable packs raced with `uv run --no-sync`, exactly as today.

### 5.3 Import naming: one top-level module per pack

`from adcs_pack import Plant` — not `from packs.adcs import Plant`, and not a
shared `metor_packs.*` namespace. A PEP 420 namespace merged across search
roots is precisely the layout that breaks: pyright resolves only the first
namespace portion it finds ([pyright #2882], closed as-designed;
[pylance-release #3002] documents an editable install shadowing all sibling
portions), and the prototype independently hit the `sys.path[0]` variant with
the checked-in `packs/` dir. Since mixed local+published packs are the steady
state (goal 3), the design must never depend on cross-root namespace merging.

Per-pack top-level modules eliminate the hazard: every import resolves to
exactly one portion in exactly one root, CPython and pyright agree by
construction, and each wheel owns its whole top-level package so
`RECORD`-based uninstalls are clean. The module name defaults to the
normalized dist name; `[tool.metor.pack] module = "…"` overrides. Missions
wanting short names alias: `import adcs_pack as adcs`.

[pyright #2882]: https://github.com/microsoft/pyright/issues/2882
[pylance-release #3002]: https://github.com/microsoft/pylance-release/issues/3002

## 6. Authoring and local dev

### 6.1 The pack crate's `pyproject.toml`

Lives in the pack crate directory, beside `Cargo.toml`:

```toml
[project]
name = "adcs-pack"
version = "1.2.0"
requires-python = ">=3.11"

[build-system]
requires = ["metor-build>=0.1,<0.2", "metor-fsw>=0.9"]
build-backend = "metor_build"

[tool.metor.pack]
id = "adcs"              # artifact id (default: module name)
crate = "adcs-systems"   # cargo package (default: this dir's [package].name)
lib = "adcs_systems"     # cdylib stem (default: [lib].name)
targets = [
  "aarch64-unknown-linux-gnu",
  "x86_64-unknown-linux-gnu",
  "aarch64-apple-darwin",
]

[tool.uv]
cache-keys = [
  { file = "pyproject.toml" },
  { file = "Cargo.toml" },
  { file = "src/**/*.rs" },
]
```

The `metor-config` and `metor-fsw-abi` pins are injected by the backend at
wheel-assembly time — authors never hand-maintain the ABI pin.

### 6.2 The mission's `pyproject.toml`

```toml
[project]
name = "cubesat-mission"
version = "0.1.0"
requires-python = ">=3.11"
dependencies = [
  "metor-fsw>=0.9",
  "metor-config>=0.4,<0.5",
  "adcs-pack>=1.2",
  "gnc-pack>=2.0",
]

# Local-dev flip: one line per pack; delete it to consume the index build.
[tool.uv.sources]
adcs-pack = { path = "../packs/adcs", editable = true }

[[tool.uv.index]]
name = "org"
url = "https://pypi.internal.example.com/simple"
```

Note what is gone relative to the prototype: no `[build-system]` (a mission
is a virtual project, never installed), no `[tool.metor.artifacts]`, no
in-tree `_backend/`, no `extra-pth`, no mission-level cache-keys. Stub
freshness for local packs moves into each pack's own editable build.

### 6.3 The editable loop

`uv sync` on the mission resolves the path source, installs the pack's
`build-system.requires` into an ephemeral build env, and calls
`metor_build.build_editable`, which shells to `metor-fsw pack dev <dir>`:
cargo-build the **host** triple only, write the manifest sidecar (the
existing build-driver path), render the stub in prebuilt flavor pointing at
`<dir>/.metor/libs/`, and lay out `.metor/` as
`{<module>/__init__.py, <module>/py.typed, libs/<host-triple>/…}`. The
editable wheel's payload is a `.pth` naming `<dir>/.metor`. The pack's own
`cache-keys` globs re-run this on source change — uv documents `cache-keys`
as per-package and governing rebuilds of local directory dependencies — so
`uv run` self-heals, as proven by the prototype.

After `uv sync` the venv holds host-arch libs for local packs, all-arch libs
for published packs, typed modules for both, and `metor-fsw` on the venv
PATH. `uv run metor-fsw run mission.py` needs no cargo unless a local pack
must be cross-built at package time.

## 7. Publishing

```
metor-fsw pack dev [DIR]                                      # host-only editable payload (backend calls this)
metor-fsw pack build [DIR] [--release] [--target T]...
                     [--libs-out DIR | --wheel-out DIR]
metor-fsw pack assemble [DIR] --libs DIR --wheel-out DIR      # CI-matrix join step
metor-fsw pack publish [DIR] [--index NAME|URL] [--dry-run]   # wraps `uv publish`
```

### 7.1 `pack build`

For each triple in `[tool.metor.pack].targets`: invoke the builder (§7.2);
additionally build the host twin and describe it via the existing
worker-quarantined path. The existing `ManifestDivergence` gate generalizes
from "host vs one target" to "host vs N targets": any per-triple sidecar that
differs from the host twin's is a hard error, and `pack assemble` re-verifies
all collected sidecars are byte-identical before writing the wheel — so a
skewed CI matrix (one runner built a different pack revision) is caught at
the join. Then render the stub module and write the wheel with a
deterministic Rust wheel writer: sorted zip entries, fixed timestamps, fixed
permissions, sha256 `RECORD` — the zip sibling of the reproducible ustar
writer in `wiring/bundle.rs`.

`uv build` on a pack dir also works (the backend's `build_wheel` shells to
`pack build --wheel-out`), so standard `uv build && uv publish` flows are
honored. `build_sdist` stays `NotImplementedError`: publishing is binary-only
by design (§2).

### 7.2 Pluggable builders

`[tool.metor.pack.builder]`, CLI-overridable:

- `kind = "cargo"` — native `cargo build --target T`; assumes rustup targets
  and linkers are configured (fine on a matching-arch machine).
- `kind = "zigbuild"` — `cargo zigbuild --target T`; covers both linux-gnu
  triples from any host. Its darwin story needs validation (§11); if it
  disappoints, darwin falls to a mac runner or a Nix shell.
- `kind = "command"` with a `{triple}`/`{crate}` template — the escape hatch
  for bespoke toolchains and the Nix hook: a per-triple cross devshell
  (`nix develop .#cross-{triple} -c cargo build …`) slots in here, and CI can
  build the whole wheel inside Nix for reproducibility without consumers ever
  knowing.
- CI-native matrix: each runner runs
  `pack build --target <its triple> --libs-out out/<triple>/`; one job joins
  with `pack assemble` + `pack publish`.

## 8. Consumption: IR v3 and provisioning

### 8.1 `Artifact` changes (IR v2 → v3)

```rust
pub struct Artifact {
    pub id: String,
    pub crate_name: String,
    /// The cdylib stem ("adcs_systems"); file names are derived per triple.
    pub lib: String,
    pub path: Option<PathBuf>,
    #[serde(default)] pub manifest_hash: Option<String>,
    /// Where prebuilt per-triple libs live (an installed wheel's `_libs`,
    /// or a local pack's `.metor/libs`). `None` = crate-built.
    #[serde(default)] pub prebuilt_dir: Option<PathBuf>,
    /// Publishing provenance ({ name, version }), carried into BundleMeta.
    #[serde(default)] pub dist: Option<DistRef>,
    #[serde(default)] pub src: Option<SourceRef>,
}
```

Replacing the serialized `cdylib` file name with the `lib` stem fixes a
latent bug this design surfaced: the recorder derives the file name on the
**host** (`metor_config._cdylib_file_name` mirrors `wiring::cdylib_file_name`),
so an IR recorded on macOS says `libadcs_systems.dylib` and cross-target
packaging for linux is wrong today. The stem is arch-neutral;
`cdylib_file_name_for(triple, stem)` derives the name per consumer. Dropping
a serialized field is the wire break that justifies the version bump — stale
producers fail loudly, matching the IR module's philosophy. `prebuilt_dir`
and `path` are both provenance-stripped by `path_stripped()` when freezing.

### 8.2 Provisioning

`build_artifacts` becomes `provision_artifacts`. Per artifact: if
`prebuilt_dir` is set, select `<dir>/<triple>/<cdylib_file_name_for(triple,
lib)>` (triple = requested `--target` or host), verify the adjacent sidecar
against `manifest_hash`, fill `path` — no cargo. A missing triple errors by
listing the triples the wheel actually ships. Otherwise the existing cargo
path runs unchanged (local crate-flavor packs, cross-built at package time
exactly as today). `host_triple()` gets a compile-time fallback so
prebuilt-only consumers do not need cargo on `PATH`. `package --target <t>`
becomes a first-class flag (prebuilt selection + forwarding to cargo for
crate-flavor artifacts), replacing the `--cargo-arg --target …` spelling for
this purpose.

## 9. Compatibility and provenance

### 9.1 ABI gating, three layers

1. **Resolve time (new): the `metor-fsw-abi` marker dist.** Every pack wheel
   is stamped `Requires-Dist: metor-fsw-abi==<abi>` at assembly; `metor-fsw`
   depends on its own ABI version. Mixing an ABI-9 `metor-fsw` with an ABI-8
   pack fails inside `uv lock` with a legible dependency conflict — pure
   standard-metadata mechanics, working against any index.
2. **Record time:** `ARTIFACT.abi_version` is checked during mission eval
   against the version the invoking `metor-fsw` reports through the existing
   `py.rs` env plumbing, naming the offending pack in one line.
3. **Load time (existing, unchanged):** the `fsw_abi_version` symbol check at
   dlopen and `BundleMeta::abi_version` at bundle load remain the backstops.

`metor_config`/IR compatibility rides normal semver: the backend injects a
`metor-config>=X,<X+1` pin matching the generating `metor-fsw`.

### 9.2 Lockfiles and the flight record

`uv.lock` pins each pack wheel by sha256 against a named index; a fat wheel
means one universal hash on every machine. Missions commit `uv.lock`. That
covers *fetching*; the flight record must outlive the venv, so `BundleMeta`
grows:

```rust
pub packs: Vec<PackProvenance>,   // serde-defaulted; old bundles load fine

pub struct PackProvenance {
    pub artifact_id: String,
    pub dist: Option<String>,         // "adcs-pack"; None for crate-built
    pub dist_version: Option<String>,
    pub source: PackSourceKind,       // Prebuilt | CrateBuilt
    pub cdylib_sha256: String,        // digest of the exact bytes packaged
    pub manifest_hash: String,
}
```

`cdylib_sha256` is computed during bundle member copy, closing the gap the
bundle docs call out today (a manifest hash checks interface compatibility;
it is not a digest of the shared object). `package --check-ir` is unchanged;
a future `package --locked` cross-checking `uv.lock` against installed
dist-info is optional hardening (§12, Phase 4).

## 10. Migration

- `[tool.metor.artifacts]` dissolves: each entry becomes a pack
  `pyproject.toml` in its crate dir, a mission dependency, and (while local)
  one `tool.uv.sources` line. `stubgen`'s pyproject-reading path retires with
  it.
- Mission-level `metor-fsw stubgen` becomes a one-release deprecation shim
  pointing at `pack dev`, then is removed. The `--check` byte-diff survives
  inside `pack build` determinism tests.
- The example `_backend/` and `extra-pth` die; `metor_config` arrives as a
  normal dependency. `py.rs` stops unconditionally prepending the embedded
  `metor_config`: prefer the venv's copy, keep the embedded one as the
  no-venv fallback, verify `metor_config.__version__` compatibility.
- Import churn is mechanical (`from packs.adcs import Plant` →
  `from adcs_pack import Plant`), done when converting the examples.
  `pack dev` refuses to run in a mission dir that still contains a legacy
  `packs/` package — the known `sys.path[0]` shadowing hazard.
- The OCI door stays open by construction: `prebuilt_dir` does not care who
  put the directory on disk.

## 11. Risks

Verified this design cycle: uv prepends the build env's scripts dir to the
backend subprocess `PATH` (uv build-frontend source); `cache-keys` is
per-package and applies to local directory dependencies (uv docs, and the
prototype observed it live on uv 0.5.28); pyright does not merge namespace
portions across roots (pyright #2882, pylance-release #3002); the whole
editable loop end to end (prototype findings doc).

Assumed, to be spiked in Phase 0:

- Binary-wheel scripts exposure: `metor-fsw` from `.data/scripts/` resolving
  on the backend subprocess `PATH` (mechanism is standard; the exact
  interaction is unexercised).
- Marker-dist conflict ergonomics: the `uv lock` error text mentioning
  `metor-fsw-abi` needs a docs entry translating it; if PyPI-public, register
  the dist names early.
- ~~`cache-keys` globs escaping the package dir~~ — **resolved (phase 1):
  works on uv ≥ 0.11 (verified 0.11.29: a `../contracts` edit re-runs the
  editable build, and a no-change sync stays quiet). On 0.5.x only in-dir
  globs invalidate — there a contracts-only change leaves a path-source pack
  consistently stale (module and lib regenerate together, so `StaleStubs`
  correctly stays quiet), healed by `uv sync --reinstall-package <pack>`.
  Document uv ≥ 0.11 as the supported floor for packs with out-of-dir
  dependencies.
- `cargo-zigbuild` → `aarch64-apple-darwin`. Fallbacks: mac runner, Nix cross
  shell — the builder abstraction exists precisely so this cannot block.

Operational: `pack publish` forces release+strip (a debug fat wheel is 10×);
first-`uv sync` latency on editable packs (cargo cold build with swallowed
output — the published `metor-fsw` binary removes the framework compile, the
rest is documented); Windows is out of scope.

## 12. Phasing

Each phase gets its own plan doc before implementation.

- **Phase 0 — the dists exist.** Publish `metor-config`, `metor-build`
  (still editable-only), `metor-fsw` binary wheels, and `metor-fsw-abi==8`
  to an internal index. Spike the §11 assumptions.
- **Phase 1 — IR v3 + the local prebuilt loop.** `lib` stem, `prebuilt_dir`,
  `dist` provenance; `provision_artifacts`; triple-aware cdylib naming;
  stubgen flavors and per-pack module rendering; `metor-fsw pack dev`;
  `metor_build.build_editable` → `pack dev`; recorder `Artifact`/emission
  updates; venv-`metor_config` preference in `py.rs`. Convert one example
  pack to a path-source dependency.
- **Phase 2 — the publish pipeline.** Deterministic wheel writer,
  `pack build/assemble/publish`, the three builders, N-way sidecar
  verification, ABI/`metor-config` pin injection, `build_wheel`. CI publishes
  the example packs to the internal index.
- **Phase 3 — consumption + bundles.** `package --target` prebuilt
  selection, `BundleMeta.packs` + `cdylib_sha256`; convert the examples fully
  (delete `_backend/`, `[tool.metor.artifacts]`, legacy `packs/`);
  deprecation shim on mission-level `stubgen`.
- **Phase 4 — hardening.** Registry playbooks (PyPI vs devpi vs S3 static
  index vs `--find-links` air-gap), `package --locked`, size guardrails,
  remove deprecated paths; revisit split-lib dists and OCI only if evidence
  demands.

## 13. Decisions log

Settled in design review (2026-07-16):

| decision | choice |
|---|---|
| Distribution format | Python wheels; uv is the resolver |
| Registry | any PEP 503 index; nothing metor-specific |
| Arch strategy | fat `py3-none-any` wheel, all triples; selection at consume time |
| Import naming | one top-level module per pack (`adcs_pack`); no shared namespace |
| Consumer requirements | uv + `metor-fsw` only; no cargo, no sdist ever |
| Local/published flip | `[tool.uv.sources]` path line; same import either way |
| ABI surfacing | `metor-fsw-abi` marker dist at resolve; record-time and dlopen checks as backstops |
| Cross-compilation | producer-side, pluggable builder (cargo / zigbuild / command template); Nix endorsed here only |
| Publishing unit | one pack = one crate = one dist |
| IR | v3: `lib` stem replaces host-derived `cdylib` name; `prebuilt_dir`; `dist` provenance |
| Flight provenance | `BundleMeta.packs` with `cdylib_sha256` of packaged bytes |
| Wheel reproducibility | deterministic Rust wheel writer (zip sibling of the bundle tar writer) |

Open (none blocks Phase 0):

1. `cargo-zigbuild` viability for `aarch64-apple-darwin` — leaning "use it
   for linux, mac runner for darwin" until spiked.
2. Whether `pack dev` output stays at `.metor/` or gains a configurable
   out-dir — leaning fixed convention (the host only looks one place).
3. Split-lib sibling dists for oversized packs — documented escape hatch,
   revisit on evidence.
