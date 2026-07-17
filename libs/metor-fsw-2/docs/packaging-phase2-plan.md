# Phase 2 plan — the publish pipeline

> **Status: LANDED.** Implements Phase 2 of `docs/design-packaging.md`
> (§5 the pack wheel, §7 publishing), after phases 0 and 1
> (`packaging-phase0-plan.md`, `packaging-phase1-plan.md`): the local
> prebuilt loop and the toolchain dists exist; this phase makes a pack
> *publishable* — one fat wheel carrying every supported triple.

Goal: `metor-fsw pack build --wheel-out` produces a deterministic
`<dist>-<ver>-py3-none-any.whl` containing the typed module and
`_libs/<triple>/` payloads for every configured target; a CI matrix can
split the triple builds across runners and join them with `pack assemble`;
`pack publish` hands the wheel to `uv publish`. Acceptance: the fixture
pack's wheel, built twice, is byte-identical; installed into a scratch
venv, its module imports and its host-triple lib provisions and resolves.

Scope discipline: no `package --target` prebuilt selection, no
`BundleMeta.packs`, no example conversion (phase 3); no registry playbooks
or size guardrails (phase 4). Cross toolchains are not assumed present —
tests drive the multi-triple paths with the host triple and fabricated
layouts; real cross builds are exercised by whoever runs the matrix.

## Milestones

### M1 — deterministic wheel writer (`wiring/wheel.rs`)

The zip sibling of `bundle.rs`'s reproducible tar: entries sorted by
archive name, timestamps fixed at the DOS epoch, modes in the external
attributes, **stored** (uncompressed) entries so bytes are deterministic by
construction and no compressor enters the tree (`crc32fast` is already in
the lock for the CRC field; cdylibs are a few MB — size is a phase-4
guardrail). Emits `METADATA` (name, version, `Requires-Python`,
`Requires-Dist` lines), `WHEEL` (`py3-none-any`, `Root-Is-Purelib: true`),
and a sha256 `RECORD`. Unit tests: byte-identical double-write, RECORD
digests, mode bits.

### M2 — `pack build` (multi-triple) + builder plugins

1. Builders, `[tool.metor.pack.builder]` (CLI-overridable): `cargo`
   (native `--target`), `zigbuild` (`cargo zigbuild --target`), `command`
   (argv template with `{triple}`/`{crate}` substitution — the Nix
   cross-devshell hook). Wheel builds force `--release` and pass
   `--config profile.release.strip=true` to the cargo-family builders.
2. `pack_build(dir, opts)`: per configured target (or `--target`
   overrides), build via the builder and stage
   `<staging>/<triple>/{cdylib, sidecar}`; the sidecar comes from the
   existing host-twin describe. Then the N-way identity gate: every
   triple's sidecar must be byte-identical (the `ManifestDivergence`
   philosophy generalized), else a hard error naming the divergent triple.
3. Wheel assembly: render the prebuilt-flavor module from the one
   manifest, inject `Requires-Dist: metor-fsw-abi==<FSW_ABI_VERSION>` and
   `Requires-Dist: metor-config>=<minor>,<<next-minor>` (from the embedded
   recorder's `__version__` — phase 0 finding 4 showed the pin is
   load-bearing) plus the pack's own `[project] dependencies`, and write
   `{<module>/__init__.py, py.typed, _libs/<triple>/…}` via M1.
4. CLI: `pack build [DIR] [--target T]... [--libs-out DIR | --wheel-out DIR]`
   — `--libs-out` stages one runner's triples without assembling (the
   matrix half), `--wheel-out` assembles.

### M3 — `pack assemble` + `pack publish` + backend `build_wheel`

1. `pack assemble [DIR] --libs DIR... --wheel-out DIR`: collect
   `<libs>/<triple>/` stagings from matrix runners, re-run the N-way
   sidecar gate across everything collected, assemble as in M2.3.
2. `pack publish [DIR] [--index URL] [--wheel FILE] [--dry-run]`: build
   (or take) the wheel, shell to `uv publish`; `--dry-run` prints the
   command and the wheel's contents summary.
3. `metor_build.build_wheel` shells to `metor-fsw pack build --wheel-out`,
   so `uv build` works on a pack dir (sdists stay refused).

### M4 — end-to-end verification

Fixture-pack wheel (host triple): built twice byte-identical; unzipped
layout matches `pack dev`'s modulo the `dist` stamp; installed into a
scratch venv from `--find-links` alongside the phase-0 dists, `import`
works and a mission consuming it provisions the host lib and resolves.
Findings recorded here.

## Verification

- `cargo test -p metor-fsw-2` (wheel writer units, pack_build/assemble
  integration over the fixture, divergence error shapes).
- The M4 scratch-venv flow, reproduced from this doc.
- Example suites stay green (no example changes this phase).

## Findings

- The fixture wheel is byte-reproducible, and the one-shot
  (`--wheel-out`) and staged-then-assembled (`--libs-out` +
  `pack assemble`) flows produce identical bytes. Python's `zipfile`
  verifies integrity, entry order, modes, DOS-epoch timestamps, and RECORD
  digests.
- The real `adcs-pack` wheel (host triple) installed into a scratch
  consumer from `--find-links`: the injected `metor-config>=0.3,<0.4` pin
  pulled the recorder transitively (the consumer declared only
  `adcs-pack`), `ARTIFACT.prebuilt` pointed into the venv's `_libs`, and
  `metor-fsw build`/`run` provisioned, manifest-checked, dlopened, and
  described the wheel's payload with no cargo for the pack. The toy
  mission then failed graph validation on an unconnected `mode_cmd` —
  correct, its producer lives in the seqs slot — which is *past* every
  packaging layer; full run-parity is covered by the example suites, whose
  `pack dev` layout is byte-identical in shape to the wheel install.
- `uv publish` exposure is `pack publish --dry-run`-verified only; a real
  upload needs an index and credentials (phase 4's registry playbooks).
- Cross builds (zigbuild, command/Nix) are wired but not exercised here —
  no cross toolchains on this machine; the CI matrix + `pack assemble`
  path is the intended venue, with the N-way sidecar gate as the
  correctness backstop (unit-tested via fabricated skew).
