# Work-Package 6 — Wiring (the KDL config front-end)

Status: **design only, pre-implementation**. Reviewer sign-off required before any code lands. No
Rust in this WP. This document specifies the **wiring layer**: a KDL configuration language and the
loader that turns a text document into a built [`Coordinator`]. It fills in DESIGN.md's "Wiring up
systems" section (currently TBD) and is a **pure front-end** onto the **landed** WP5 builder
(`src/coordinator.rs`): it instantiates systems, calls `add_cyclic`/`add_async`/`connect`, and
`build()`s. **No coordinator logic lives in WP6** — WP5's report says exactly this (coordinator.md
§2.1), and this WP holds that line.

Relevant landed code (read before implementing):
- `src/coordinator.rs` — `Coordinator::builder(CoordinatorConfig)`, `CoordinatorBuilder::{add_cyclic,
  add_async, connect, build}`, `SystemHandle`, `PortRef { system, frame_id }`, `WireError`. **This is
  the entire surface WP6 drives.** `add_cyclic`/`add_async` return a `SystemHandle`; `connect` takes
  two `PortRef`s addressed by `(SystemHandle, ComponentId)`; `build()` runs validation/sizing and
  returns a `Coordinator`.
- `src/descriptor.rs` — `SystemDescriptor { name, kind, inputs, outputs }`, `PortDesc { frame_id,
  vtable, max_size, rate_hint }`, `SystemKind::{Cyclic, Async}`, `compatible(..)`. Every port already
  carries its `frame_id`; the loader matches a KDL frame name to a `PortDesc` by `frame_id`.
- `src/system.rs` — `CyclicSystem`/`AsyncSystem`/`System`, `System::NAME`, the `descriptor()` each
  trait derives. WP4 §1.2: a system is **constructible before `init`** — `ConcreteSystem::new(params)`
  yields a value whose ports do not exist yet; the coordinator binds them at `build()`.
- `src/frame.rs` — `Frame::NAME` / `Frame::FRAME_ID = ComponentId::new(NAME)`.
- `libs/metor-proto/src/types.rs` — `ComponentId::new(&str)` (const fnv1a-64, top-bit masked). A
  frame name hashes to its `frame_id` by exactly this call.
- `libs/metor-proto/kdl/` — the existing KDL ↔ `Schematic` crate (`metor-proto-kdl`). It depends on
  the `kdl = "6.3.4"` crate, hand-walks `KdlDocument`/`KdlNode` (`node.get("x").as_float()`,
  `node.entries()`), and reports errors as `miette`-`Diagnostic`s carrying a `#[source_code]` string
  and a `SourceSpan` (`node.span()`). **WP6 reuses this crate's dependency, parsing style, and error
  shape** — see §3. Its `parse_component_monitor` already shows the `frame name → ComponentId::new`
  step we need (`node.get("component_id").map(ComponentId::new)`).

---

## 0. Design summary (orientation)

KDL is *data*; systems are *Rust types*. The loader must turn

```kdl
system "imu" type="ImuDriver" { i2c_bus=1 }
```

into `builder.add_cyclic::<ImuDriver>(ImuDriver::new(params))`. Rust has no reflection over an
arbitrary type named by a runtime string, so WP6 needs a **system registry**: a map from the
KDL `type="…"` string to a **factory** that (a) deserializes that system's params from its KDL
config block and (b) constructs the boxed system and calls the right `add_*` on the builder, handing
back a `SystemHandle`. The app builds this registry once (registering each concrete system type),
then calls the loader.

The loader runs in three passes over the parsed document:

1. **Systems pass.** For every `system` node, look its `type` up in the registry, run the factory
   (params → `new` → `add_cyclic`/`add_async`), and record `instance-name → (SystemHandle,
   SystemDescriptor)` in an **instance table**.
2. **Edges pass.** For every `connect` edge, resolve each `(instance, frame)` endpoint to a
   `PortRef { system, frame_id }` — `frame_id = ComponentId::new(frame)` — **validated against that
   instance's descriptor port list** so a typo is a load error, then call `builder.connect(..)`.
