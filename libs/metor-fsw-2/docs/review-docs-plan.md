# Doc-comment review: remove work-package / changelog framing

## Goal
Rewrite doc comments (`///`, `//!`) and `Cargo.toml` comments so each describes the
CURRENT state of the code in present tense, with no references a fresh reader can't
resolve: work-packages (WP3/4/5/6/7/8, "Wave 2/3a/3b/4"), `dl-open.md §X` justification
pointers, and temporal/changelog framing ("now", "no longer", "previously", "used to",
"as of", "pre-existing", "the landed", "the old", "Today's seam", "the fix").

## Rules
- Drop the framing, keep all technical substance, invariants, `# Safety`, examples.
- Strip every `dl-open.md §X` pointer (flagged by the verify grep). KEEP the genuine
  "see the design doc" pointers to the OTHER design docs (frames.md/system.md/
  wiring.md/telemetry.md/coordinator.md §X) — those are architecture pointers, not
  changelog justifications, and are not flagged.
- Leave legitimate present-tense "now" (e.g. "writing now would overwrite").
- Do NOT touch code, `#[cfg]`, names, logic.

## Files to edit
- Cargo.toml (dep-comment WP/Wave/§ framing; feature comment)
- src/lib.rs (module-map WP comments)
- src/{dl,abi/mod,descriptor,binder,system/mod,port,reader,dynamic,writer,health}.rs
- src/wiring/{mod,model,builder,build_driver}.rs
- src/coordinator/mod.rs
- src/tests.rs, src/{system,telemetry,coordinator,wiring,abi}/tests.rs
- ring/src/lib.rs, ring/src/tests.rs
- tests/{dl_integration,wiring_resolve}.rs
- tests/fixtures/dl-fixture/{src/lib.rs,Cargo.toml}

## Verify
cargo check --all-features && --no-default-features; cargo test --all-features;
cargo doc --all-features --no-deps; final grep for WP/Wave/no longer/pre-existing/
dl-open.md §/as of/used to/previously.
