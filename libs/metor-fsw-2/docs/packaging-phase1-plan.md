# Phase 1 plan — IR v3 + the local prebuilt loop

> **Status: LANDED.** Implements Phase 1 of `docs/design-packaging.md`
> (§8 IR v3, §5.2/§5.3 stub flavor and naming, §6.3 the editable loop, §12
> phasing). The `sphw/py-backend` prototype (`findings-python-build-backend.md`)
> is cherry-picked onto this branch and is the starting state: mission-level
> stubgen via the in-tree `_backend`, venv-only stubs, cache-keys self-heal.

Goal: a pack crate with its own `pyproject.toml` is a real uv dependency —
`[tool.uv.sources] adcs-pack = { path = …, editable = true }` in the mission,
`uv sync` builds the host triple and renders a per-pack typed module, and
`metor-fsw run mission.py` consumes it through the IR's new prebuilt artifact
flavor. Acceptance: the adcs example runs with the `adcs` pack consumed as a
path-source dependency (`from adcs_pack import …`) while `seqs` stays on the
legacy mission-level path — proving mixed mode.

Scope discipline: no multi-triple `pack build`, no wheel writer, no
publish/assemble (Phase 2); no `package --target` prebuilt selection, no
`BundleMeta.packs`, no full example conversion or stubgen deprecation
(Phase 3). `provision_artifacts` is triple-parameterized from the start but
only exercised with the host triple this phase.

## Milestones

Four milestones, one commit each, in order.

### M1 — IR v3 + recorder emission

1. `src/ir.rs`: `Artifact.cdylib: String` (file name) → `lib: String` (stem);
   add `prebuilt_dir: Option<PathBuf>` (serde default) and
   `dist: Option<DistRef>` (`{ name, version }`, serde default);
   `IR_VERSION` 2 → 3. `path_stripped()` strips `prebuilt_dir` like `path`.
   Fix the latent host-name bug this dissolves (design §8.1).
2. `wiring::cdylib_file_name_for(triple, stem)` — `.so` / `.dylib` / `.dll`
   by triple; `cdylib_file_name(stem)` delegates with the host.
3. Recorder (`python/metor_config/__init__.py`): `Artifact` dataclass gains
   `prebuilt`, `abi_version`, `dist`, `dist_version` (all optional); IR
   emission writes `lib` (stem) instead of the host-derived `cdylib`;
   `_cdylib_file_name` is deleted. Emit `prebuilt_dir`/`dist` when the
   artifact carries them.
4. Tests: IR JSON round-trip snapshot updated for v3; recorder emission
   tests; a v2-IR ingest fails loudly naming both versions (existing check).

### M2 — provisioning

1. `build_driver`: `build_artifacts` → `provision_artifacts(wiring, opts)`.
   Per artifact: `prebuilt_dir` set → select
   `<dir>/<triple>/<cdylib_file_name_for(triple, lib)>`, fill `path`, no
   cargo; a missing triple errors listing the triples present. Else the cargo
   path unchanged. No hash check at selection: the sidecar sits next to the
   selected library, so resolve's existing `check_manifest_hashes` gate
   already covers prebuilt artifacts — one validation gate, not two.
2. `host_triple()` compile-time fallback (`env!`-style per-platform constant)
   so prebuilt-only consumers need no cargo on PATH.
3. Bundle member naming derives from `lib` + build target (was
   `artifact.cdylib`).
4. Tests: prebuilt fixture dir (host triple) resolves and runs; missing
   triple and sidecar-hash mismatch error shapes.

### M3 — `[tool.metor.pack]`, stubgen flavors, `pack dev`

1. New pack-config parsing (`src/wiring/pack.rs`): `[project]` +
   `[tool.metor.pack]` with `Cargo.toml` defaults for `crate`/`lib`, module
   name defaulting to the normalized dist name.
2. Stubgen: `Flavor { Local, Prebuilt { prebuilt, abi, dist } }` controls the
   `ARTIFACT` constant and header; new per-pack rendering emits
   `<module>/__init__.py` + `py.typed`. Legacy mission-level `packs/<id>.py`
   rendering unchanged.
3. `metor-fsw pack dev [DIR]`: build host triple (existing driver), write
   sidecar, render prebuilt-flavor module into
   `<dir>/.metor/{<module>/, libs/<host-triple>/…}`. Refuses to run in a dir
   that still has a legacy `packs/` package (shadowing hazard).
4. Tests: `pack dev` round-trip determinism; config-default derivation from
   `Cargo.toml`.

### M4 — backend promotion + mixed-mode example

1. Promote the pack-oriented backend to `python/metor_build/` (the mission's
   in-tree `_backend` stays for the legacy `seqs` path until phase 3);
   `build_editable` calls `metor-fsw pack dev`; binary resolution
   `METOR_FSW_BIN` → PATH → cargo, with a `pack --help` capability handshake
   (a stale PATH binary predating the subcommand falls through to cargo; an
   explicit `METOR_FSW_BIN` that cannot serve errors loudly — hit for real
   during verification). The dist `pyproject.toml` for `metor-build` is
   deferred to phase 0 with publishing; in-repo packs reach the backend via
   a one-file `_backend/metor_build_shim.py` (PEP 517 `backend-path` must
   stay inside the source tree).
2. `py.rs`: prefer the venv's `metor_config` over the embedded copy (embedded
   stays the no-venv fallback); pass the expected ABI version into the eval
   env; recorder checks `ARTIFACT.abi_version` against it at record time.
3. Example: `examples/adcs-fsw2/systems/` gets a pack `pyproject.toml`
   (dist `adcs-pack`, module `adcs_pack`); mission depends on it via
   `tool.uv.sources`; `mission.py` imports flip to `from adcs_pack import …`
   for adcs only; `seqs` stays on `[tool.metor.artifacts]` + mission-level
   stubgen (mixed mode is the acceptance test).
4. Tests: example suites pass hermetically (no venv) and via `uv sync` +
   `uv run`; pyright clean on `mission.py`.

## Findings (phase 1 verification)

- uv 0.5.28 `cache-keys`: in-dir globs invalidate the pack's editable build;
  `../` globs do **not** (phase-0 spike question answered early). A
  contracts-only change leaves a path-source pack consistently stale —
  module and lib regenerate together, so `StaleStubs` correctly stays
  quiet — healed by `uv sync --reinstall-package <pack>` or any in-pack
  edit.
- `metor-fsw run mission.py --build` no longer cargo-rebuilds a prebuilt
  artifact (provisioning selects from `.metor`); `uv run`'s implicit sync is
  the blessed refresh. Revisit in phase 3 whether provisioning should re-run
  `pack dev` for path-source packs itself.

## Verification

- `cargo test -p metor-fsw-2` and the example suites, hermetic (no venv).
- In `examples/adcs-fsw2`: `uv sync`, `uv run pyright`, `uv run metor-fsw run
  mission.py --build --cycles 60`.
- Edit a pack param doc → `uv sync` regenerates the module; revert →
  byte-identical (determinism).
