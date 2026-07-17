# metor-fsw-2 Code Style and Simplification Review

## Summary

The code is generally disciplined Rust. Clippy reports only one production
warning, naming is consistent, state is usually explicit, and unsafe blocks are
well justified. The main problem is not local syntax. It is accumulated
explanation and parallel framework surfaces.

Production code is about 26,000 lines across 62 Rust files. Approximately
7,000 lines (27%) are comments, including 5,900 lines of rustdoc. There are 124
separator-banner lines and 289 public items. Several central modules exceed
1,000 lines. The repository can become materially shorter without making the
unsafe or state-machine code terse.

## Implementation Status

The bounded simplifications from this review have been implemented:

- `WireError` now derives `thiserror::Error`; the manual `Display` and `Error`
  implementations are gone.
- `WiringBuilder` uses one `SystemSpecBuilder`, and slot occupant construction
  goes through one private helper.
- `PortDecl` and `split_decls` were replaced by `Declarations { ports,
  capabilities }`. Port and capability fields now contribute their native
  types directly.
- Stub generation shares keyword parameter and argument renderers using
  `std::fmt::Write`.
- The production Clippy warning and the two test-only warnings were fixed.
- Several redundant and historical comments adjacent to the changed code were
  removed or shortened.

The driver-pipeline convergence and process-platform isolation remain design
work. Both affect lifecycle and unsupported-platform behavior across static,
dynamic, process, and slot systems; they should be implemented as dedicated
changes after the async lifecycle contract described in `CODE_REVIEW.md` is
settled. Large module and test splits are also deferred until those internal
boundaries are defined, to avoid reorganizing code around models that are
about to change.

## Findings

### 1. Comments routinely explain what the next statement already says

The comment density is justified in `ring/src/lib.rs`, `src/abi/mod.rs`, and
the foreign-library parts of `src/dl.rs`, where safety arguments and protocol
invariants are part of correctness. Elsewhere, many comments narrate mechanics:

- `src/coordinator/bind.rs:403-451` labels each local immediately before its
  descriptive constructor call.
- `src/telemetry/mod.rs:832-857` explains borrow ordering and then expresses it
  directly with deferred vectors.
- `src/port.rs:261-262` says the scratch buffer is retained immediately before
  assigning it to `self.scratch`.
- `src/message.rs:223-226` repeats the same single-writer explanation used by
  `Output::bind`.
- Section banners consume two or three lines where a short module, function,
  or no marker would be clearer.

Historical language also remains in normative code: “WP3”, “Phase 1”, “v1”,
“interim”, and repeated “twin of” descriptions. Examples include
`src/coordinator/mod.rs:853-856`, `src/wiring/stubgen.rs:1-24`,
`src/coordinator/init.rs:848`, and `src/port.rs:74-77`. These references require
knowledge of old plans and make current code harder to read.

Recommendation: keep comments only when they state a reason, invariant,
ownership/lifetime fact, surprising policy, or safety proof. Delete narration,
change-history prose, and banners. Public rustdoc should describe the contract,
not the implementation tour. A conservative pass could remove 1,500-2,000
comment lines while leaving unsafe documentation untouched.

### 2. The framework has too many system-authoring and execution surfaces

There are three advertised authoring styles (function, async function, and
struct), plus separate static, pack, dlopen, process, and slot mounting paths.
They converge late through several adapters:

- `CyclicRunner` in `src/system/mod.rs:356-447`
- `FnDriver`, `FutureDriver`, `OccupantFuture`, and `OccupantCyclic` in
  `src/handler/driver.rs`
- `DriverSlot` in `src/pack.rs:470-506`
- `DlSlot` in `src/dl.rs:541-696`
- process slot adapters in `src/proc/host.rs`

This creates repeated init/step/shutdown, health timing, drop folding, terminal
state, and occupant-tail logic. `Pack::task` manually assembles a descriptor,
constructor, clock, drop counter, health tail, and mount wrapper
(`src/pack.rs:283-344`) instead of using the same entry factory as other
systems.

