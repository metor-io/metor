# Phase 0 plan — the toolchain dists exist

> **Status: LANDED.** Implements Phase 0 of `docs/design-packaging.md`
> (§4 published distributions, §12 phasing), executed after Phase 1
> (`packaging-phase1-plan.md`) — the local prebuilt loop exists; this phase
> gives the toolchain itself distribution form and closes the remaining
> spike questions. Hosting to a real index stays out of scope: the design is
> registry-agnostic, and a local `--find-links` directory proves every
> mechanic an index would.

Goal: the four toolchain dists are real, buildable projects, and a consumer
outside the workspace can `uv sync` a mission whose pack backend finds a
**pinned, packaged** `metor-fsw` — never an ambient `~/.cargo/bin` /
`~/.local/bin` binary. Decision from design review: the binary ships as
**package data with an importable locator** (`metor_fsw.find()`, the
ziglang-wheel pattern) plus a console-script shim for humans; in-process
PyO3 was rejected because the describe machinery re-executes
`current_exe()` as a worker, which breaks under a Python host.

## Milestones

### M1 — per-dist project layout under `python/`

```
python/
  metor-config/  pyproject.toml + metor_config/   (recorder, py3-none-any)
  metor-build/   pyproject.toml + metor_build/    (PEP 517 backend, py3-none-any)
  metor-fsw/     pyproject.toml + metor_fsw/ + _backend/   (M2)
  metor-fsw-abi/ pyproject.toml + metor_fsw_abi/  (M2)
  tests/         (recorder tests, path fixups only)
```

Pure dists use `uv_build` with a flat module root. Path fixups: the
embedded-recorder `include_str!` in `wiring/py.rs`, the example mission's
`extra-pth`, the pack shim's `sys.path` insert, the Python tests'
`sys.path` inserts. The example mission additionally gains
`metor-config` as a real (editable path-source) dependency — the
venv-preferred `metor_config` path landed in Phase 1 does the rest; the
embedded copy stays as the no-venv fallback.

### M2 — the `metor-fsw` binary dist and the ABI marker

- `metor-fsw-abi`: empty marker package, version == `FSW_ABI_VERSION`
  (currently `8`).
- `metor-fsw`: `metor_fsw/find()` returns the packaged binary
  (`metor_fsw/bin/metor-fsw`, mode preserved via zip attrs);
  `main()` execs it (`[console_scripts] metor-fsw = metor_fsw:main`);
  `[project] dependencies = ["metor-fsw-abi==8"]` — the resolver tier of
  the ABI gate. Version == the crate version.
- In-tree stdlib backend (`python/metor-fsw/_backend/`): `build_wheel`
  cargo-builds the release binary and writes a **platform-tagged** wheel
  (`py3-none-macosx_*_arm64` / `py3-none-linux_*`; manylinux is a phase-2
  concern with the zigbuild matrix). The generic wheel writer generalizes
  `metor_build._wheel` (files + modes + entry points + platform tag), which
  the backend reaches through the sibling-path shim trick, same as packs.
- Drift guards in Rust tests: the dist's pyproject version ==
  `CARGO_PKG_VERSION`, its `metor-fsw-abi` pin == `FSW_ABI_VERSION`, and
  the marker dist's version == `FSW_ABI_VERSION`.

### M3 — `metor_build` prefers the packaged binary

Resolution order becomes: `pack-dev-command` override → `$METOR_FSW_BIN`
(hard error if it cannot serve) → **`metor_fsw.find()`** (present exactly
when the pack declared `requires = [..., "metor-fsw"]` and the frontend
installed it into the isolated build env — the pinned path) → `PATH` with
the `pack --help` capability handshake → cargo (monorepo).

### M4 — spikes against a local find-links index

Build the four wheels into a scratch `dist/` and, from a consumer project
*outside* the workspace with a toy pack:

1. **Pinned-binary proof**: toy pack `requires = ["metor-build",
   "metor-fsw"]`, `uv sync` with cargo absent from `PATH` and a decoy stale
   `metor-fsw` on `PATH` — the build must use the packaged binary via
   `find()` and succeed.
2. **ABI conflict ergonomics**: a toy dist pinning `metor-fsw-abi==7`
   alongside `metor-fsw` (pinning `==8`) must fail inside `uv lock`;
   capture the message for the docs.
3. **Build-requires + sources**: whether `[tool.uv.sources]` satisfies a
   pack's `build-system.requires` from path sources (would let in-repo
   packs drop the `_backend` shim). Document the verdict either way.

Findings land in this doc; the design doc's §11 ledger gets updated.

## Findings (all on uv 0.11.29, macOS)

1. **Pinned-binary proof: PASS.** A toy pack outside the workspace with
   `requires = ["metor-build", "metor-fsw"]` (both from a flat find-links
   index, no shim, no `backend-path`) built through the packaged binary via
   `metor_fsw.find()`. A decoy stale `metor-fsw` first on `PATH` — crafted
   to *pass* the capability handshake and record any invocation — was never
   touched. The wheel's package-data binary keeps its exec bit (zip external
   attrs honored by uv), and the console script runs it.
2. **ABI conflict ergonomics: PASS.** With both marker versions on the
   index, `uv lock` fails with: *"Because all versions of metor-fsw depend
   on metor-fsw-abi==8 and toy-abi7 depends on metor-fsw-abi==7, we can
   conclude that all versions of metor-fsw and toy-abi7 are incompatible."*
   Legible, names both parties and both pins, fails before anything runs.
   (With only one version published the message degrades to "no version of
   metor-fsw-abi==7" — publish the marker for every ABI that shipped.)
3. **Build-requires + sources: SUPPORTED.** uv applies the pack's own
   `[tool.uv.sources]` to its `build-system.requires`, so in-repo packs
   declare `requires = ["metor-build"]` with a path source — the
   `_backend/metor_build_shim.py` is deleted. Caveat: build-requirement
   wheels are cached; after editing `metor_build` itself, heal with
   `uv cache clean metor-build`.
4. The generated module imports `metor_config`, so a pack wheel without a
   `metor-config` runtime pin fails at import in a fresh consumer — the
   phase-2 assembly's pin injection is load-bearing, not cosmetic (the toy
   pack had to declare it by hand).
5. In-repo packs deliberately do **not** put `metor-fsw` in `requires`: the
   binary wheel is release-built and cached against the dist's pyproject
   only, so it would go stale against the checkout; the backend's cargo
   fallback stays the monorepo path.

## Non-goals

- Real index hosting and `uv publish` (phase 2 `pack publish` wraps it;
  registry choice is operational).
- The multi-triple pack wheel writer and manylinux tags (phase 2).
- Converting the example mission's `metor-fsw` usage away from cargo — a
  path-source binary wheel would go stale against the checkout (its
  cache-keys cannot see workspace-wide Rust changes); monorepo dev keeps
  the cargo fallback by design.

## Verification

- `uv build` succeeds for all four dists; `uv sync` in the example still
  works end to end (pyright clean, mission runs) after the layout move.
- All Phase 1 suites stay green (`cargo test -p metor-fsw-2`, example
  suites hermetic from a clean `.metor`).
- The M4 spike script's three outcomes, reproduced from the plan doc.