3. **Build.** `builder.build()` runs WP5's compatibility/structural validation and returns the
   `Coordinator`. Every `WireError` is re-surfaced with the KDL source span of the offending node.

Everything below the registry+loader is reuse: parsing is the `kdl` crate (as `metor-proto-kdl`
uses it), wiring/validation/sizing is the landed WP5 builder, addressing is the landed `PortRef`.
The genuinely new surface is the **KDL schema**, the **registry + registration macro**, the
**per-system param-deser trait**, and the **edge resolver**.

---

## 1. KDL schema

A wiring document has three node kinds at the top level: one `coordinator` block, N `system` nodes,
and M `connect` edges. Convention follows `metor-proto-kdl` (lowercase node names, `key=value`
properties, KDL v2 `#true` booleans, children in `{ … }`).

### 1.1 `coordinator`

```kdl
coordinator {
    cycle_rate=100.0      // Hz; CoordinatorConfig.cycle_rate
    default_depth=8       // optional; CoordinatorConfig.default_depth (ring::DEFAULT_DEPTH)
}
```

Maps one-to-one onto `CoordinatorConfig` (`src/coordinator.rs`). Exactly one `coordinator` node;
zero is an error, more than one is an error. `default_depth` is optional (defaults to
`DEFAULT_DEPTH`).

### 1.2 `system`

```kdl
system "imu" type="ImuDriver" {
    i2c_bus=1
    sample_hz=200.0
}
```

- The first (nameless) entry, `"imu"`, is the **instance name** — the unique handle the rest of the
  document and the telemetry sink use to refer to this system. It is **not** the type name; two
  instances of one type get two distinct instance names (§6, the collision question).
- `type="ImuDriver"` is the **registry key** — the string a concrete system registered itself under
  (§2). Unknown ⇒ load error.
- The children block is the **param config**, deserialized into the system's params struct (§3).
- **Cyclic vs async is not declared in KDL.** It is a property of the Rust type
  (`impl CyclicSystem` vs `impl AsyncSystem`), and the factory already knows which `add_*` to call
  (§2.3). Restating it in KDL would let the document contradict the type — a class of error we get to
  not have. (Alternative considered: a `kind="async"` property as documentation/validation — see Q3.)

Instance names must be unique across the document; a duplicate is a load error.

### 1.3 `connect`

An edge names a producer endpoint and a consumer endpoint, each as `(instance, frame)`:

```kdl
connect from="imu" out="imu" to="nav" in="imu"
```

Read as: *instance `imu`'s output frame `imu` feeds instance `nav`'s input frame `imu`*. The four
properties are:

- `from` / `to` — producer and consumer **instance names** (resolved to `SystemHandle`s).
- `out` / `in` — the **frame names** on each side (resolved to `frame_id = ComponentId::new(name)`,
  validated against the descriptor — §4).

`out` and `in` are usually the same frame name (a frame is a contract — DESIGN.md), so a shorthand is
offered for the common case:

```kdl
connect "imu" -> "nav" frame="imu"        // from="imu" to="nav", out=in="imu"
```

