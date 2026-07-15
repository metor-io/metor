# Wiring — describing and building a mission

> **Phase 4 update.** The KDL front-end this document originally described was
> deleted (`python-config-phase4-plan.md`). Missions are now Python
> (`mission.py`), evaluated into the versioned `Wiring` IR — see
> `design-python-config.md`. The `Wiring` data model, the resolver, the static
> registry, the build driver, and the Rust `WiringBuilder` are all unchanged;
> only the front-end that produces a `Wiring` changed. The **KDL schema**
> section below (§2) and the KDL-specific narration in §5/§6.3 are **historical**
> — the grammar is gone (`design-kdl-serde.md`, also historical). Everything
> else still reflects the shipped resolver.

A mission is a graph of systems and the edges between them, plus the coordinator config and an
optional telemetry downlink. The wiring layer is how that graph is **described**, **serialized**,
and **built** into a runnable [`Coordinator`].

It is a pure front-end onto the landed [`CoordinatorBuilder`] (`src/coordinator/`): it instantiates
systems, calls `add_*`/`connect`, and `build()`s. No coordinator logic lives here. Every failure is
a span-carrying [`LoadError`] — a `miette` `Diagnostic` that anchors on a spec's `SourceRef`.

The wiring module lives at `src/wiring/` and is gated behind the `wiring` cargo feature (default-on;
the lighter `wiring-model` feature exposes just the `Wiring` IR in `src/ir.rs`):

- `src/wiring/mod.rs` — the shared resolver (`resolve`), the static system [`Registry`], and the dl
  schema-guided value-tree param encoder (`encode_value_params`).
- `src/wiring/py.rs` — the subprocess-CPython evaluation path (`eval_python_mission`), which runs a
  `mission.py` against the embedded `metor_config` recorder and ingests the emitted IR.
- `src/wiring/params.rs` — the schema-guided value-tree → postcard encoder for dl params.
- `src/ir.rs` — the serializable [`Wiring`] data model (feature `wiring-model`).
- `src/wiring/builder.rs` — the fluent Rust [`WiringBuilder`] front-end.
- `src/wiring/build_driver.rs` — the cargo build driver that locates each artifact's `.so`.
- `src/wiring/bundle.rs` — the relocatable bundle writer/loader.
- `src/wiring/tests.rs` — the wiring unit/acceptance tests.

---

## 0. The shape of it: two front-ends, one data model, one resolver

```
        ┌──────────────────┐
  .py ──┤ eval_python_     ├──┐
        │   mission        │  │   ┌─────────┐      ┌──────────┐      ┌─────────────┐
        └──────────────────┘  ├──▶│ Wiring  │─────▶│ resolve  │─────▶│ Coordinator │
        ┌──────────────────┐  │   └─────────┘      └──────────┘      └─────────────┘
  Rust ─┤   WiringBuilder  ├──┘  (data model)      (shared)
        └──────────────────┘
```

[`Wiring`] (`src/ir.rs`) is the single source of truth for a mission: a plain, serializable
(`Serialize`/`Deserialize`) Rust description with **no runtime types** in it. There are two
front-ends that produce a [`Wiring`] and exactly one resolver that consumes it:

- [`eval_python_mission`]`(path: &Path) -> Result<Wiring, _>` — runs a `mission.py` under a
  subprocess CPython against the embedded `metor_config` recorder and ingests the versioned IR JSON
  it emits. Evaluation only: it touches no [`Registry`], `dlopen`s nothing, and does not validate
  the graph. See `design-python-config.md`.
- [`WiringBuilder`] — a fluent Rust builder producing the same [`Wiring`]. Anything a `mission.py`
  can express, the Rust builder can express, because both target this type.
- [`resolve`]`(wiring: &Wiring, registry: &Registry) -> Result<Coordinator, LoadError>` — walks a
  [`Wiring`], instantiates every system (static through the [`Registry`], dl by `dlopen`), connects
  the edges, and `build()`s. The validation/sizing/telemetry passes are identical
  for static and dl systems and for both front-ends.

Because [`Wiring`] is format-independent and serializable, a mission can be authored in Python or in
Rust, round-tripped through serde, persisted (frozen in a bundle), or shipped, and resolved
identically. The IR carries an `ir_version` (currently 2) checked on every consumption path.

---

## 1. The `Wiring` data model

