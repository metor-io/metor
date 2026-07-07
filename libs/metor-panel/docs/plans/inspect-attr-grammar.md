# Inspect attribute grammar: wire it in or delete it

## Question

`src/inspect.rs` defines a facet attribute grammar
(`inspect::label`, `inspect::widget`, `inspect::read_only`, `inspect::range`)
via `define_attr_grammar!`. Nothing reads it. `CLAUDE.md` claims
"Field rendering is driven by **facet attributes**" — but the real system is
the programmatic `FieldOverride` map registered in
`src/inspector/registry/defaults.rs`. Either the grammar becomes real (option A)
or the claim becomes real (option B). This doc picks one and lays out the
migration.

**Recommendation: option A** — wire the grammar into the field walker, move the
range overrides onto the fields they constrain, and shrink `FieldOverride` to
only what attributes genuinely can't express. Deletion (option B) is a viable
fallback and is spelled out at the end.

## Verified current state

Re-checked against the source, not the docs:

- **The grammar exists and is well-formed.** `src/inspect.rs:9` declares
  `Attr { Label(&'static str), Widget(&'static str), ReadOnly, Range(Range) }`
  with `Range { min: &'static str, max: &'static str }` (string bounds,
  explicitly documented "parsed as f64 at runtime").
- **Zero real usages.** `rg "#\[facet\(inspect::"` returns 5 hits, all of them
  doc-comment `Usage:` examples inside `src/inspect.rs` itself. No struct field
  anywhere in the workspace carries an `inspect::` attribute.
- **Nothing reads the grammar.** `src/inspector/registry/dispatch.rs`
  (`row_for_field`, `default_row_for_shape`) consults only
  `self.field_override(parent_shape_id, ctx.field_name)` — the programmatic map.
  `src/inspector/reflect.rs:69` checks only `FieldFlags::SKIP`. Neither calls
  `field_def.get_attr(...)`, `has_attr`, or `attributes`. The grammar's generated
  `Attr` type is never named outside `inspect.rs`.
- **The `FieldOverride` self-justification is stale.** `registry/mod.rs:61-71`
  says ranges live in `FieldOverride` because "Facet attributes can't take
  non-string literal ranges without parse support at the grammar level." But
  `inspect::Range` was built for exactly that: string `min`/`max` parsed to
  `f64` at runtime. The stated blocker does not exist.

### How the grammar would actually be read (mechanics confirmed)

`facet_core::Field` carries `attributes: &'static [Attr]` plus
`get_attr(ns, key) -> Option<&Attr>` and `has_attr(ns, key) -> bool`
(`facet-core/src/types/ty/field.rs:366-380`). Each `Attr` decodes via
`attr.get_as::<T>()` with `T` matching the stored payload
(`facet-core/src/types/attr.rs:74`). Per the facet decode contract
(`docs/content/extend/extension-attributes.md:86-104`):

| grammar variant | decode |
|---|---|
| `Label(&'static str)` | `attr.get_as::<&'static str>()` |
| `Widget(&'static str)` | `attr.get_as::<&'static str>()` |
| `ReadOnly` (marker) | key presence via `has_attr(Some("inspect"), "read_only")` |
| `Range(Range)` (struct payload) | `attr.get_as::<inspect::Attr>()` then `match Attr::Range(r) => (r.min, r.max)` |

So reading is a few lines in the walker: the `Field` (`field_def`) is already in
hand at `reflect.rs:68`, it just isn't threaded into `FieldBuildCtx` or consulted.

## The crux: what can and can't move to attributes

The complete set of `register_field_override` calls in `defaults.rs`:

| # | site | override | movable to attribute? |
|---|---|---|---|
| 1 | `Trace::stroke_width` (`:65`) | `range: (0.5, 10.0)` | **Yes** — `#[facet(inspect::range(min="0.5", max="10.0"))]` |
| 2 | `Viewer3d::camera_fov` (`:73`) | `range: (0.1, PI)` | **Yes**, with a caveat — `max="3.141592653589793"`; loses the symbolic `std::f64::consts::PI` |
| 3 | `XyTrace::stroke_width` (`:80`) | `range: (0.5, 10.0)` | **Yes** |
| 4 | `XyTrace::style` (`:87`) | `enum_allowed: ["Line","Scatter"]` | **No** with current grammar — filters out `Bar`; there is no `inspect::` variant for an allow-list. Needs a new grammar variant, or stays in `FieldOverride`. |
| 5 | `ListTrace::stroke_width` (`:102`) | `range: (0.5, 10.0)` | **Yes** |
| 6 | `ListTrace::style` (`:109`) | `enum_allowed: ["Line","Scatter","Bar"]` | **Redundant** — `PlotStyle::ALL` is exactly `[Line, Scatter, Bar]` (`time_series/mod.rs:909`), so this filters nothing. Delete it regardless of option. |

