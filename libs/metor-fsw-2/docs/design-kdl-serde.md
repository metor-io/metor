# Design: KDL params via a serde Deserializer

Status: DESIGN — approved decision is an **in-house `serde::Deserializer` over
`&kdl::KdlNode`** (not knus/knuffel; `serde` is already a non-optional dep, `kdl`
6.5 + `miette` stay behind the `kdl` feature). This replaces the bespoke
`FromKdlNode`/`FromKdlScalar` traits, the `#[derive(FromKdlNode)]` proc macro, and
`encode_kdl_params` with **one** deserializer that serves both the static path
(typed `Params` structs) and the dl path (`serde_json::Value` → postcard-dyn).
It structurally fixes review finding **B1** (slot occupant params can never
resolve) and folds in the **E5** surface fixes and the **C5** `RegisteredSystem`
deletion.

Files this design touches:

- `libs/metor-fsw-2/src/wiring/mod.rs` — delete `FromKdlNode`/`FromKdlScalar`/
  `kdl_required`/`kdl_optional`/`RegisteredSystem`; rewrite `encode_kdl_params`;
  new `de.rs` submodule; `parse_*` migrations; new/renamed `LoadError` variants.
- `libs/metor-fsw-2/src/wiring/model.rs` — no data-model changes required
  (`ParamSource::Kdl(String)` carries node text exactly as today).
- `libs/metor-fsw/macros/src/from_kdl.rs` + `lib.rs` — delete the derive.
- `libs/metor-fsw-2/src/lib.rs`, `src/system/mod.rs` — re-export/doc updates,
  bound change.
- `examples/adcs-fsw2/contracts/src/lib.rs`, `src/abi/tests.rs`,
  `src/wiring/tests.rs`, `tests/*` — migration (params structs already derive
  `serde::Deserialize`, so mostly deletion).
- `examples/adcs-fsw2/mission.kdl` and test fixtures — `lib=` → `artifact=` on
  `system` nodes (E5b).

---

## 1. The deserializer

One new module, `src/wiring/de.rs` (kdl-feature-gated like the rest of wiring).

### 1.1 Types

```rust
/// Deserializes any `T: Deserialize` from one KDL node's params surface:
/// its non-reserved line properties, its non-reserved positional arguments
/// (there must be none — extras are a spanned error), and its child nodes.
pub(crate) struct KdlNodeDe<'de> {
    node: &'de KdlNode,
    /// Line-property keys that belong to the wiring surface, not the params:
    /// `&["type", "artifact"]` for a `system` node, `&["occupant"]` for an
    /// `allow` node, `&[]` for a nested params child node.
    reserved: &'static [&'static str],
    /// Leading positional (nameless) args that belong to the wiring surface
    /// (the instance name on `system`, nothing on nested children).
    skip_args: usize,
}

/// One field's value source, produced by the map access.
enum FieldSource<'de> {
    /// A line property value: `k=v`.
    Entry(&'de KdlEntry),
    /// A child node: `k v`, `k a b c`, `k p=1 { ... }`, `k { ... }`.
    Node(&'de KdlNode),
    /// Repeated same-name children: `k 1` `k 2` → a sequence.
    Nodes(&'de [&'de KdlNode]),   // (owned SmallVec in practice)
}

/// The value-position deserializer the MapAccess hands to `next_value_seed`.
struct FieldDe<'de> {
    source: FieldSource<'de>,
    key: &'de str,
    span: SourceSpan,             // the entry's / child node's span
}
```

Entry points (the only public surface of `de.rs` inside `wiring`):

```rust
/// Static path: a typed `Params`.
pub(crate) fn from_kdl_node<T: serde::de::DeserializeOwned>(
    node: &KdlNode,
    src: &str,                    // the document text, for miette source context
    system: &str,                 // instance/type label for diagnostics
    reserved: &'static [&'static str],
    skip_args: usize,
) -> Result<T, LoadError>;

/// Dl path: the same deserializer targeting `serde_json::Value`, plus the
/// property→span side table the schema-validation pass uses for diagnostics.
pub(crate) fn params_value(
    node: &KdlNode,
    src: &str,
    system: &str,
    reserved: &'static [&'static str],
    skip_args: usize,
) -> Result<(serde_json::Value, HashMap<String, SourceSpan>), LoadError>;
```