Both forms desugar to the same `(PortRef, PortRef)` pair. The explicit `out`/`in` form exists for the
rare case where a producer's port frame name differs from the consumer's, but since `connect`
requires `producer.frame_id == consumer.frame_id` (WP5's `WireError::FrameIdMismatch`), `out` and
`in` must hash equal — so in practice they are the same string. (Q4 asks whether the asymmetric form
is worth keeping at all.)

### 1.4 Worked example (imu-driver → nav-filter → controller, with an async logger)

```kdl
coordinator {
    cycle_rate=200.0
}

// Cyclic IMU driver: no inputs, produces the `imu` frame.
system "imu" type="ImuDriver" {
    i2c_bus=1
    sample_hz=200.0
}

// Cyclic navigation filter: consumes `imu`, produces `nav`.
system "nav" type="MekfFilter" {
    process_noise=1e-6
    init_quat=(1.0 0.0 0.0 0.0)
}

// Cyclic attitude controller: consumes `nav`, produces `cmd`.
system "ctrl" type="YangLqr" {
    gain=0.8
}

// Cyclic process-telemetry source producing a *dynamic* frame (FrameList/FrameMap
// member): the loader treats it like any other producer — the dynamism lives inside
// the frame's VTable, not in the wiring (§4.4).
system "procmon" type="ProcessMonitor" { }

// Async telemetry logger: self-paced, consumes `nav` over a private copy-in buffer.
system "logger" type="NavLogger" {
    path="/var/log/nav.bin"
}

connect "imu"  -> "nav"    frame="imu"
connect "nav"  -> "ctrl"   frame="nav"
connect "nav"  -> "logger" frame="nav"
```

This wires four cyclic systems and one async system. `imu`→`nav`→`ctrl` is the cyclic chain;
`nav`→`logger` crosses into an async consumer (WP5 allocates the private copy-in buffer at `build()`
automatically — nothing async-specific appears in KDL). `procmon` produces a dynamic frame; its
`FrameList`/`FrameMap` member is invisible to the wiring layer because compatibility is checked on the
frame's realized VTable inside `build()` (`compatible`, `src/descriptor.rs`).

Note the edges never mention `kind`, buffer sizes, `max_readers`, or health/log ports — all of that
is WP5's job at `build()` (sizing from `PortDesc`, fan-out from the edge set, auto-provisioned
health). WP6 only names systems and edges.

---

## 2. The system registry

### 2.1 Why a registry (the reflection gap)

`builder.add_cyclic::<S>(s)` is generic over a concrete `S` chosen at **compile** time. KDL names the
type with a **runtime** string. Bridging the two requires a value the loader can look up by string
and call without naming `S` — i.e. a boxed factory closure per concrete system. The registry is the
`HashMap<&'static str, SystemFactory>` of those factories.

### 2.2 The factory contract

```rust
/// Everything a factory needs and produces, erased of the concrete system type.
struct LoadCtx<'a> {
    node:    &'a KdlNode,            // the `system` node (for params + spans)
    src:     &'a str,               // full document, for miette source-code context
    builder: &'a mut CoordinatorBuilder,
}

/// A registered factory: parse params from `ctx.node`, construct the system, add it
/// to the builder, return the handle + the descriptor (for edge validation, §4).
type SystemFactory =
    fn(&mut LoadCtx) -> Result<(SystemHandle, SystemDescriptor), LoadError>;

pub struct Registry {
    factories: HashMap<&'static str, SystemFactory>,
}

impl Registry {
    pub fn new() -> Self { … }
    /// Register concrete system `S` under `type_name`. `S` supplies its params type
    /// and which `add_*` to call (§2.3) via the `RegisteredSystem` trait.
    pub fn register<S: RegisteredSystem>(&mut self, type_name: &'static str) -> &mut Self { … }
}
```

The factory captures **no** state; it is a plain `fn` pointer that, given the node and the builder,
does the whole "params → `new` → `add_*`" dance for one concrete type. It returns the
`SystemDescriptor` alongside the `SystemHandle` so the loader can validate edge frame names against
the actual port list (§4) without re-deriving it.

### 2.3 How a concrete system opts in — `RegisteredSystem` + a derive

A concrete system declares three things: its params type, how to build itself from params, and which
kind it is. A small trait captures this, and a derive macro generates the factory body so the user
writes no boilerplate:

```rust
pub trait RegisteredSystem: Sized {
    /// The params struct deserialized from the KDL config block (§3).
    type Params: FromKdlNode;
    /// Construct the (pre-init) system from its params — WP4 §1.2 "constructible before init".
    fn new(params: Self::Params) -> Self;
    /// Register on the builder with the correct add_*; returns the handle.
    /// Implemented once per system *kind* (blanket impls below), never by the user.
    fn add(self, builder: &mut CoordinatorBuilder) -> SystemHandle;
    /// The descriptor, for edge validation.
    fn descriptor() -> SystemDescriptor;
}
```