Score: **4 of 6 ranges move cleanly**, **1 (the redundant `ListTrace` filter)
is dead and should be deleted either way**, **1 (the `XyTrace` `Bar` exclusion) is
the only override that attributes can't express today.**

What is *structurally* out of reach of any attribute and must stay programmatic —
this is the bulk of the inspector and neither option touches it:

- **Type row builders** (`register_type_builder`): `Viewer3d` "Add Model"/"Reset
  Camera", `Pane` toggles, `DashboardPanel` grid, the `Trace` axis picker,
  `ComponentBrowser` title. These emit rows with no backing Facet field.
- **Field widget factories** keyed by type (`Hsla`→`ColorRow`,
  `ComponentId`→picker, every `Override<T>`, `TimeRangeBehavior`). These are
  closures over `db`/callbacks; `inspect::widget = "…"` (a bare string) cannot
  carry them.
- **Entity lists** (`register_entity_list`) with `AddBehavior::Wizard` closures.

So the attribute surface is genuinely narrow: scalar slider ranges, plus (with a
grammar addition) enum allow-lists. Options A and B are a decision about that
narrow slice only.

### The one real argument for attributes: stringly-typed keys rot silently

`register_field_override::<Trace>("stroke_width", …)` keys on the literal
`"stroke_width"`. Rename the field and the override simply stops matching — no
compile error, the slider silently reverts to a plain scalar box. An attribute
on the field (`#[facet(inspect::range(...))]`) cannot desync from the field it
sits on. This is a real maintainability win and is the substance behind
`CLAUDE.md`'s "config-static structure over dynamic catch-alls" stance
(cf. memory: *extend the standard path, no bespoke carve-outs*).

### The honest caveat about the grammar's other three variants

`Label`, `Widget`, and `ReadOnly` map to **no existing behavior**:

- Labels already come from `field_def.name` (`reflect.rs:79`); nothing renames.
- Widget selection is by *type* (`field_widgets` keyed on `ConstTypeId`), never
  by a per-field string. `inspect::widget = "color_picker"` has no dispatch.
- There is no read-only render path at all; every row is editable.

Wiring `Range` is a genuine improvement. Wiring the other three means *building
new features*, not migrating existing ones. Option A must decide their fate
rather than leave them as advertised-but-dead grammar (which the crate's
"no permanent dead code / no variants that never fire" rule disfavors).

## Recommendation: option A (wire it in), scoped

Wire `Range`, absorb the enum allow-list with one new grammar variant so
`FieldOverride` can be **deleted entirely**, wire `Label` (cheap, same rename-rot
win), and **trim `Widget` + `ReadOnly`** from the grammar until a consumer needs
them. Net result: `CLAUDE.md`'s claim becomes true, the stringly-typed override
keys disappear, and the grammar advertises only what it delivers.

Why A over B: `CLAUDE.md` prescribes attribute-driven rendering as the intended
design; the memory note on the separate-crate fix shows the maintainer already
paid the hard cost to make attributes *definable*; the rename-rot hazard is real;
and the change is small and mechanical. B throws away that prior investment to
save ~50 lines, and re-enshrines the exact dynamic-override pattern the house
style pushes against.

### Migration path (each step leaves the crate compiling and green)

**Step 1 — dead-code cleanup, no behavior change.**
Delete override #6 (the redundant `ListTrace::style` allow-list). `PlotStyle::ALL`
already equals the list, so the enum row is unchanged. Confirms the test suite
still passes before touching mechanics.

**Step 2 — read attributes in the walker; keep `FieldOverride` as the source of
truth.** Thread the `Field` into dispatch: add `field_def: &'static Field` to
`FieldBuildCtx` (set it at `reflect.rs:77`). In `row_for_field`, before
consulting `self.field_override(...)`, derive an effective override from
attributes:
- `range`: `field_def.get_attr(Some("inspect"), "range")` →
  `get_as::<inspect::Attr>()` → `Attr::Range(r)` → parse `r.min`/`r.max` as `f64`.
- Merge precedence: attribute wins over the map (or assert they never both
  exist — see open questions). No attributes exist yet, so behavior is identical.
  This step is pure plumbing and ships green with zero attribute usages.