`serde_json::Value` implements `Deserialize`, so `params_value` is
`from_kdl_node::<serde_json::Value>` plus a trivial span-collection walk over
the node's entries/children — **one deserializer serves both paths** (the design
crux). The span table maps top-level property/child names to their
`KdlEntry::span()`/`KdlNode::span()` (kdl 6.5 exposes both).

### 1.2 KDL → serde mapping table

| KDL shape | serde call driven | Rust target |
|---|---|---|
| line property `k=v` (scalar) | map key `k`, value = scalar (below) | struct field `k` |
| child node `k v` (one positional arg, no props/children) | map key `k`, value = scalar | struct field `k` (asserted today by `slot_node_round_trips_to_slot_spec`: `gain 0.8`) |
| child node `k a b c` (≥2 positional args) | map key `k`, value = seq of scalars | `Vec<T>`, `[T; N]`, tuple |
| child node `k p=1 q=2` and/or `k { ... }` | map key `k`, value = **recursive `KdlNodeDe`** (`reserved: &[]`, `skip_args: 0`) | nested struct / `HashMap` |
| repeated children `k ...` `k ...` | map key `k` (once), value = seq; each element deserialized per the two rows above | `Vec<Nested>` |
| `KdlValue::Integer(i128)` | `visit_i64`/`visit_u64`/`visit_i128` per `deserialize_*` hint, **range-checked**; out-of-range → spanned invalid-value | any integer width |
| `KdlValue::Float(f64)` | `visit_f64` | `f32`/`f64` |
| integer literal where a float is asked (`rate=200`) | `visit_f64(200.0)` | float field (preserves today's `FromKdlScalar` coercion, and matches postcard-dyn's `as_f64`) |
| `KdlValue::Bool` (`#true`/`#false`) | `visit_bool` | `bool` |
| `KdlValue::String` | `visit_str` | `String`, `&str`-adjacent, `char` |
| string where `deserialize_enum` is asked | `visit_enum(UnitVariantAccess)` | fieldless enum from a string (`state="running"`) |
| `#null`, or `deserialize_option` on an absent-able source | `visit_none` / `visit_some` | `Option<T>` present-with-null |
| **absent** property/child for an `Option<T>` field | serde's own `missing_field` mechanism (its private deserializer supports only `deserialize_option`) → `None` | `Option<T>` — no special casing needed |
| absent property/child for a non-Option field without `#[serde(default)]` | `Error::missing_field` → `LoadError::MissingParam` | — |
| extra positional args beyond `skip_args` | spanned error ("unexpected argument") | — |
| same key as both a property and a child, or repeated property | spanned error (ambiguous) — KDL last-wins for repeated properties is **not** honored for params; explicit is better | — |
| root `deserialize_unit` (i.e. `T = ()` / unit struct) | ok iff the node has **no** non-reserved params; else unknown-param errors | `type Params = ()` |

Scalar handling is driven by the `deserialize_*` method called (serde structs
tell us the wanted type), with `deserialize_any` falling back on the KDL value's
own type — which is exactly what the `serde_json::Value` target exercises:
properties/child-scalars become JSON numbers/strings/bools, nested children
become JSON objects, repeated children/multi-arg children become JSON arrays,
`#null` becomes `Value::Null`.

### 1.3 Unknown-field handling (deny-by-default, no attribute required)

serde-derive only rejects unknown fields when the struct opts into
`#[serde(deny_unknown_fields)]`; otherwise it drains them through
`deserialize_ignored_any`. We must not depend on every system author remembering
an attribute, and today's `DlUnknownParam` diagnostic quality is the bar. So:

**`FieldDe::deserialize_ignored_any` returns an error** —
`DeErrorKind::UnknownField { field }` carrying the entry/child span. Since the
only way serde-derive reaches `ignored_any` on a struct is an unmatched key,
this turns *every* params struct into deny-unknown-fields with a span naming the
property, uniformly, static and nested alike. (Structs that genuinely want
passthrough can use a `HashMap`/`Value` field — `deserialize_any` still works.)

This is a strict improvement over today: the static path (`kdl_required` walk)
currently **silently ignores** unknown properties; only the dl path rejects them.

For the dl path the `serde_json::Value` target consumes every key (no
`ignored_any` is ever called), so unknown-field detection there stays in the
schema-validation pass (§3) — same division as today, better spans.

`NodeMapAccess::next_key_seed` skips reserved keys and the first `skip_args`
positional args before yielding, so `type=`/`artifact=`/`occupant=` never reach
the params struct at all — this is the structural half of the B1 fix.

---

## 2. Span-carrying errors (the crux)

`serde::de::Error::custom` only gives a message string, so the error type must
capture position *inside the deserializer* and the map access must *attach* it
around visitor-originated errors.

```rust
/// The de.rs-internal error. Implements `serde::de::Error`; converted to
/// `LoadError` only at the `from_kdl_node`/`params_value` boundary.
#[derive(Debug)]
pub(crate) struct DeError {
    kind: DeErrorKind,
    /// The most specific span known. `None` only for visitor-originated
    /// `custom` errors until the enclosing MapAccess attaches one.
    span: Option<SourceSpan>,
    /// The property/child name in scope, attached by the MapAccess.
    property: Option<String>,
}

#[derive(Debug)]
enum DeErrorKind {
    /// serde `custom` / `invalid_value` from a visitor (e.g. a hand-written
    /// `Deserialize` parsing a string) — message only, span attached by context.
    Custom(String),
    MissingField { field: &'static str },
    UnknownField { field: String },
    /// Raised by OUR code with the exact value span (we see the KdlValue and
    /// the wanted type before any visitor runs).
    InvalidType { expected: String, found: String },
    /// Integer out of range for the asked width (u8 = 300, i32 = 2^40, ...).
    OutOfRange { expected: String },
    /// Extra positional argument / property-vs-child ambiguity / etc.
    Shape(String),
}

impl serde::de::Error for DeError {
    fn custom<T: fmt::Display>(msg: T) -> Self { /* Custom, span: None */ }
    fn missing_field(field: &'static str) -> Self { /* MissingField */ }
    fn unknown_field(field: &str, _expected: &'static [&'static str]) -> Self
        { /* UnknownField — also produced by deny_unknown_fields structs */ }
    fn invalid_type(unexp: de::Unexpected, exp: &dyn de::Expected) -> Self
        { /* InvalidType */ }
    // invalid_value/invalid_length/duplicate_field → Custom/Shape
}
```

Two mechanisms keep spans exact:

1. **Origin spans.** Type mismatches are detected by `FieldDe` itself (it holds
   the `KdlValue` and knows which `deserialize_*` was called), so
   `InvalidType`/`OutOfRange` are born with `span = Some(entry.span())`. This
   covers everything today's `kdl_required` reports — but with the **entry's**
   span, not the whole node's (an improvement).

2. **Context attachment.** `NodeMapAccess::next_value_seed` wraps:

   ```rust
   fn next_value_seed<V>(&mut self, seed: V) -> Result<V::Value, DeError> {
       let (key, span, source) = self.pending.take().unwrap();
       seed.deserialize(FieldDe { source, key, span })
           .map_err(|e| e.with_context(key, span))   // fills span/property IF None
   }
   ```

   so a `custom` error raised deep inside a user's hand-written `Deserialize`
   (or `serde(deserialize_with)`) still lands on the property that contained it.
   `MissingField` errors (raised by serde-derive at end-of-map) get the **node**
   span attached at the boundary — identical to today's `MissingParam` behavior.