[`Wiring`] (`src/ir.rs`) and its members are deliberately decoupled from the runtime types — a
[`ClockSpec`] mirrors [`ClockMode`] without holding a `Duration`, a [`CoordinatorSpec`] mirrors
[`CoordinatorConfig`] without a clock value, etc. The conversion to runtime types happens in
[`resolve`], so the model stays a pure serde data format.

```rust
pub struct Wiring {
    pub coordinator: CoordinatorSpec,   // cycle rate, default depth, clock
    pub artifacts:   Vec<Artifact>,     // the cdylibs this mission loads
    pub systems:     Vec<SystemSpec>,   // the system instances
    pub slots:       Vec<SlotSpec>,     // runtime-loadable slots
    pub edges:       Vec<EdgeSpec>,     // producer → consumer edges
}
```

The telemetry downlink and the command uplink are **ordinary systems** here — instances of
the built-in registry types (`TCP_DOWNLINK_TYPE` = `"TcpDownlink"`, `TCP_UPLINK_TYPE` =
`"TcpUplink"`), not dedicated fields. `SystemSpec::tcp_downlink`/`tcp_uplink` construct the
specs the builder sugar and the CLI flags push.

### 1.1 `CoordinatorSpec` / `ClockSpec`

```rust
pub struct CoordinatorSpec {
    pub cycle_rate:    f64,            // Hz, held under a Wall clock
    pub default_depth: Option<usize>, // None ⇒ framework DEFAULT_DEPTH
    pub clock:         ClockSpec,
}

pub enum ClockSpec {
    Wall,                           // paced to cycle_rate
    Simulated { dt_secs: f64 },     // free-running, advancing dt_secs each cycle
}
```

[`resolve`] converts this into a [`CoordinatorConfig`]: `ClockSpec::Wall` → [`ClockMode::Wall`],
`ClockSpec::Simulated { dt_secs }` → [`ClockMode::Simulated`] with `Duration::from_secs_f64(dt_secs)`.

### 1.2 `Artifact`

A loadable shared object — "which `.so`, and what crate it comes from". Each cdylib
exports one **pack** — any number of system types — through the fixed `fsw_pack_*` ABI
symbols (`docs/packs.md`); a `system` node's `type=` selects an entry from the opened
pack's manifest. Multiple [`SystemSpec`]s may reference one `Artifact` (and one entry) to
instance it more than once; the loader opens the object once per resolve and runs the
create phase per instance.

```rust
pub struct Artifact {
    pub id:         String,        // what SystemSpec::artifact references
    pub crate_name: String,        // cargo package, for `cargo build -p <crate_name>`
    pub cdylib:     String,        // produced file name (libfoo.so / libfoo.dylib / foo.dll)
    pub path:       Option<PathBuf>, // resolved location, filled by the build driver
}
```

`path` is `None` until the build driver (§5) builds/locates the `.so`; [`resolve`] errors with
[`LoadError::ArtifactNotBuilt`] if a dl system's artifact still has no path.

### 1.3 `SystemSpec` / `ParamSource`

```rust
pub struct SystemSpec {
    pub name:     String,           // instance name (telemetry prefix, §7)
    pub ty:       Option<String>,   // type= key (registry key, or the pack entry name)
    pub artifact: Option<String>,   // Some(id) ⇒ loaded from that pack; None ⇒ static
    pub params:   ParamSource,
    pub process:  bool,             // run the artifact in its own worker process
}

pub enum ParamSource {
    None,              // a paramless system
    Postcard(Vec<u8>), // canonical postcard Params bytes (the typed Rust builder path)
    Value(Value),      // a params value tree (the Python front-end's format)
}
```

`artifact` is the static-vs-dl switch: `None` resolves through the [`Registry`], `Some(id)`
`dlopen`s that [`Artifact`]. [`ParamSource`] is an explicit three-way so the cases never overload a
single `Vec<u8>`. A `Value` tree becomes canonical postcard `Params` bytes (dl, via
`encode_value_params`) or feeds a serde deserialize into the typed `S::Params` (static) at resolve
time — see §5. (Before Phase 4 a fourth `Kdl(String)` variant carried a KDL node's source text;
it was dropped with the KDL front-end, bumping `ir_version` to 2.)

### 1.4 `EdgeSpec`