Recommendation: choose one internal `EntryFactory -> Pending -> Driver`
pipeline. All authoring styles should lower into it before registration. Put
health accounting and terminal-state behavior in one driver decorator, with
mounting as another decorator. Keep multiple ergonomic public authoring styles
only if they compile into the same small internal representation.

This is the highest-payoff structural simplification, but it should follow a
decision on the async lifecycle contract from `CODE_REVIEW.md`.

### 3. `WiringBuilder` uses three sub-builders where one data builder is enough

`SystemSpecBuilder` and `ArtifactSystemBuilder` duplicate `name`, `ty`,
`params`, `params_value`, construction of `SystemSpec`, validation, and `end`
(`src/wiring/builder.rs:393-526`). The split encodes “static versus artifact” in
the builder type, but the final `SystemSpec` already represents that choice and
the shared validator already checks its legality. `SlotSpecBuilder` similarly
has four `allow_*` methods that each build nearly the same
`AllowedOccupantSpec` (`src/wiring/builder.rs:278-346`).

Recommendation: use one `SystemBuilder { parent, spec: SystemSpec }` with
`artifact(...)`, `ty(...)`, `params(...)`, `params_value(...)`, and `process()`.
Validate the completed spec in one `end`. Give `SlotSpecBuilder` one private
`push_allowed` helper, or accept an `AllowedOccupantSpec` through `allow` and
make convenience methods thin wrappers.

This trades a small amount of compile-time sequencing for substantially less
implementation and documentation. The validator remains the authoritative
contract for both serialized IR and Rust-built IR.

### 4. Capability declarations masquerade as ports

`SystemInput::decls` and `SystemOutput::decls` return `Vec<PortDecl>`, where
`PortDecl` is either a real `PortDesc` or a `Capability`
(`src/descriptor.rs:157-205`). Every descriptor build must call `split_decls`,
and binding code must preserve the special rule that capabilities consume no
cursor. Tests explicitly protect that incidental representation.

This is less idiomatic than returning the two concepts as two fields. It makes
the port list heterogeneous even though every later stage immediately splits
it.

Recommendation: return a `Declarations { ports: Vec<PortDesc>, capabilities:
Vec<Capability> }`. Derives can append to the correct field directly. This
removes `PortDecl`, `split_decls`, and capability-specific bind-order reasoning.
It also makes invalid states harder to represent without adding abstraction.

### 5. Platform configuration is scattered through core slot and resolver code

There are 98 `cfg`/`cfg_attr` sites in production `src`, with process-platform
conditions repeated throughout `coordinator/slot.rs`, `coordinator/bind.rs`,
`wiring/resolve.rs`, `dl.rs`, and `proc/mod.rs`. `coordinator/slot.rs` alone
interleaves platform branches through its state machine, making the common
in-process path harder to follow.

Recommendation: expose one platform-neutral process backend from `proc`, with
supported and unsupported implementations selected inside that module. Core
coordinator code should call the same functions on every target and receive a
normal `Unsupported` error from the stub backend. Keep feature gates at module
boundaries instead of inside state transitions.

This will remove duplicated unsupported-target functions and reduce the number
of states a reader must simulate while reading one platform's code.

### 6. Python stub generation repeats low-level string assembly

`src/wiring/stubgen.rs` mixes artifact discovery, file freshness, manifest
loading, schema-to-Python translation, source rendering, and integration tests
in 1,334 lines. Within rendering, `init_signature` and `occupant_signature` are
the same parameter renderer with different indentation and prefixes;
`super_init` and `occupant_body` similarly duplicate keyword-argument rendering
(`src/wiring/stubgen.rs:776-834`). Most rendering repeatedly allocates with
`push_str(&format!(...))`.

Recommendation:

- Split artifact discovery/file update from pure Python rendering.
- Use one signature renderer and one call renderer parameterized by indentation
  and header/footer text.
- Use `std::fmt::Write` with `write!`/`writeln!` for incremental output.
- Keep a pure `render_module(manifest) -> String` boundary with golden tests.

A template engine would add more machinery than this generator needs. Small
rendering helpers and `fmt::Write` are the idiomatic middle ground.

