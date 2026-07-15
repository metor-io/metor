# Phase 4 plan — delete the KDL front-end

> **Status: PLANNED.** Revised from the design doc's §11: `metor migrate` and
> the one-release feature flag are dropped by decision (2026-07-12) — nothing
> outside the adcs example consumes this crate's KDL surface, so the front-end
> is deleted outright. The panel graph tile (design §9) proceeds as a separate
> metor-panel work stream, not in this plan.

Goal: Python is the only config front-end. `parse.rs`, `de.rs`, and every
KDL-only arm disappear; the adcs example runs entirely from `mission.py`; the
IR contract drops the `Kdl` params variant and bumps to v2.

## Milestones

### M1 — the example off KDL

1. Inventory how each tracked adcs test target obtains its wiring
   (mission.kdl parse vs builder vs TestBench) and switch any mission.kdl
   consumers to `mission.py` (via the subprocess eval path) or the Rust
   builder — whichever each test already sits closest to. The suite must be
   green with `mission.kdl` unreferenced.
2. Delete `examples/adcs-fsw2/mission.kdl` and the equivalence test (its
   purpose — proving the two front-ends agree — completes with the KDL
   front-end; if a golden-IR regression guard is still wanted, snapshot the
   emitted `wiring.json` instead and say so in the report).
3. Sweep example docs/comments that narrate the KDL surface.

### M2 — delete the front-end

1. Verify reverse deps first: `rg`/`cargo tree` for anything in the
   workspace enabling metor-fsw-2's `kdl` feature or naming its KDL paths
   (panel, cube-sat, tools). Report anything found before deleting.
2. Delete `wiring/parse.rs`, `wiring/de.rs`; shrink `wiring/kdl_params.rs`
   to the value pipeline (`encode_value_params` + `conform_to_schema` +
   `merge_onto_defaults`, renamed accordingly — e.g. `params.rs`).
3. Remove the `Kdl` arms: `ParamSource::Kdl`, `StaticParams::Kdl`,
   `EntryParams::Kdl`, their call sites, and the `kdl` crate dependency.
   Miette stays only where SourceRef-anchored rendering uses it (keep the
   anchored-error rendering; drop KDL span plumbing).
4. **Bump `IR_VERSION` to 2** (the `ParamSource` enum loses a variant — a
   wire-shape change). Old bundles fail with the existing version-mismatch
   error; that is acceptable (bundles are rebuildable).
5. Collapse features: `kdl` disappears; `wiring-model` + the wiring resolver
   become the default surface (exact feature layout at implementer's
   discretion — report it). CLI: `.kdl` missions get a clear "KDL support
   was removed; missions are Python" error, `.py` and bundles unchanged.
   Drop `BundleError::OldLayout` (no old bundles remain by decision).
6. `testbench.rs`, `cli.rs`, `registry.rs`: remove KDL-only surface
   (factory `Kdl` decode arm, `parse_with_origin`, etc.). `WiringBuilder`
   stays — it is the Rust-native front-end.

### M3 — accuracy docs sweep

Not a beautification pass — delete or retag what M2 made false:

1. `docs/wiring.md`: the KDL grammar sections go or get a one-line
   "historical, see design-kdl-serde.md" pointer; the IR/front-end sections
   update to Python + builder.
2. `docs/design-kdl-serde.md`: mark historical at the top.
3. The stale passages catalogued in Phase 0 reports while in there:
   `design-packs-authoring.md` dl-defaults bullet (fixed by WP6),
   `process-slots.md` `n_occ_*` naming (WP5), `sequences-slots.md` if it
   names parse-era details.
4. `docs/design-python-config.md` §11 phasing note: Phase 4 as executed
   (no migrate tool).

## Constraints

- Standing ground rules: tracked adcs targets only (bare `-p adcs-fsw2`
  broken by untracked scratch files); no blanket formatting; house style;
  the two pre-existing clippy warnings and no new ones.
- The Python golden/round-trip contract tests must keep passing at v2 —
  update the goldens deliberately (the diff should show only `ir_version`
  and any `ParamSource` tag change), and bump `metor_config`
  `__version__`/`EMBEDDED_METOR_CONFIG_VERSION` in lockstep since emission
  changes.
- If anything outside the example turns out to genuinely depend on KDL
  missions (M2.1), STOP and report before deleting.

## Acceptance

- `kdl` appears nowhere in metor-fsw-2's `Cargo.toml`; `parse.rs`/`de.rs`
  are gone; workspace builds.
- Full net green: `cargo test -p metor-fsw-2`, tracked adcs targets, Python
  suite; clippy adds nothing.
- `metor-fsw build|run|package examples/adcs-fsw2/mission.py` and a
  packaged-bundle `run` still work end-to-end.
- A `.kdl` path passed to the CLI produces the clear removal error.