```rust
pub struct EdgeSpec {
    pub from:    String,            // producer instance name
    pub out:     String,            // producer output port frame name
    pub to:      String,            // consumer instance name
    pub in_:     String,            // consumer input port frame name
    pub delayed: bool,              // true ⇒ a one-cycle-delayed feedback back-edge
}
```

`delayed` selects [`CoordinatorBuilder::connect_delayed`] over `connect`: the back-edge of a control
loop, excluded from cycle detection (§4.3).

---

## 2. The KDL schema (historical)

> **Historical.** This section documents the KDL grammar the deleted front-end
> parsed. It is retained for the record only; missions are authored in Python
> today (`design-python-config.md`), and the same graph is expressed with the
> [`WiringBuilder`] in Rust. The `Wiring` node kinds it describes
> (`coordinator`/`artifact`/`system`/`slot`/`connect`) survive as IR concepts;
> only their KDL surface syntax is gone.

A KDL wiring document has up to five node kinds at the top level: exactly one `coordinator`,
N `artifact` nodes, N `system` nodes, N `slot` nodes, and M `connect` edges. (The
pre-normalization `telemetry`/`uplink` blocks surface a guidance error carrying the
`system` spelling that replaced them.)
Conventions follow `metor-proto-kdl` (lowercase node names, `key=value` properties, KDL v2 `#true`
booleans). Params and config are **properties on the node line**, not a `{ key=value }` children
block (the latter is not valid KDL v2).

### 2.1 `coordinator`

```kdl
coordinator cycle_rate=200.0 default_depth=8            // wall clock, paced
coordinator cycle_rate=200.0 sim_dt=0.00833            // simulated clock, free-running
```

Exactly one `coordinator` node ([`MissingCoordinator`] / [`MultipleCoordinators`] otherwise).
`cycle_rate` (Hz) is required; `default_depth` is optional. A `sim_dt` (seconds) property selects a
[`ClockSpec::Simulated`] clock with that logical per-cycle step (the loop free-runs); absent ⇒ a
paced [`ClockSpec::Wall`] clock holding `cycle_rate`.

### 2.2 `artifact` (dl systems only)

```kdl
artifact "adcs" crate="adcs-systems" lib="adcs_systems"
```

- The first (nameless) argument, `"adcs"`, is the artifact **id** a `system`'s `artifact=`
  references.
- `crate=` is the cargo package the build driver compiles.
- `lib=` is the library **stem**; the framework decorates it to the platform's produced cdylib
  file name (`libadcs_systems.dylib`/`.so` / `adcs_systems.dll`) so one document is portable
  (cli-runner.md §4.6).
- There is **no** `type=` — a pack exports many system types, and the `system` node picks
  the entry. The pre-pack spelling gets a pointed error ([`LoadError::ArtifactType`]:
  "packs export many systems; name the system on the `system` node").

Maps one-to-one onto [`Artifact`] (`path` filled later by the build driver). A static-only mission
needs no `artifact` nodes.

### 2.3 `system`

```kdl
system "imu" type="ImuDriver" i2c_bus=1 sample_hz=200.0     // static
system "plant" type="Plant" artifact="adcs" gain=5.0        // loaded (references artifact "adcs")
```

- The first (nameless) argument, `"imu"`, is the **instance name** — the unique handle the rest of
  the document refers to, and the telemetry prefix (§7). It is not the type name; two instances of
  one type get two distinct instance names. Duplicates are a [`DuplicateInstance`] error.
- `type=` is the **registry key** (static) or the **pack entry name** (loaded). It is
  required for a static system ([`MissingType`] otherwise); for a loaded one it may be
  omitted iff the artifact's pack exports exactly one entry ([`LoadError::PackTypeRequired`]
  otherwise, listing the choices), and an unknown entry name is a clean error listing the
  pack's exports.
- `artifact=` (optional) makes this a **dl** system referencing that `artifact` id; absent ⇒ a
  **static** system resolved through the [`Registry`]. (This property was originally spelled `lib=`
  — same as the `artifact` node's own stem property — and was hard-renamed to `artifact=` so the
  two could not be confused; there is no `lib=` alias.)