The `add`/`descriptor` halves are provided by **two blanket impls** keyed on the system trait, so the
user only ever writes `type Params` + `new`:

```rust
// Cyclic systems dispatch to add_cyclic.
impl<S> CyclicRegistered for S
where S: CyclicSystem<Output = Out<…>> + … {
    fn add(self, b: &mut CoordinatorBuilder) -> SystemHandle { b.add_cyclic(self) }
    fn descriptor() -> SystemDescriptor { <S as CyclicSystem>::descriptor() }
}
// Async systems dispatch to add_async (mirrored).
```

The cyclic-vs-async branch is therefore resolved **at compile time** by which system trait `S`
implements — there is no runtime `kind` switch, and KDL cannot disagree with the type (§1.2). The
factory `Registry::register::<S>` stores is:

```rust
|ctx| {
    let params = S::Params::from_kdl_node(ctx.node, ctx.src)?;  // §3
    let system = S::new(params);
    let handle = system.add(ctx.builder);                       // add_cyclic / add_async
    Ok((handle, S::descriptor()))
}
```

### 2.4 Compile-time table vs `inventory` auto-collection

**Recommendation: an explicit, app-built registry table** (the app calls `registry.register::<S>("…")`
for each system it links), **not** global auto-registration via `inventory`/`linkme`.

Justification, grounded in the repo:
- **`inventory` is present only transitively** (it appears in `Cargo.lock` but **no** crate under
  `libs/`/`examples/` depends on it directly, and nothing uses `inventory::submit!`). The existing
  KDL path (`metor-proto-kdl`) uses an **explicit `match` on `node.name()`** (`parse_schematic_elem`),
  i.e. a hand-maintained table — the established project convention is explicit dispatch, not magic
  collection.