**Step 3 — migrate the four range overrides onto their fields.** Add
`#[facet(inspect::range(min="0.5", max="10.0"))]` to `Trace::stroke_width`
(`time_series/mod.rs:958`), `XyTrace::stroke_width`, `ListTrace::stroke_width`,
and `#[facet(inspect::range(min="0.1", max="3.141592653589793"))]` to
`Viewer3d`'s fov field. Delete overrides #1–3 and #5 from `defaults.rs`. Verify
each type still gets an `EntityAdapter`: `Trace`/`XyTrace`/`ListTrace` via
`register_entity_list`, `Viewer3d` via `register_entity_list::<Viewer3d, …>` and
`register_viewer3d_builder` — all independent of the deleted
`register_field_override` calls (which only *also* called `register_inspectable`
as a side effect). So no adapter is lost. Sliders render identically.

> Verify `#[facet(pod)]` on `Trace` tolerates an extension attribute on a field —
> `pod` should not conflict with a namespaced `#[facet(inspect::…)]`, but
> confirm the derive compiles (this is the one place to actually build, not just
> reason).

**Step 4 — absorb the enum allow-list, then delete `FieldOverride`.** Add one
grammar variant to `inspect.rs`, e.g. `Variants(&'static str)` carrying a
comma-separated allow-list (string, parsed at runtime — mirrors `Range`'s
approach and sidesteps the "no non-string literals" limit). Read it in
`default_row_for_shape`'s enum branch (`dispatch.rs:120-141`) in place of
`field_override.and_then(|o| o.enum_allowed)`. Put
`#[facet(inspect::variants("Line,Scatter"))]` on `XyTrace::style` and delete
override #4. With ranges and the allow-list both attribute-driven, remove the
`FieldOverride` struct, `field_overrides` map, `field_override()`,
`register_field_override()`, and the `field_override` params threaded through
`dispatch.rs`. `CLAUDE.md`'s claim is now literally true.

**Step 5 — wire `Label`, trim `Widget`/`ReadOnly`.** In the walker
(`reflect.rs:77-81`), let `field_def.get_attr(Some("inspect"), "label")` override
`SharedString::from(field_def.name)` when present — cheap, and extends the
rename-rot win to labels. Then delete `Widget` and `ReadOnly` from the grammar:
`Widget` is subsumed by type-keyed dispatch and has no per-field meaning, and
there is no read-only render path to gate. (Alternatively, keep `ReadOnly` and
build a disabled-row path — but that is a *feature*, out of scope for making the
docs honest; leave it for when a field actually needs it.)

**Step 6 — docs.** Update `registry/mod.rs`'s stale comment (now deleted with the
struct). Trim `inspect.rs`'s module doc to the surviving variants. Confirm
`CLAUDE.md`'s "Field rendering is driven by facet attributes … `inspect::range`,
`inspect::read_only`" list matches what the grammar now actually contains.

After step 4 the crate already honors `CLAUDE.md`; steps 5–6 are polish. Each
step compiles and keeps `cargo test -p metor-panel` green.

## Fallback: option B (delete the grammar)

If the team decides the inspector will stay deliberately programmatic (the honest
read is that ~all of it already is — type builders, widget factories, wizards),
delete `src/inspect.rs`, its `pub mod inspect` in `lib.rs`, and fix `CLAUDE.md` to
describe `FieldOverride`. Also delete redundant override #6 either way.

The sunk-investment objection (the separate-crate macro-resolution fix, recorded
in memory) does **not** block deletion: that work bought the ability to *define*
resolvable attributes, and that knowledge lives in memory, recoverable if ever
wanted. Definability without a single consumer is exactly the dead code the crate
forbids. But B is the weaker choice: it discards a design `CLAUDE.md` explicitly
prescribes and keeps the stringly-typed, rename-rot-prone override keys that
option A eliminates. Recommend B only if the maintainer affirmatively wants the
inspector to remain override-map-driven.

## Open questions

1. **Precedence / coexistence:** during the migration window (step 2), a field
   could in principle carry both an attribute and a map override. Assert they're
   mutually exclusive, or define attribute-wins? Proposed: attribute wins, and
   after step 4 the map is gone so the question dissolves.
2. **`#[facet(pod)]` + extension attribute on `Trace`:** confirmed needed but not
   yet compiled — does the `pod` derive path accept a namespaced field attribute?
   This is the single build-time risk; validate before step 3.
3. **`camera_fov` symbolic `PI`:** `max="3.141592653589793"` parses to the exact
   `f64` nearest `PI` (which *is* `f64::consts::PI`), so behavior is identical,
   but the literal loses its symbolic name. Acceptable, or keep this one override
   programmatic as the sole survivor? (If kept, `FieldOverride` can't be fully
   deleted — argues for accepting the numeric string.)
4. **`ReadOnly`'s fate:** trim it (no consumer) or build the disabled-row path now?
   Recommend trim; revisit when a real field needs read-only display.
5. **`Widget` genuinely dead?** Confirm no future plan wants per-field widget
   override (e.g. forcing a slider vs. numeric box for the same type). If yes,
   keep the variant and wire it; if no, trim.