### 2.1 Boundary mapping into `LoadError`

`from_kdl_node` converts at the edge (no serde types leak into `LoadError`):

| `DeErrorKind` | `LoadError` |
|---|---|
| `MissingField` | `MissingParam { property, system, src, span }` |
| `InvalidType`/`OutOfRange` | `InvalidParam { property, system, expected, src, span }` |
| `UnknownField` | **new** `UnknownParam { property, system, src, span }` (label: "no params field is named this") |
| `Custom`/`Shape` | `InvalidParam` with the message as `expected` (or a new `ParamError` catch-all — recommend reusing `InvalidParam`, message quality is equivalent) |

`MissingParam.system` / `InvalidParam.system` change from `&'static str` (the
derive passed the struct name) to `String` (the instance name — more useful in a
mission file with two `Nav` instances). Fallback span when `DeError.span` is
`None` after context attachment: `node.span()`.

The dl-path variants `DlUnknownParam`/`DlMissingParam`/`DlParamTypeMismatch`
**merge into** `UnknownParam`/`MissingParam`/`InvalidParam` — the two paths now
report identically (same codes, one label wording). `DlParamEncode` stays as the
postcard-dyn catch-all. Net: `LoadError` loses three variants, gains one.

Span frame of reference: unchanged. Resolve-time params errors index into the
carried node text (`ParamSource::Kdl(String)`), parse-time errors into the full
document — exactly today's behavior, but with entry-precise spans instead of
whole-node spans where possible.

---

## 3. The dl path: `params_value` + schema validation + postcard-dyn

`encode_kdl_params(node_text, schema, system)` is rewritten (same signature,
plus `reserved`/`skip_args` threaded in — or better, it takes the already-parsed
`&KdlNode`; `resolve_dl`/`resolve_slot` re-parse the carried text as today):

```rust
pub(crate) fn encode_kdl_params(
    node: &KdlNode,
    src: &str,
    schema: &OwnedNamedType,
    system: &str,
    reserved: &'static [&'static str],
) -> Result<Vec<u8>, LoadError> {
    let (value, spans) = params_value(node, src, system, reserved, /*skip_args*/ 1)?;
    let value = conform_to_schema(&schema.ty, value, &spans, system, src, node.span())?;
    postcard_dyn::to_stdvec_dyn(schema, &value)
        .map_err(|e| /* DlParamEncode, as today */)
}
```

### 3.1 What postcard-dyn 0.2.1 tolerates (verified against `ser.rs` source)

- **Integer widths:** `to_stdvec_dyn` does `value.as_i64()/as_u64()` +
  `try_from` per width — a plain JSON number for a `u8` field is accepted iff in
  range. No width-tagged coercion needed. Out-of-range → nameless
  `Error::SchemaMismatch`.
- **Floats:** `value.as_f64()` — accepts JSON integer numbers (so `rate=200`
  into `f64` works even without our coercion). An integer field given a JSON
  float fails (`as_i64` → `None`) — correct.
- **Structs (`ser.rs:268-280`):** requires `obj.len() == fields.len()` **and**
  every schema field present. A missing `Option` field is a `SchemaMismatch` —
  `Value::Null` must be inserted explicitly. Unknown keys break the length check
  with a nameless error.
- **Recursion:** `Seq`/`Tuple`/`Map(String-keyed)`/`Option`/`Enum`
  (unit-variant-from-string and one-key-object variants) all supported.

Conclusion: **schema-guided conformance is still needed**, not for width
coercion but for (a) inserting `Null` for absent `Option` fields, (b) named,
spanned diagnostics postcard-dyn cannot give.

### 3.2 `conform_to_schema`

A recursive walk replacing today's flat field loop, now supporting nested
shapes (a capability upgrade — the deserializer produces nested maps/arrays and
postcard-dyn encodes them):

