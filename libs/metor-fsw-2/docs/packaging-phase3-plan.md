# Phase 3 plan — consumption, bundle provenance, full conversion

> **Status: LANDED.** Implements Phase 3 of `docs/design-packaging.md`
> (§8.2 provisioning, §9.2 flight provenance, §10 migration), after phases
> 0–2: packs build, publish, and provision; this phase finishes the
> consumer story — first-class cross-target packaging, a self-describing
> flight record, and the example converted off every legacy path.

Goal: `metor-fsw package --target <t>` selects prebuilt payloads for the
flight triple with no cargo for published packs; `meta.json` records what
was actually packaged (dist, source kind, the exact cdylib bytes' sha256);
and `examples/adcs-fsw2` consumes **both** packs as ordinary dependencies —
no `[tool.metor.artifacts]`, no mission `[build-system]`, no `_backend/`.
Mission-level `stubgen` becomes a deprecation shim.

## Milestones

### M1 — `package --target` + `BundleMeta.packs`

1. CLI: `package` (and `build`/`run --build`) gain `--target <TRIPLE>`,
   sugar for the `--cargo-arg --target …` spelling that also drives
   prebuilt selection and the bundle's recorded triple. Prebuilt-only
   missions package cargo-free.
2. `BundleMeta` gains `packs: Vec<PackProvenance>` (serde-defaulted; old
   bundles load unchanged): `artifact_id`, `dist: Option<DistRef>`,
   `source: Prebuilt | CrateBuilt`, `cdylib_sha256` hashed from the bytes
   actually copied into the bundle, and the recorded `manifest_hash`.
   Recording only — the load-time gates (ABI, IR hash, target triple,
   manifest hash) already cover integrity; this is the flight record that
   outlives the venv (design §9.2).
3. Tests: provenance recorded for prebuilt and crate-built artifacts; an
   old `meta.json` without `packs` still loads.

### M2 — seqs becomes a pack; the example fully converts

1. `examples/adcs-fsw2/systems/adcs-sequences/pyproject.toml`: dist
   `adcs-seqs`, module `adcs_seqs`, id `seqs`, same backend shape as the
   adcs pack (requires `metor-build` via `[tool.uv.sources]`).
2. Mission: `adcs-seqs` joins `dependencies` + sources;
   `[tool.metor.artifacts]`, `[tool.metor.build]`, `[build-system]`, and
   `_backend/` are deleted — the mission is a virtual uv project, exactly
   the design's consumer shape. `mission.py` flips to
   `from adcs_seqs import commissioning, safe_mode`.
3. `tests/common::ensure_stubs` runs `pack_dev` for both packs (no more
   mission-level stubgen); suites and README updated.

### M3 — mission-level `stubgen` deprecation shim

`metor-fsw stubgen` keeps working for one release but warns on stderr,
pointing at `pack dev`. The `--check` byte-diff contract survives via the
stubgen round-trip test over a fabricated mission dir (the example no
longer has an artifacts table to exercise it).

## Verification

- Example: `uv sync` → pyright clean → `uv run … run mission.py --build
  --cycles 60` → all seven suites hermetic from deleted `.metor` dirs.
- `package -o … --target <host>` over the converted mission: bundle loads,
  `meta.json` carries both packs' provenance with `source: Prebuilt`.
- `cargo test -p metor-fsw-2` green; old-bundle compatibility test.

## Findings

- All of the above verified. `package --target <host>` over the converted
  mission ran **cargo-free for both artifacts** (prebuilt selection from
  the venv payloads) and recorded `Prebuilt` + dist identity + cdylib
  sha256 for each.
- `run` deliberately did not gain `--target`: a foreign-triple bundle
  cannot execute on this host, and `build`/`package` cover every cross
  use.
- The mission uninstalling itself on the conversion sync (`- adcs-fsw2`)
  is uv confirming the virtual-project shape — the design's intended
  consumer state.