- `process=#true` (optional, requires `artifact=`; [`ProcessNeedsArtifact`] otherwise) runs the
  artifact in its **own worker process** instead of dlopen'ing it in-process
  (`docs/process-systems.md`): resolve obtains its descriptor from a describe-mode worker, and
  `build()` spawns the run worker. Everything else about the node — `type=` agreement, params,
  edges — is identical to a dl system. `WiringBuilder`'s `.process()` is the Rust twin. The
  same property on a `slot` node runs the slot's **occupants** out of process, one worker per
  `Load` (`docs/process-slots.md`): resolve describes every allowed occupant via a worker (no
  artifact requirement — occupants are always artifact-backed), and no worker exists until a
  `Load` picks one. `SlotSpecBuilder::process()` is that surface's Rust twin.
- Every other property (anything but the reserved `type=`/`artifact=`/`process=`) is a **param** (§6). A system
  with any config property carries [`ParamSource::Kdl`] (its node source text, verbatim); a
  config-less system carries [`ParamSource::None`]. A repeated property or an unrecognized top-level
  node is a load error (stricter than KDL's own last-wins).

Cyclic vs async is **not** declared in KDL — it is a property of the Rust type (`impl CyclicSystem`
vs `impl AsyncSystem`), and the factory knows which `add_*` to call (§3.2). The document cannot
contradict the type.

### 2.4 `connect`

An edge names a producer endpoint and a consumer endpoint, each as `(instance, port)`. There is
**one** `connect` node kind; whether it is a **frame** edge or a **message** edge is inferred from
which property is present — `frame=` or `msg=` (`EdgeSpec::kind: EdgeKind { Frame, Msg }`,
`docs/message-wiring.md` §3.4). Two syntaxes desugar to the same [`EdgeSpec`]:

```kdl
connect "imu" -> "nav" frame="imu"                  // shorthand, frame edge (the common case)
connect from="nav" out="nav" to="log" in="nav"      // explicit long form
connect "ctrl" -> "plant" frame="torque_cmd" delayed=#true   // feedback back-edge
connect "uplink" -> "mode" msg="SequenceCommand"    // message edge (many-to-many, no cycle check)
```

- Shorthand: nameless `"from"`, an optional `->`, `"to"`, plus `frame="…"`/`msg="…"` (which becomes
  both `out` and `in`).
- Explicit: `from`/`to` instance names and `out`/`in` port names. The asymmetric form exists for the
  rare producer/consumer name mismatch, but since `connect` requires the two ports' edge keys to
  match (`F::FRAME_ID` for a frame edge, `M::ID` for a message edge), `out` and `in` must hash
  equal.
- `delayed=#true` marks the edge a one-cycle-delayed feedback back-edge (default `#false`; frame
  edges only — message edges are excluded from feedback-cycle detection in the first place, so
  `delayed` is meaningless on one).
- A **message** edge (`msg=`) is many-to-many (an input may take zero, one, or many producers,
  unlike a frame input's exactly-one rule) and does not participate in cycle detection — a command
  channel is a decoupled event bus, not a same-cycle data dependency. The `WiringBuilder` exposes
  the same two kinds as `connect`/`connect_delayed` (frame) and `connect_msg` (message) — an
  ergonomic split kept in the Rust front-end even though the lower-level
  `CoordinatorBuilder`/`PortRef` API needs only one `connect`, inferring the kind from the
  `PortRef`'s `PortId` (`Component` vs `Packet`).

### 2.5 The downlink/uplink built-ins

```kdl
system "telemetry" type="TcpDownlink" addr="127.0.0.1:2240" {
    instances "nav" "imu"      // optional subset children; omit both to tap everything
    frames "gyro_b"
}
system "uplink" type="TcpUplink" addr="127.0.0.1:2241"
```

Ordinary `system` nodes of the built-in registry types (telemetry.md §8, messages.md §4.4)
— `"telemetry"`/`"uplink"` are conventional instance names, not reserved words, and several
instances are legal. The resolver defers static `ReceiveAll` systems (the downlink, the
alarm engine) behind every other cyclic registration, so document position is free.

### 2.6 Worked example

```kdl
coordinator cycle_rate=200.0

system "imu" type="ImuDriver" i2c_bus=1 sample_hz=200.0   // produces `imu`
system "nav" type="NavFilter" gain=2.0                    // consumes `imu`, produces `nav`
system "log" type="NavLogger"                             // async, no params, consumes `nav`

connect "imu" -> "nav" frame="imu"
connect "nav" -> "log" frame="nav"
```

`imu`→`nav` is a cyclic chain; `nav`→`log` crosses into an async consumer (the coordinator
allocates the private copy-in buffer at `build()` automatically — nothing async-specific appears in
KDL). Edges never mention buffer sizes, `max_readers`, or health/log ports — all of that is the
coordinator's job at `build()`.

---

## 3. The static system registry

### 3.1 The reflection gap

`builder.add_cyclic_named::<S>(name, s)` is generic over a concrete `S` chosen at **compile** time;
KDL names the type with a **runtime** string. Bridging the two needs a value the resolver can look
up by string and call without naming `S` — a boxed factory `fn` pointer per concrete system. The
[`Registry`] is the map of those factories:

```rust
pub struct Registry { /* HashMap<&'static str, RegistryEntry { factory, descriptor }> */ }

pub type SystemFactory =
    Box<dyn Fn(&mut LoadCtx) -> Result<(SystemHandle, SystemDescriptor), LoadError>>;

impl Registry {
    pub fn new() -> Self;
    pub fn register<S, K>(&mut self, type_name: &'static str) -> &mut Self
    where S: BuildSystem + AddToBuilder<K>, S::Params: serde::de::DeserializeOwned;
    /// Every entry of a pack under its entry name as the `type=` key, so the
    /// same `pack()` a cdylib exports serves a statically-linked mission.
    pub fn register_pack(&mut self, pack: Pack) -> &mut Self;
}
```

The factory is a boxed `Fn`, not a bare `fn` pointer, because a pack entry's factory
closes over the shared entry it instantiates (`register_pack` wraps each `PackEntry` in an
`Rc<RefCell<_>>` and drives `CoordinatorBuilder::add_pack_entry`).

The factory captures no state; given the node and the builder it does the whole
"params → `new` → `add_*_named`" dance for one concrete type, returning the [`SystemHandle`] plus the
[`SystemDescriptor`] (so the resolver can validate edge frame names against the real port list, §4).

### 3.2 How a system opts in

Construction is split in two:

- [`BuildSystem`] (`src/system/`) is the **format-independent** contract: `type Params` + `fn new`,
  with no KDL coupling. This is what a pack entry's create phase (and so the dlopen ABI)
  also uses.
- The static factory adds only one more bound: `S::Params: serde::de::DeserializeOwned` — every
  `BuildSystem` whose `Params` derives (or hand-implements) `serde::Deserialize` can register
  statically. A dl-only system needs no `Deserialize` impl on `Params` at all (its params reach the
  host as opaque postcard bytes, §6).

The cyclic-vs-async branch is resolved at **compile** time by the [`AddToBuilder<Kind>`] trait, with
two non-overlapping blanket impls keyed on a `Kind` marker (`CyclicKind` / `AsyncKind`):

```rust
impl<S, O> AddToBuilder<CyclicKind> for S where S: CyclicSystem<Output = Out<O>> + … {
    fn add_to(self, name, b) -> SystemHandle { b.add_cyclic_named(name, self) }
    fn descriptor() -> SystemDescriptor { <S as CyclicSystem>::descriptor() }
}
impl<S> AddToBuilder<AsyncKind> for S where S: AsyncSystem + … {
    fn add_to(self, name, b) -> SystemHandle { b.add_async_named(name, self) }
    fn descriptor() -> SystemDescriptor { <S as AsyncSystem>::descriptor() }
}
```

The `K` type parameter on `register::<S, K>` is inferred from the system trait `S` implements; the
`register_system!` macro keeps the call site terse:

```rust
let mut r = Registry::new();
register_system!(&mut r, ImuDriver => "ImuDriver");   // == r.register::<ImuDriver, _>("ImuDriver")
r.register::<NavFilter, _>("NavFilter");
r.register::<NavLogger, _>("NavLogger");               // async — same call, AsyncKind inferred
```

### 3.3 An explicit table, not `inventory`

The registry is an **app-built** table — the app (or each system crate's `pub fn register(&mut
Registry)`) registers exactly the systems it links — rather than global auto-registration via
`inventory`/`linkme`. For flight software this is deterministic and auditable (the set of loadable
systems is a visible list in `main`, with no link-section side effects that dead-code elimination
could silently drop), and it keeps tests isolated (each builds its own registry).

---

## 4. Edge resolution

### 4.1 The instance table

The systems pass of [`resolve`] records, per instance name, an `Instance { handle, desc }`. A
duplicate name is a [`DuplicateInstance`] error.

### 4.2 Resolving one endpoint

Each `(instance, frame)` endpoint resolves to a [`PortRef`] (`{ system: SystemHandle, frame_id:
ComponentId }`):

1. `inst = instances.get(name)` else [`LoadError::UnknownInstance`].
2. `frame_id = ComponentId::new(frame)` — the identical const hash [`Frame`]`::FRAME_ID` performs at
   compile time, so a KDL string and the Rust frame name land on the same `ComponentId` with no
   shared table to keep in sync.
3. Validate the frame is a port of the right **direction** on that instance: producer side checks
   `inst.desc.outputs`, consumer side checks `inst.desc.inputs`. A misspelled or wrong-direction
   frame is [`LoadError::UnknownFrame`] — a typo is a load error with a span, not a silent miss.

### 4.3 Connecting and surfacing `WireError`

For each edge the resolver calls `builder.connect_delayed(p, c)` if `delayed`, else
`builder.connect(p, c)`, wrapping any early [`WireError`] (`UnknownSystem`, `FrameIdMismatch`) as
[`LoadError::Wire`]. The deferred structural checks run at `builder.build()` and surface the same
way: [`WireError`]`::{UnknownPort, Incompatible, UnconnectedInput, DoubleConnect, FeedbackCycle}`.
An unbroken feedback loop is a `FeedbackCycle`; declaring the back-edge `delayed=#true` breaks it so
the document loads.

Dynamic frames (a `FrameList`/`FrameMap`-bearing frame) need no special handling: the edge names the
frame, `ComponentId::new` resolves it, and compatibility is checked on the **realized** VTable inside
`build()`. The wiring layer is oblivious to the dynamism.

---

## 5. Param deserialization (static systems)

A static system's params reach resolve as a [`ParamSource::Value`] tree (the Python front-end's
format) and become its `Params` struct through plain `serde`: `decode_value_params::<T>(value, …)`
(`src/wiring/mod.rs`) drives `T::deserialize` over the `serde_json::Value`, with `serde_ignored`
supplying the typo guard (a key no field consumed ⇒ `UnknownParam`). `Params` is an ordinary
`#[derive(serde::Deserialize)]` struct:

```rust
#[derive(serde::Deserialize)]
struct RoundTrip {
    count:  i64,
    rate:   f64,
    label:  String,
    offset: Option<f64>,
    #[serde(default = "four")]
    depth:  i64,
}
fn four() -> i64 { 4 }
```

Because it is plain serde, `#[serde(default = …)]` field attributes are honored, an `Option<T>`
absent ⇒ `None`, and floats accept an integer literal (`rate: 200` ⇒ `200.0` through serde_json).
A paramless system carries [`ParamSource::None`], decoded through a small `NoParams` deserializer
that yields unit for `()` and an empty map for a defaulted struct (so a required field absent is a
clean missing-field error). The param schema is owned by the system crate, next to the system — the
loader never hard-codes any system's fields. A paramless system uses `type Params = ()`.

(Historically a static system's params came from a KDL node through an in-house
`serde::Deserializer` over `kdl-rs`; `src/wiring/de.rs` and `design-kdl-serde.md` are that deleted
path.)

---

## 6. The build driver and dl systems

### 6.1 The build driver

Each dl system lives in its own cdylib crate. The build driver (`build_driver.rs`) turns a
[`Wiring`]'s [`Artifact`]s into located `.so`s:

```rust
pub fn build_artifacts(wiring: &mut Wiring, opts: &BuildOptions) -> Result<(), BuildError>;
```

For each artifact it runs `cargo build -p <crate_name> --message-format=json`, scans the
`compiler-artifact` lines for the file matching `cdylib`, and writes the path into [`Artifact::path`]
so [`resolve`] can `dlopen` it. It is std-only (`std::process::Command`), idempotent, and incremental
— cargo only rebuilds stale crates. [`BuildOptions`] carries `release` and `extra_args`. Failures are
clean errors ([`BuildError`]`::{Spawn, CargoFailed, ArtifactNotFound}`), never a panic.

The driver also writes a **manifest sidecar** next to each built library — `<cdylib>.manifest`, the
raw postcard `PackManifestMsg` bytes from describing a host-runnable build — so downstream consumers
(stubgen, cross-arch resolve; today verification only) can read the pack's self-description without
running the artifact. Sourcing it executes the crate's `pack()` at build time, the same trust model
as a `build.rs`. Under a cross `--target` the crate is additionally built for the host and that twin
is described; a host build failure is a hard error, and an up-to-date sidecar already next to the
target `.so` is compared against the host's bytes (manifests must be arch-independent). Packs that
cannot build for the host opt out with `BuildOptions::manifest_sidecar = false` (CLI:
`--no-manifest-sidecar`).

### 6.2 Resolving a dl system

For a dl [`SystemSpec`], [`resolve`] finds its [`Artifact`] (else [`UnknownArtifact`]), reads
`artifact.path` (else [`ArtifactNotBuilt`]), opens the `.so` with `DlPack::open` (else
[`DlOpen`]) — **once per resolve**, through a cache keyed by artifact id, since reopening
would re-run the crate's `pack()` and fork any shared state its entries captured
(`docs/packs.md` §8) — selects the entry named by `type=` (or the pack's sole entry) as a
`DlSystem`, resolves the params to canonical postcard bytes, and registers it via
[`CoordinatorBuilder::add_dl_cyclic`]`(name, loaded, params)`. The same open serves the
param encode, the bound slot, and every other system or slot occupant the artifact backs.

### 6.3 Schema-guided value tree → postcard params (the one-encoding invariant)

The headline property is that the **same** logical params produce **byte-identical** wire bytes
whether authored in Rust or in Python — so the value-tree front-end ≡ the Rust builder on the wire.

- The Rust builder's [`SystemSpecBuilder::params`]`<P: Serialize>(p)` postcard-encodes `P` into
  [`ParamSource::Postcard`] — exactly the bytes `fsw_pack_create` decodes.
- A Python-authored dl system carries [`ParamSource::Value`] (a value tree). At resolve,
  [`encode_value_params`] schema-encodes it against the `.so`'s **exported** `Params` schema
  ([`DlSystem::params_schema`], an `OwnedNamedType` from `postcard-schema`) — the host never links
  the system's `Params` type.

`encode_value_params` (`src/wiring/params.rs`) `conform_to_schema`s the value against the `.so`'s
schema (so JSON object order becomes canonical, matching the typed builder's struct-field order —
the basis of the byte equality), recursing per field with `conform_value`, and hands the conformed
value to `postcard_dyn::to_stdvec_dyn`. Only a top-level struct of scalar fields (or a unit) maps
from a flat value tree. The errors are span-aware (`src/wiring/mod.rs`):

- [`UnknownParam`] — a property with no matching schema field.
- [`MissingParam`] — a non-`Option` schema field with no property (the schema has no defaults; an
  `Option` field with no property encodes as `None`).
- [`InvalidParam`] — a property whose value type does not match the field.
- [`DlParamEncode`] — an un-encodable schema shape, or the dynamic encoder rejected the value.

The byte equality holds against the typed builder's `Postcard` bytes for the same logical value,
which is what makes the two front-ends interchangeable on the wire.

---

## 7. Instance-name disambiguation and the telemetry prefix

[`Frame`]`::FRAME_ID = ComponentId::new(Frame::NAME)` is baked into the **type** at compile time, so
two instances of one system type — `system "imu_left" type="ImuDriver"` and `system "imu_right"
type="ImuDriver"` — both produce a frame named `"imu"`, hence the same `ComponentId`.

**Wiring is collision-free.** Ports are addressed by `(SystemHandle, frame_id)`, and the two
instances have distinct `SystemHandle`s. The coordinator keys fan-out and edges by `(system, port)`,
never by a global `frame_id`, and the instance table keys on the unique instance **name**, so
`connect from="imu_left"` and `connect from="imu_right"` resolve to different handles.

**Identity at the telemetry sink is disambiguated by the instance name.** Each system is added under
its instance name (`add_cyclic_named`/`add_async_named`/`add_dl_cyclic` all take a `name`), and that
name is the path prefix: outputs register under `ComponentId::new("<instance>.<frame>")`
(`imu_left.imu` vs `imu_right.imu`). The instance name — unique by construction — is the
disambiguator, not `System::NAME` (which is a property of the type and identical for both instances).
`Coordinator::output_instances()` exposes the `(instance_name, frame_id)` pairs, and the output
registry (`src/registry.rs`) indexes each tappable buffer by its instance-qualified id.

---

## 8. Errors

[`LoadError`] is a `thiserror` + `miette` `Diagnostic`, each variant carrying `#[source_code]` +
`#[label]` so diagnostics render with the responsible mission line highlighted (the spec's
`SourceRef`, a `mission.py:line` anchor, when it has one). The variants:

| Variant | When |
|---|---|
| `IrVersionMismatch` | a `Wiring` stamped with a different `ir_version` than this build |
| `MissingType` / `DuplicateInstance` | a static system with no `type`, or two systems sharing a name |
| `UnknownType` | `type` not in the registry |
| `UnknownParam` / `MissingParam` / `InvalidParam` / `ValueParams` | a param key with no schema field / a required field absent / a type mismatch / a serde decode failure — shared by the static deserialize (§5) and the dl schema-conform pass (§6.3) |
| `UnknownInstance` / `UnknownFrame` | edge endpoint resolution (§4.2) |
| `Wire` | a `WireError` from `connect`/`build()` (§4.3) |
| `UnknownArtifact` / `ArtifactNotBuilt` / `DlOpen` / `StaleStubs` | dl artifact resolution and stub freshness |
| `PackTypeRequired` / `PackSystem` / `PackCreate` / `OccupantNotReloadable` | pack surface: a multi-entry pack with no `type`; an unknown entry name (the choices are listed); an entry's create phase failed; a `.state(...)` entry named as a slot occupant or second instance |
| `StaticPostcardParams` | typed builder (`Postcard`) params on a static system, which has no postcard decode path |
| `DlParamEncode` | dl param encoding: an un-encodable schema shape, or the dynamic postcard encoder rejected the value (§6.3) |

When [`resolve`] consumes a [`Wiring`], it anchors each error on the spec's `SourceRef` when it has
one (a Python-evaluated mission fills these with `mission.py:line` anchors; a builder-origin
[`Wiring`] has none), falling back to a best-effort reconstructed snippet — but the error
**variant** (what callers and tests match on) is identical either way. `build()`-time `WireError`s
are wrapped with the error's own rendered message as the snippet.

---

## 9. Reused vs. new

| Concern | Reused | New here |
|---|---|---|
| Wiring / validation / sizing | the coordinator builder: `add_*_named`, `connect`/`connect_delayed`, `build`, `PortRef`, `SystemHandle`, `WireError` | nothing — wiring only drives it |
| Mission authoring | Python (`metor_config`, evaluated by CPython) | the value-tree IR the recorder emits (`design-python-config.md`) |
| Frame addressing | `ComponentId::new(name)` (== `Frame::FRAME_ID`); `SystemDescriptor`/`PortDesc.frame_id` | the resolver + typo guard (§4) |
| Error reporting | `thiserror` + `miette` `Diagnostic` | `LoadError` (§8) |
| dl ABI | `DlPack`/`DlSystem`, `export_pack!`, the `fsw_pack_*` symbols, `params_schema` | the build driver, the schema-guided value-tree encoder (§6), the per-resolve pack cache |
| System construction | `BuildSystem::new` | `Registry`, `SystemFactory`, `AddToBuilder`, the value-tree deserialize (§3, §5) |
| Mission description | — | the `Wiring` data model + `WiringBuilder` + `eval_python_mission`/`resolve` (§0, §1) |

---

## Tests

- `src/wiring/tests.rs` — the in-crate acceptance suite, all driven through [`WiringBuilder`]:
  an end-to-end build + run of a params-bearing cyclic chain into an async logger; instance-name
  disambiguation of two instances of one type; the span-carrying error cases; feedback-cycle
  detection and the `delayed` break; a `TcpDownlink` system loading; the value-tree param decode;
  the IR-version check; and bundle round-trips.
- `tests/wiring_resolve.rs` — the dl acceptance gate driven **through** the [`Wiring`] data model: a
  static producer + a dlopen'd consumer, the build driver locating the `.so`, then `resolve` + run;
  and `type=` entry selection with the unknown-type rejection.
- `tests/py_eval.rs` — the subprocess-CPython evaluation path end to end.
</content>
</invoke>