- **Determinism & auditability (it's flight software).** An explicit table makes the set of
  loadable systems a visible, reviewable list in the app's `main`. `inventory` relies on
  link-section side effects that can be silently dropped by dead-code elimination when a system crate
  is a dependency but otherwise unreferenced — a genuinely nasty failure mode for FSW, where "the
  IMU driver silently isn't registered" must be impossible.
- **No global mutable state / test isolation.** Each test or mission can build its own `Registry`
  with exactly the systems it wants; `inventory` is process-global.

A `register_system!` convenience macro can wrap `registry.register::<S>("ImuDriver")` to keep the
call site terse, and a system crate can expose a `pub fn register(r: &mut Registry)` that registers
its own systems — so an app composes registries by calling each crate's `register`. This keeps
opt-in explicit while staying ergonomic. (Q1 asks the reviewer to confirm explicit-table over
`inventory`; the trait/factory design is identical either way, only *who fills the map* changes.)

---

## 3. Param deserialization

### 3.1 Mechanism choice — reuse `metor-proto-kdl`'s hand-walk, behind a `FromKdlNode` trait

A system's config block must become its `Params` struct. Three candidate mechanisms exist in-repo:

| Mechanism | In-repo status | Verdict for WP6 |
|---|---|---|
| `metor-proto-kdl` style (hand-walk `KdlNode` via the `kdl` crate, `miette` errors) | **Used today** for the whole `Schematic` ↔ KDL path | **Chosen.** Same dep, same error shape, zero new crates. |
| `serde` | present (transitive/other paths), **no serde-kdl** in tree | Rejected: no KDL serde adapter in the workspace; would add a dep and a second config idiom. |
| `facet` / `facet-kdl` | `facet` used by `metor-panel`/`metor-proto`; **`facet-kdl` not yet in tree** | Deferred. `metor-proto-kdl`'s own README says "long term this should be replaced with facet-kdl when it's ready" — adopt it later behind the same `FromKdlNode` trait, no schema change. |

So WP6 deserializes params exactly the way `metor-proto-kdl` deserializes a `Viewport` or a `Graph`:
walk the `KdlNode`'s properties with `node.get("x").and_then(|v| v.as_float())`, defaulting optionals,
and raising `KdlSchematicError::MissingProperty`/`InvalidValue` (reused or mirrored as `LoadError`
variants — §5) with the node's `span()`.

### 3.2 How a system declares its params type

```rust
pub trait FromKdlNode: Sized {
    fn from_kdl_node(node: &KdlNode, src: &str) -> Result<Self, LoadError>;
}
```

A system sets `type Params = ImuParams` and implements `FromKdlNode` for `ImuParams`. For the common
case (a flat struct of scalars/strings) a **derive** `#[derive(FromKdlNode)]` generates the walk —
field `f: f64` ⇒ `node.get("f").as_float()` required, `Option<T>` ⇒ optional, `#[kdl(default=…)]` ⇒
defaulted — directly mirroring the hand-written `parse_*` functions in `metor-proto-kdl/src/de.rs`.
A system with no params uses `type Params = ()` (empty block, like `procmon` above). When `facet-kdl`
lands, the derive can be retargeted onto it without touching any system's declaration.

This keeps the **param schema owned by the system crate**, next to the system, which is where it
belongs — the loader never hard-codes any system's fields.

---

## 4. Edge resolution

### 4.1 Instance table

After the systems pass, the loader holds:

```rust
struct Instance {
    handle: SystemHandle,
    desc:   SystemDescriptor,
    name:   String,          // the KDL instance name (also the telemetry prefix, §6)
}
instances: HashMap<String /*instance name*/, Instance>
```

### 4.2 Resolving one endpoint `(instance, frame)` → `PortRef`

```
1. inst   = instances.get(instance_name)        else LoadError::UnknownInstance{ span }
2. fid    = ComponentId::new(frame_name)         // the same hash Frame::FRAME_ID uses
3. // validate the frame name is actually a port of the right direction on this instance:
   producer side: inst.desc.outputs.iter().any(|p| p.frame_id == fid)
   consumer side: inst.desc.inputs .iter().any(|p| p.frame_id == fid)
       else LoadError::UnknownFrame{ instance, frame, span }
4. PortRef { system: inst.handle, frame_id: fid }
```

Step 2 is the **frame-name → `ComponentId`** registry the WP5 report calls for: it is simply
`ComponentId::new(name)`, the identical construction `Frame::FRAME_ID` performs at compile time
(`src/frame.rs`), so a KDL string and the Rust frame name land on the same `ComponentId` with no
shared table to keep in sync. (`metor-proto-kdl`'s `parse_component_monitor` already does exactly
`node.get("component_id").map(ComponentId::new)`.)

Step 3 is the **typo guard**: because the descriptor enumerates every port's `frame_id`, a misspelled
or wrong-direction frame name is caught **at load with a source span**, not silently ignored —
satisfying the WP5 requirement that "a typo is a load error, not a silent miss." (Note `PortRef::new::<F>`
exists for the code-first path but takes a Rust type; the loader can't name `F`, so it constructs
`PortRef { system, frame_id }` directly — both fields are public.)

### 4.3 Connecting and surfacing `WireError`

```rust
let p = resolve_producer(edge.from, edge.out)?;
let c = resolve_consumer(edge.to,   edge.in)?;
builder.connect(p, c).map_err(|e| LoadError::Wire { source: e, span: edge.span })?;
```

`connect` returns early `WireError::{UnknownSystem, FrameIdMismatch}` (and `UnknownPort` etc. are
caught later at `build()`). Each is wrapped with the originating `connect` node's span so the diagnostic
points at the offending line. Likewise `builder.build()` returns `WireError::{UnknownPort,
Incompatible, UnconnectedInput, DoubleConnect}`; the loader maps these back to a source span where it
can (it knows which edge/instance each frame_id+system came from — §5.2).

### 4.4 Dynamic frames need no special handling

A producer of a `FrameList`/`FrameMap`-bearing frame (e.g. `procmon`) is wired exactly like any other:
the edge names the frame, `ComponentId::new` resolves it, and `compatible()` (inside `build()`)
compares the **realized** VTable fields, including dynamic member templates (registration mode —
`realize_set` in `src/descriptor.rs`). The wiring layer is oblivious to the dynamism; it lives inside
the frame.

---

## 5. The loader API

### 5.1 Entry point

```rust
/// Parse a KDL wiring document, instantiate every system from `registry`, connect the
/// edges, and return a built coordinator ready to `run`.
pub fn load(kdl: &str, registry: &Registry) -> Result<Coordinator, LoadError>;
```

Pseudo-flow:

```rust
let doc = kdl.parse::<KdlDocument>().map_err(LoadError::parse)?;     // kdl crate, like metor-proto-kdl
let config = parse_coordinator(&doc)?;                              // §1.1 → CoordinatorConfig
let mut builder = Coordinator::builder(config);
let mut instances = HashMap::new();

for node in doc.nodes().filter(|n| n.name().value() == "system") {  // systems pass (§2)
    let inst_name = first_entry_string(node).ok_or(LoadError::MissingInstanceName{..})?;
    let ty = node.get("type").and_then(|v| v.as_string()).ok_or(LoadError::MissingType{..})?;
    let factory = registry.factories.get(ty).ok_or(LoadError::UnknownType{ ty, span })?;
    let (handle, desc) = factory(&mut LoadCtx { node, src: kdl, builder: &mut builder })?;
    if instances.insert(inst_name, Instance{ handle, desc, .. }).is_some() {
        return Err(LoadError::DuplicateInstance{ .. });
    }
}

for node in doc.nodes().filter(|n| n.name().value() == "connect") { // edges pass (§4)
    let edge = parse_edge(node)?;
    let p = resolve(&instances, edge.from, edge.out, Dir::Out)?;
    let c = resolve(&instances, edge.to,   edge.in,  Dir::In)?;
    builder.connect(p, c).map_err(|e| LoadError::wire(e, edge.span))?;
}

builder.build().map_err(|e| LoadError::wire_at_build(e, &instances))  // §4.3 / §5.2
```

The loader holds the source string for the lifetime of the parse so every error carries
`#[source_code]` + span, exactly as `metor-proto-kdl` does.

### 5.2 `LoadError` — every failure with source context

```rust
#[derive(Error, Diagnostic, Debug)]
pub enum LoadError {
    Parse{ source: kdl::KdlError, #[source_code] src, #[label] span },        // bad KDL
    UnknownType{ ty, #[source_code] src, #[label] span },                     // §2: no registry entry
    BadParams{ source: /* FromKdlNode err */, #[source_code] src, #[label] span }, // §3
    MissingInstanceName{ .. } | MissingType{ .. } | DuplicateInstance{ .. },  // §1.2
    UnknownInstance{ name, #[source_code] src, #[label] span },               // §4.2 step 1
    UnknownFrame{ instance, frame, #[source_code] src, #[label] span },       // §4.2 step 3 (typo guard)
    Wire{ #[source] source: WireError, #[source_code] src, #[label] span },   // §4.3: WP5 errors
    MissingCoordinator | MultipleCoordinators{ .. },                          // §1.1
}
```

This mirrors `KdlSchematicError` (`metor-proto-kdl/src/lib.rs`) — same `thiserror`+`miette`
derive, same `#[source_code]`/`#[label]` pattern — so diagnostics render with the offending KDL line
highlighted. The five `WireError` variants WP5 already defines are wrapped in `LoadError::Wire`;
where the error names a `frame_id`/system id, §5.2's instance table lets the loader translate it back
to the human instance + frame name and the source span (a `build()` error like `Incompatible{ producer,
consumer, frame_id }` knows the system *names* already, and the loader maps those to the `connect`
node that introduced the edge).

### 5.3 Code-first builder remains the primary path

The KDL loader is **optional sugar over the builder**, which stays the canonical API (the WP5 tests
in `tests_coordinator.rs` wire everything code-first via `add_cyclic`/`connect`/`PortRef::new::<F>`).
KDL buys runtime reconfiguration without recompiling and a reviewable mission description; the typed
builder buys compile-time port checking (`PortRef::new::<F>` proves the frame exists at compile time,
where the loader can only check at load). Both produce the identical `Coordinator`. (Q5: is KDL a
hard requirement for v1, or a fast-follow, given the builder already satisfies every WP5 test?)

---

## 6. The frame_id collision across two instances of the same system type (first-class problem)

**The problem.** `Frame::FRAME_ID = ComponentId::new(Frame::NAME)` is baked into the *type* at compile
time. Two instances of the same system type — `system "imu_left" type="ImuDriver"` and `system
"imu_right" type="ImuDriver"` — both produce the frame named `"imu"`, hence **the same
`ComponentId`**. This is a real collision the design must address.

**Where it does *not* break: wiring.** WP5 addresses ports by `(SystemHandle, frame_id)`, and the two
instances have **distinct `SystemHandle`s**. Internally `build()` keys fan-out and edges by
`(system_id, port_idx)`, never by a global `frame_id` (see `cons_edge`/`fan_out` in
`coordinator.rs`), so two producers sharing a `frame_id` are unambiguous. The loader's instance table
keys on the **instance name**, so `connect from="imu_left"` and `connect from="imu_right"` resolve to
different handles. **Wiring is collision-free** because the instance name disambiguates the producer
and the `SystemHandle` carries that through.

**Where it *does* break: identity at the db/telemetry sink.** Both instances emit components under the
frame path `imu.omega` (the `Frame::NAME` prefix). At the db, instance-left's `imu.omega` and
instance-right's `imu.omega` are **the same fully-qualified component** — they collide. This is
exactly the `<NAME>` prefixing WP5 **deferred to the sink** (coordinator.md §5.4, Q6), and WP6 is where
the disambiguating name actually exists.

**The resolution: the prefix is the KDL instance name, not `Frame::NAME` and not `System::NAME`.**
WP5 proposed prefixing each system's frames with `System::NAME` — but `System::NAME` is a property of
the *type* (`"ImuDriver"`), so it is **identical** for two instances and does **not** disambiguate.
WP6 refines this: the disambiguator is the **instance name** from KDL (`"imu_left"` / `"imu_right"`),
which is unique by construction (§1.2). Concretely:

- The loader records, per system, its instance name in the instance table (§5.2). It passes this
  name down to the coordinator's per-buffer ownership record (the `BufferRole::Output { system, .. }`
  in `coordinator.rs` already identifies the owning system; WP6/WP5 associates that system index with
  its instance name).
