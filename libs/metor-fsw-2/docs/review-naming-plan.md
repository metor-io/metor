# metor-fsw-2 — Idiomatic naming review (Rust API Guidelines)

Scope: API naming + general style only (C-CASE, C-GETTER, C-ITER, C-CONV, conversion
prefixes `as_`/`to_`/`into_`, stutter, predicate clarity). Docs, perf, and module layout
were out of scope.

## Outcome

The crate is already highly idiomatic for naming. A full sweep of the public surface
(every `pub`/`pub(crate)` `fn`, every type/trait/enum/variant, every const/field, plus
re-export paths in `lib.rs`) found **no high-confidence, clearly-correct rename**.

Verified clean:
- Conversion prefixes correct: `Name::as_str` (cheap borrow), `into_port_desc` /
  `into_descriptor` / `into_slot` (consuming). No `as_`-that-allocates, no `to_`-that-consumes.
- No `get_*` getters. Bare `get` only on collection/view types (`Registry::get(key)`,
  `ListReader::get(i)`, `MapReader::get(key)`, `FrameRef::get`) — all C-GETTER conformant.
- Iterators: `ListReader::iter` / `MapReader::iter` (C-ITER).
- Casing: acronyms are single words already — `Tcp`, `Kdl`, `Fsw`, `Hz`; consts
  `FSW_ABI_VERSION` / `FRAME_ID` correct; no `TCP`/`ID`/`HTTP`-style violations; no
  camelCase fns/vars; all fields snake_case.
- No module stutter (`FrameWriter`/`SystemDescriptor`/`PortDesc` etc. flatten cleanly at
  crate root; no `frame::FrameHeader`-style names exist).
- Booleans: `is_empty`/`is_lapped`/`is_stopped`/`is_none` all conformant.
- Constructors: `new`, `with_*`, validating `Name::new -> Option` (NonZero-style). Fine.

## Renames applied

None — applying churn to a clean, ABI-bearing public API would violate the
"don't churn debatable names / be conservative with the public surface" guidance.

## Flagged for human decision (subjective; NOT changed)

- `SystemSpecBuilder::from_artifact` / `from_static` (builder.rs) take `self` while the
  `from_*` convention is for no-self constructors. clippy::wrong_self_convention
  deliberately stays silent (exported API). Reads fine in the fluent chain; renaming
  (`via_artifact`/`via_static`?) is a public-API churn with no obviously-better name.
- `MapReader::entry(i)` — positional `(key, value)` accessor; `entry` overlaps std's
  HashMap Entry API. Defensible as "the i-th entry"; replacement name is debatable.
- `Out<O, B>` (system) — very terse public type name; intentional brevity.
- `HealthPort::error` / `log` vs `record_lapped` — mild verb-prefix inconsistency.

## Flagged ABI surface (do NOT rename — exported contract)

`fsw_*` symbols + `SYM_*` consts (abi/mod.rs), `extern "C"`/`#[no_mangle]` fns,
`FswRing`/`FswStatus`/`ByteSink`/`FSW_ABI_VERSION` wire-mirror types (abi, dl). The
`Fsw`/`Sym` casing is already correct; these are the dl-open ABI contract with loaded
`.so`s and the fixture.