### 7. Error formatting contains avoidable manual boilerplate

`WireError` has a roughly 100-line manual `Display` implementation
(`src/coordinator/error.rs:115-215`) even though the crate already depends on
`thiserror`. Most variants map directly to one format string. The ring crate
manually implements `Display` and `Error` for five error types
(`ring/src/lib.rs:259-374`).

Recommendation: derive `thiserror::Error` for `WireError` now. For the ring,
weigh approximately 100 lines of deletion against adding a proc-macro
dependency; keeping it dependency-light is a reasonable exception. Do not
force `LoadError` into a derive if its custom source anchoring and miette
behavior become less clear.

### 8. Large modules combine distinct reasons to change

The largest production modules are:

- `ring/src/lib.rs`: 1,511 lines
- `src/wiring/stubgen.rs`: 1,334 lines
- `src/coordinator/init.rs`: 1,187 lines
- `macros/src/system_attr.rs`: 1,141 lines
- `src/wiring/resolve.rs`: 1,082 lines
- `src/coordinator/slot.rs`: 1,020 lines

Size alone is not a defect. The ring is cohesive and benefits from keeping its
layout and atomic invariants together. The others mix separable work:

- `system_attr.rs`: syntax classification, diagnostics, intermediate data, and
  token emission.
- `coordinator/init.rs`: graph validation, edge solving, allocation planning,
  registry creation, copy-in planning, and final binding inputs.
- `resolve.rs`: artifact loading, params, system resolution, slot resolution,
  and edge name resolution.
- `slot.rs`: pure lifecycle transitions, in-process loading, and process
  loading.

Recommendation: split only at pure-data boundaries. For example, parse macro
input into a small `SystemModel` and emit from that model; make ring allocation
consume a `RingPlan`; make slot transition logic independent of its loader
backend. Avoid files that merely forward calls to each other.

### 9. Test organization is harder to navigate than the behavior requires

`src/coordinator/tests.rs` is 2,135 lines, `src/wiring/tests.rs` is 1,390, and
`src/telemetry/tests.rs` is 1,262. Numbered separator banners act as a manual
table of contents. This is a sign the module boundary should do that work.

Recommendation: split tests by behavior, not by implementation helper:
`coordinator/tests/{validation,clock,async,process,registry}.rs` and analogous
telemetry/wiring modules. Keep fixtures close to the tests that own them.
Prefer explicit setup over a generic test framework; recent removal of overly
abstract helpers was directionally correct.

### 10. Avoid micro-refactoring code that Clippy already considers idiomatic

The all-target Clippy run found one production issue: a collapsible nested `if`
at `src/telemetry/mod.rs:969`. Iterator use, `let-else`, enum matching, ownership,
and error propagation are already conventional. Replacing clear loops with
dense iterator chains, introducing newtypes for every index, or extracting
one-use three-line helpers would make the code more abstract without making it
shorter.

Recommendation: fix the one warning, then focus style work on deletion and
representation changes rather than syntax churn.

## Comment Policy

Keep:

- Every `SAFETY` explanation and the invariant it relies on.
- Shared-memory layout, memory ordering, ABI lifetime, and teardown contracts.
- Public behavior that cannot be inferred from the signature.
- Reasons for non-obvious policy choices such as loss versus backpressure.

Delete or shorten:

- Comments that restate the next statement.
- “Twin of”, “same as”, and step-by-step tours duplicated across modules.
- Work-package, phase, and former-version history outside changelogs.
- Section banners where normal Rust items already provide structure.
- Private-field rustdoc that only expands the field name.

## Suggested Order

1. Apply the comment policy and split the three largest test modules. Low risk,
   immediate readability gain.
2. Simplify `WiringBuilder`, `PortDecl`, and stub rendering. These are bounded,
   testable refactors.
3. Move platform selection behind the `proc` module boundary.
4. Resolve async semantics, then converge all authoring styles on one internal
   driver pipeline.
5. Split large production modules only after the simpler internal models exist.

The target should not be the fewest possible lines. It should be fewer concepts
that each have one representation, one lifecycle, and one place where their
invariants are explained.