- The **telemetry sink applies `<instance-name>.` as the path prefix** when ingesting that buffer's
  records into db (so `imu_left.imu.omega` vs `imu_right.imu.omega`) — applied at ingest, per
  instance, not baked into the on-ring frame bytes (whose name stays the fixed `Frame::NAME`). This
  is precisely WP5 Q6, with the prefix source corrected from `System::NAME` to the instance name.
- For the **code-first builder** (no KDL), the instance name defaults to `System::NAME`; a future
  `add_cyclic_named(name, system)` overload supplies an explicit instance name for the
  two-instances-of-one-type case. (Q2 asks the reviewer to confirm: instance-name prefix at the
  sink, and whether the builder needs the `_named` overload now or can defer it until a mission
  actually instantiates a type twice.)

This ties the deferred per-system prefix to the one place a unique human name for each system
instance exists — the KDL document.

---

## 7. Reused vs. new

| Concern | Reused (landed) | New in WP6 |
|---|---|---|
| Wiring / validation / sizing | the **entire** WP5 builder: `builder()`, `add_cyclic`/`add_async`/`connect`/`build`, `PortRef`, `SystemHandle`, `WireError`, `compatible` | nothing — WP6 only *drives* it |
| KDL parsing | the `kdl = "6.3.4"` crate + `metor-proto-kdl`'s hand-walk style (`node.get`, `entries`, `span`) | the wiring document grammar (§1) |
| Frame addressing | `ComponentId::new(name)` (== `Frame::FRAME_ID`); `SystemDescriptor`/`PortDesc.frame_id` | the frame-name→`ComponentId`→`PortRef` resolver + typo guard (§4) |
| Error reporting | `thiserror`+`miette` `Diagnostic` with `#[source_code]`/`#[label]` (`KdlSchematicError`) | `LoadError` variants + mapping `WireError` back to spans (§5.2) |
| Param config | `metor-proto-kdl`'s per-node `parse_*` walk | `FromKdlNode` trait + derive; per-system `Params` (§3) |
| System construction | `System::new(params)` (WP4 §1.2 "constructible before init") | the `Registry`, `SystemFactory`, `RegisteredSystem` + blanket cyclic/async impls (§2) |
| Telemetry namespacing | health-as-frames; the deferred `<NAME>` prefix (WP5 §5.4) | the prefix is the **instance name**, recorded by the loader, applied at the sink (§6) |

