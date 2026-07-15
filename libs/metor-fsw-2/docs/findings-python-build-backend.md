# Findings: pack stubs via a Python build backend

Prototype landed 2026-07-15 in `examples/adcs-fsw2`. Verdict: **yes** — a PEP
517/660 build backend makes stub generation a standard-tooling concern. `uv
sync` (and even a plain `uv run`) regenerates the typed `packs/<id>.py`
modules whenever the pack sources change, pyright resolves them from the
venv, and nothing generated is checked in anymore.

## What was built

- `examples/adcs-fsw2/_backend/metor_build/` — an in-tree, stdlib-only PEP
  517/660 backend (~180 lines: `__init__.py` hooks + `_wheel.py` editable
  wheel writer). `build_editable` shells out to `metor-fsw stubgen … --out-dir
  .metor/packs` and installs a wheel whose only payload is a `.pth` exposing
  `.metor` (the generated `packs` package) and `libs/metor-fsw-2/python`
  (`metor_config`) to the venv.
- `StubgenOptions.out_dir` / `metor-fsw stubgen --out-dir <DIR>` — stubgen
  can now write the `packs` package outside the mission source tree.
- `examples/adcs-fsw2/pyproject.toml` — grew `[project]`, `[build-system]`
  (`backend-path = ["_backend"]`, `requires = []`), `[tool.metor.build]`
  (out-dir, extra-pth, stubgen-command override), and `[tool.uv] cache-keys`
  globs over `systems/**` and `contracts/**`.
- The checked-in `examples/adcs-fsw2/packs/` is deleted; `.metor` is
  gitignored; `pyrightconfig.json` pins the checker to `.venv`.

## The mechanism, end to end

1. `uv sync` sees the project (or a changed cache key) and calls
   `metor_build.build_editable`.
2. The backend runs `cargo run -q -p metor-fsw-2 --bin metor-fsw -- stubgen
   <root> --out-dir <root>/.metor/packs` (override: `METOR_FSW_BIN`, or
   `[tool.metor.build] stubgen-command`). Stubgen cargo-builds the pack
   cdylibs, reads their manifest sidecars, and renders the modules.
3. The editable wheel installs `_adcs_fsw2_editable.pth` — two absolute path
   lines. Both CPython and pyright follow static `.pth` lines, so
   `from packs.adcs import Plant` and `import metor_config` resolve from the
   venv for the interpreter, the IDE, and `metor-fsw`'s mission eval
   (`uv run` sets `VIRTUAL_ENV`, which `resolve_interpreter` already prefers).
   Independently of the venv, `eval_python_mission` now also puts a
   mission-adjacent `.metor` on the subprocess `PYTHONPATH`, so a bare
   `metor-fsw run` or `cargo test` (no venv) evaluates the mission once
   stubs exist — the tracked example suites regenerate them themselves via
   `tests/common/mod.rs::ensure_stubs`.
4. `[tool.uv] cache-keys` file globs invalidate the cached build when any
   pack/contract source changes, so the next `uv sync` — or implicit sync of
   a plain `uv run` — regenerates the stubs.
5. The resolve-time manifest-hash check is unchanged and remains the
   last-line backstop: a stub generated, then the pack edited and run
   *without* a sync (`uv run --no-sync`), is refused with `StaleStubs`.

## Verified (all on uv 0.5.28, macOS)

- `uv sync -v` → backend runs cargo, writes `.metor/packs/{__init__.py,
  py.typed,adcs.py,seqs.py}`, installs the `.pth`; `uv run python -c "import
  packs.adcs, metor_config"` resolves both from the venv.
- `uv run --with pyright pyright` → 0 errors on `mission.py` with no source
  fallback (checked-in `packs/` deleted first — it would shadow the venv
  copy via `sys.path[0]`).
- Regen loop: editing a params doc comment in `contracts/` and `uv sync`
  regenerated a changed `adcs.py`; reverting and re-syncing reproduced the
  original **byte-identical** (generation is deterministic).
- `uv run -- … run mission.py --build --cycles 60` → resolve accepts the
  venv stubs, sim runs clean.
- Staleness: source edit + `uv run --no-sync … --build` → `StaleStubs`
  refusal; a plain `uv run` heals it (implicit sync) before the run even
  starts. The error hint now names `uv sync` first.
- Hermetic tests: with `.metor` deleted and no venv anywhere, `cargo test -p
  metor-fsw-2 --lib` (232) and all seven example suites pass — the suites
  regenerate stubs via `ensure_stubs` and the evaluator finds them through
  the `.metor` `PYTHONPATH` fallback.

## What PEP 517/660 forced

- **`backend-path` must stay inside the source tree** (pyproject_hooks and
  uv both enforce it), so the prototype backend lives under the example, not
  next to `metor_config`. It reads everything from `[project]` +
  `[tool.metor.build]` with zero example-specific hardcoding, so
  productization is "publish it as a `metor-build` dist and drop the
  `backend-path` line", not a rewrite.
- **Editable-only**: `build_wheel`/`build_sdist` raise `NotImplementedError`
  with a clear message. A non-editable mission wheel has no consumer today;
  defining one (bundle-shaped? stubs-only?) is a productization question.
- `prepare_metadata_for_build_editable` is implemented without running
  cargo, so metadata probing stays cheap for frontends that call it.

## uv specifics observed

- `cache-keys` file globs work on uv 0.5.28 and apply to the root project's
  editable rebuild — the prototype's central bet, confirmed.
- `uv run` performs an implicit sync, which turns "forgot to regenerate"
  into a non-event; `--no-sync` is the only way to race the backstop.
- uv swallows backend stdout unless the build fails or `-v` is passed; the
  first sync compiles `metor-fsw` + packs (~1 min warm, minutes cold) with
  no progress shown. Documented in the example README; `METOR_FSW_BIN`
  skips the framework compile (the CI-friendly path).
- pip compatibility is by construction (standard hooks, static `.pth`,
  `requires = []`) but was not exercised; without cache-keys pip users
  rerun `pip install -e .` by hand after pack changes.

## Productization checklist (follow-up arc)

- Package `metor-build` and `metor-config` as real distributions (publish or
  `tool.uv.sources`); missions then declare `build-backend = "metor_build"`
  with `requires = ["metor-build"]` and depend on `metor-config` instead of
  the `extra-pth` checkout line.
- Teach `py.rs` to prefer a venv-provided `metor_config` over the embedded
  copy (today the embedded copy shadows it via `PYTHONPATH` prepend —
  harmless while identical, drift-prone once versions can differ). The
  `.metor` `PYTHONPATH` fallback is convention-based (`out-dir` is
  configurable but the host only looks for the default); read it from
  `[tool.metor.build]` if anyone changes it.
- Decide non-editable `build_wheel`/`build_sdist` semantics.
- CI: `uv sync` + `uv run pyright` in the example replaces the deleted
  `stubgen --check` byte-diff gate (the round-trip determinism test in
  `tests/stubgen.rs` keeps `--check` itself honest).
- Windows untested (the `.pth`/wheel writer is portable; the cargo/cdylib
  story is the open half).

## Open questions

- Multi-mission workspaces: one venv per mission dir, or a uv workspace with
  shared build? cache-keys are per-project, so per-mission is the natural
  first cut.
- Lockfile hygiene: `uv.lock` in a mission dir is meaningful (it pins
  nothing today — no dependencies) but will matter once `metor-config` is a
  real dependency; decide whether examples commit it.
- Whether `metor-fsw stubgen` keeps its in-source-tree default output once
  all missions are backend-driven, or flips to `.metor/packs` everywhere.