- Top level must be `Struct`/`Unit`/`UnitStruct` (as today).
- For each JSON key with no schema field → `UnknownParam` (span from the §1
  span table; today's `DlUnknownParam` quality, better span).
- For each schema field absent from the JSON object: `Option` → insert
  `Value::Null`; else `MissingParam` (node span, as today).
- Leaf type check per field (bool/int-with-range/float/string/char/option) →
  `InvalidParam` with `leaf_expected`-style wording and the property's span.
- Nested `Struct`/`Seq`/`Enum` values: recurse with the same rules; nested
  errors carry the top-level property's span (the side table is top-level only —
  acceptable v1; deeper spans would require a spanned-Value type, deferred).
- v1 staging escape hatch: any schema shape the walk doesn't model yet falls
  through to postcard-dyn, whose failure surfaces as `DlParamEncode` (never
  silently wrong bytes).

Byte-equality with `WiringBuilder::params` (the `tests_abi` gate) is preserved:
`to_stdvec_dyn` iterates **schema field order** and looks keys up by name
(`ser.rs:275`), so JSON map order is irrelevant.

---

## 4. Static path migration

- `Registry::register<S, K>` / `factory<S, K>` bound: `S::Params: FromKdlNode`
  → `S::Params: serde::de::DeserializeOwned`. `()` already implements
  `Deserialize` (deserialize_unit; §1.2 makes it succeed on a param-less node
  and reject stray properties — an improvement over today's blanket `Ok(())`).
- `factory` body: `from_kdl_node::<S::Params>(ctx.node, ctx.src, ctx.name,
  &["type", "artifact"], 1)`.
- **Delete:** `FromKdlNode` trait + `impl for ()`, `FromKdlScalar` + all impls,
  `kdl_required`, `kdl_optional`, the `pub use metor_fsw_macros::FromKdlNode`
  re-export, `libs/metor-fsw/macros/src/from_kdl.rs`, and the
  `#[proc_macro_derive(FromKdlNode, attributes(kdl))]` entry in the macros
  crate.
- **Delete `RegisteredSystem`** (finding C5 — the marker trait is unused; its
  doc-role moves to a comment on `Registry::register`).
- `#[kdl(default = expr)]` → serde attributes: `#[serde(default)]` (Default
  impl) or `#[serde(default = "fn_path")]`. The one in-tree user is the
  `RoundTrip` test struct; contracts use only required + `Option` fields.
- `examples/adcs-fsw2/contracts/src/lib.rs`: params structs already derive
  `Serialize, Deserialize, Schema` — just drop `metor_fsw_2::wiring::FromKdlNode`
  from the derive lists. `src/abi/tests.rs` drops its hand-written
  `impl FromKdlNode for CounterParams` (it already derives what's needed).
- `parse_coordinator`/`parse_telemetry`/slot children (`input`/`output`/
  `allow`/`initial`) migrate onto the same deserializer via small private serde
  structs (e.g. `struct CoordinatorProps { cycle_rate: f64, default_depth:
  Option<usize>, sim_dt: Option<f64> }`), which gives them unknown-property
  rejection and entry-precise spans for free. `parse_edge` stays hand-written
  (the `"a" -> "b"` positional shorthand doesn't map onto a struct).
  `BadSlotState` becomes a fieldless-enum deserialize (`state="running"` →
  `SlotInitState`) — keep the variant by mapping the enum's unknown-variant
  `DeError` to `BadSlotState` in `parse_slot`, or fold it into `InvalidParam`
  (recommend: fold; one fewer bespoke variant, message parity via `expected =
  "one of `empty`/`loaded`/`running`"`). **Default-on-absent stays `Loaded`.**

## 5. B1: slot `allow` params, structurally

Reserved-property convention: on an `allow` node, **`occupant=` is the only
reserved key**; *everything else is params* — remaining line properties **and**
child nodes, uniformly, because `KdlNodeDe` reads both surfaces through the same
map access. Both forms are therefore supported at zero extra cost:

```kdl
allow occupant="commissioning" gain=0.8            // line-property form
allow occupant="commissioning" { gain 0.8 }        // child-node form (today's test syntax)
allow occupant="safe_mode"                          // paramless
```

Canonical/documented form: line properties for scalars (mirrors `system`
nodes), children for nested/sequence params. Changes:

- `parse_slot`: an `allow` with any non-reserved property **or** any child ⇒
  `ParamSource::Kdl(child.to_string())`; else `ParamSource::None`. (Today only
  children trigger `Kdl`, so line-property params are silently dropped — half of
  B1.)
- `parse_system`: same has-config test gains "or has children" so nested static
  params work.
- `resolve_slot`/`resolve_dl`: call the new `encode_kdl_params` with the right
  reserved set — `&["occupant"]` for allow nodes, `&["type", "artifact"]` for
  system nodes (today's hardcoded `type`/`lib` skip inside `encode_kdl_params`
  is what rejected `occupant=` as `DlUnknownParam` — the other half of B1).
  `skip_args = 1` for both (instance name / none required on allow: use 0 for
  allow — `occupant` is a property, not an arg).

## 6. E5 surface fixes folded in

- **(c) Unknown top-level nodes**: `parse()` collapses its six per-name passes
  into one loop with a `match` on `node.name().value()` (also review item C6);
  the `_` arm returns a new spanned
  `LoadError::UnknownTopLevelNode { node: String, src, span }` with label
  "expected `coordinator`/`artifact`/`system`/`slot`/`connect`/`telemetry`".
- **(b) `system` `lib=` → `artifact=`**: **hard rename** (recommended). The
  project is pre-1.0 with all users in-repo; a deprecated alias would keep two
  reserved keys competing with params namespaces forever. Ease the cut with a
  dedicated spanned error when `lib=` appears on a `system` node: "`lib=` on
  `system` nodes was renamed to `artifact=` (it references an artifact id;
  `lib=` on `artifact` nodes still means the library stem)". `artifact` nodes
  keep `lib=` (there it genuinely is the file stem).
- **(a) `type=` on dl systems**: becomes **optional when `artifact=` is given**
  (the artifact's `system_type` is authoritative); when present, `resolve_dl`
  validates it equals `artifact.system_type` — mismatch is a new spanned
  `LoadError::TypeMismatchesArtifact { system, ty, artifact_type, src, span }`.
  Static systems still require `type=` (`MissingType` unchanged). Model change:
  `SystemSpec.ty: String` → `Option<String>`? No — keep `String`, resolved at
  parse: dl system without `type=` stores the artifact id's type lazily…
  parse can't see artifacts' types reliably (forward refs are legal though —
  artifacts are parsed first, same doc). Simplest: keep `SystemSpec.ty:
  Option<String>` and let `resolve_dl` fill from the artifact; `resolve_static`
  errors `MissingType` when `None`. (Model + builder `ty()` stays; builder-made
  dl specs may omit `ty`.)
- **(d) WireError carrying system/port names**: coordinator-side
  (`src/coordinator/mod.rs` `WireError` variants print indices/id hashes) — out
  of scope for this refactor; noted for the coordinator work package. The wiring
  layer keeps wrapping `WireError` in `LoadError::Wire` unchanged.

## 7. Migration and test plan

Existing coverage to keep green (variant names asserted via `matches!`):

- `src/wiring/tests.rs` — `err_missing_param`, `err_invalid_param` (variants
  unchanged), `end_to_end_load_and_run`, `builder_and_kdl_produce_equal_wiring`,
  `dl_kdl_params_are_carried_as_kdl_source`, `slot_node_round_trips_to_slot_spec`
  (child-form params), `slot_state_defaults_to_loaded_and_rejects_garbage`
  (update if `BadSlotState` folds into `InvalidParam`), telemetry/slot error
  tests.
- `src/wiring/tests.rs:586` `RoundTrip` derive tests → re-derive with
  `serde::Deserialize` + `#[serde(default = "...")]`; assertions unchanged
  (int-literal-for-float coercion is preserved by §1.2).
- `tests/wiring_resolve.rs` — `kdl_and_builder_dl_params_are_byte_identical_and_run_equal`
  is the byte-equality gate; must pass unmodified.
- `tests/slot_wiring.rs`, `tests/slot_integration.rs`, `tests/dl_integration.rs`,
  `src/cli/tests.rs` — parse-driving; only fixture syntax updates (`lib=` →
  `artifact=` on `system` nodes).
- `src/abi/tests.rs` — drop the manual `FromKdlNode` impl.

New tests:

1. **B1 regression (end-to-end, the missing test the finding names):** a slot
   with a parametered occupant in **both** forms (`allow occupant="x" gain=0.8`
   and `allow occupant="x" { gain 0.8 }`) resolved against the dl fixture;
   assert the occupant's encoded bytes equal
   `WiringBuilder::allow_with_params(..)`'s postcard bytes, and the loaded
   occupant runs with the params applied.
2. Static-path **unknown param** is now an error (was silently ignored):
   `system "nav" type="Nav" gian=0.8` → `UnknownParam` naming `gian`, spanning
   the entry.
3. Nested params through the dl path: a fixture `Params` with a nested struct +
   `Vec` + `Option`, expressed as children; byte-equal to the typed encode.
4. Span quality: `InvalidParam` span points at the **entry** (`rate="x"`), not
   the whole node (assert `span.offset()` within the value token).
5. E5: unknown top-level node → `UnknownTopLevelNode`; `lib=` on system →
   rename error; `type=` omitted with `artifact=` resolves; `type=` mismatching
   the artifact → `TypeMismatchesArtifact`.
6. `()` params reject stray properties (`system "src" type="Source" bogus=1`).

Fixture/document updates: `examples/adcs-fsw2/mission.kdl` (3 `system` lines:
`lib=` → `artifact=`; optionally drop the now-redundant `type=`), any KDL
strings in tests using `lib=` on systems, `DESIGN.md`/`docs/wiring.md` schema
sections, dl-fixture comments.

Suggested implementation order (each step keeps the tree green):

1. `de.rs` + `DeError` + boundary fns, with unit tests against local structs.
2. Switch the static factory bound + delete trait/derive/helpers; migrate
   contracts/tests derives. (C5 `RegisteredSystem` deletion rides along.)
3. Rewrite `encode_kdl_params` on `params_value` + `conform_to_schema`; merge
   the `Dl*` error variants; byte-equality test must stay green.
4. B1: reserved-set plumbing through `parse_slot`/`resolve_slot`/`parse_system`
   + the regression tests.
5. E5 a/b/c + the single-pass `parse()` + new error variants + fixture renames.

---

## Open questions (human decision needed)

1. **`BadSlotState` variant**: fold into `InvalidParam` (fewer bespoke
   variants, same message quality) or keep for test/UI stability? Design
   recommends folding.
2. **`SystemSpec.ty: String` → `Option<String>`** (E5a) is a `Wiring`
   data-model + `WiringBuilder` API change (serialized bundles include it —
   `#[serde(default)]` keeps old bundles loading). Acceptable, or should `ty`
   stay required in the model and only the KDL surface make it optional?
3. **Hard rename `lib=` → `artifact=`** with a guidance error vs. a silent
   deprecated alias for one release — design recommends the hard rename;
   confirm no out-of-repo mission files exist.
4. **Merging `DlUnknownParam`/`DlMissingParam`/`DlParamTypeMismatch` into the
   shared variants** changes diagnostic codes (`fsw_wiring::dl_unknown_param` →
   `fsw_wiring::unknown_param`). Anything downstream matching on codes?
5. **Duplicate-key policy** (§1.2): error on a repeated property (design
   recommendation) vs. KDL-spec last-wins. Erroring is stricter than the KDL
   spec; confirm strictness is wanted for mission files.