Genuinely new code: the KDL schema, the registry + registration macro, the `FromKdlNode` param-deser
trait/derive, the edge resolver, and the `load()` loader with span-carrying `LoadError`. The builder,
validation, sizing, addressing, and parsing dependency are all reuse.

---

## 8. Open questions / risks for the reviewer

1. **Q1 — registration mechanism: explicit table vs `inventory`.** Proposed: an app-built `Registry`
   (each crate exposes `register(&mut Registry)`), **not** `inventory`/`linkme` global collection —
   justified by determinism/auditability for FSW, the dead-code-elimination footgun of link-section
   registration, test isolation, and the fact that `inventory` is only a transitive dep today while
   `metor-proto-kdl` already uses explicit dispatch. Confirm explicit-table; the trait/factory design
   is identical either way.
2. **Q2 — the frame_id-collision / instance-name prefix (the headline risk).** Proposed: wiring is
   already collision-free (distinct `SystemHandle`s); the collision is at the db sink, resolved by
   prefixing each system's frames with its **KDL instance name** (correcting WP5's `System::NAME`,
   which doesn't disambiguate two instances), applied at ingest, not in the frame bytes. Confirm the
   prefix source is the instance name and the application site is the sink. Does the **code-first**
   builder need an `add_cyclic_named(name, sys)` overload now, or can the `name == System::NAME`
   default stand until a mission instantiates one type twice?
3. **Q3 — cyclic/async declared by type only.** Proposed: KDL does **not** carry `kind`; the factory
   knows from which system trait `S` implements. Acceptable, or do we want an advisory `kind="async"`
   property cross-checked against the type (documentation value vs. a way for the document to
   contradict the code)?
4. **Q4 — edge syntax.** Proposed: `connect "a" -> "b" frame="f"` shorthand plus the explicit
   `from/to/out/in` form. Since `connect` requires matching `frame_id`, `out` and `in` must hash
   equal — is the asymmetric `out`/`in` form worth keeping, or should an edge always name a single
   `frame`?
5. **Q5 — is KDL in-scope for v1 at all?** The WP5 builder already satisfies every acceptance test
   code-first. Is the KDL loader a v1 deliverable, or a fast-follow with the builder as the only v1
   surface? (Affects how much of §3's derive machinery we build now.)
6. **Q6 — `FromKdlNode` now vs. wait for `facet-kdl`.** Proposed: hand-walk like `metor-proto-kdl`
   behind a `FromKdlNode` trait + derive, retargetable onto `facet-kdl` later. Acceptable to ship the
   hand-walk derive now, or hold params-deser until `facet-kdl` lands (per `metor-proto-kdl`'s own
   README intent)?
7. **Q7 — build-time `WireError` → span mapping.** `build()` returns errors naming `frame_id` /
   system *names*, not the KDL node. The loader can translate via its instance table (§5.2), but the
   mapping is best-effort (e.g. `UnconnectedInput` names a system+frame with no originating `connect`
   node to point at — it would highlight the `system` node instead). Is best-effort span mapping
   acceptable, or must every `build()` error pinpoint an exact source span?
