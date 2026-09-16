# Packs

The third slice of metor-fsw-3: systems from outside the binary. A pack is
a `cdylib` with five exported functions; the host `dlopen`s it, reads a
descriptor, and registers every system in it into the same `SystemTable`
a built-in registers into. Around the ABI sit the pieces that make a pack
usable from a target file: `metor-build`, the typed Python module, the
recorder's `to_config()`, and `metor run`.

In scope:

- The ABI: five `extern "C"` exports and JSON throughout: the descriptor
  built from `SystemDef`, the params, the params schema, and the one
  error.
- The host adapter: `Pack::open`, `SystemTable::register_pack`, and the
  table refactor that hands rings instead of bound ports.
- Panic containment at the boundary and in the coordinator, so a static
  and a dylib registration behave alike.
- `libs/metor-fsw-3/python/`: `metor-build`, `metor-config` with the
  fsw-3 `Target` and `to_config()`, and `metor-fsw-abi`.
- The `metor` binary: `pack dev` and `run`.
- `examples/adcs-fsw3`, the convergence gate deferred from slice 2, and
  the docs pass that folds shipped deviations into `03-authoring.md`.

Out of scope: process and wasm adapters, slots and sequences, shared
state, links, deployments, the `pack build` wheel, cross-compilation,
and the panel. Each consumes this ABI without changing it.

## Why this shape

Three rules carry over from fsw-2: rings cross as raw regions the pack
attaches to with its own copy of the frozen ring crate, each side frees
only what it allocated, and no panic crosses `extern "C"`. Everything
else is written fresh against the fsw-3 types.

Slice 2 put the codec on the record, params in a serde value tree, and
the log on an ordinary port. The descriptor is therefore the `SystemDef`
the coordinator already validates, params cross as the JSON Python
already emits, and the descriptor, schema, and error share that one
format. A record's message codec stays the record's business.

## Layout

```
libs/metor-fsw-3/
  src/
    pack/            export side: descriptor, the exported fns, export_pack!
    dl.rs            host side: Pack::open, register_pack
    bin/metor.rs     CLI: pack dev, run
    cli/             target eval, config file, dev-pack refresh
  python/
    metor-config/    Target, System, ports, to_config()
    metor-build/     PEP 517 / 660 backend shelling to `metor pack`
    metor-fsw-abi/   ABI marker distribution
examples/adcs-fsw3/
  contracts/         frames and params shared by systems and tests
  systems/adcs/      the pack: cdylib + rlib, pyproject with [tool.metor.pack]
  target.py, pyproject.toml, tests/
```

One crate still. A pack links `metor-fsw-3` whole. The core/host split
waits for wasm in slice 6.

## ABI

```rust
pub const ABI_VERSION: u32 = 1;

#[repr(C)]
pub struct RawSlice { pub ptr: *const u8, pub len: usize }
#[repr(C)]
pub struct RawRing  { pub base: *mut u8, pub len: usize }
/// One input port: its producers' rings, in edge order.
#[repr(C)]
pub struct RawPort  { pub rings: *const RawRing, pub len: usize }

extern "C" fn metor_fsw_abi_version() -> u32;
/// JSON `PackDef`, owned by the pack for the life of the library.
extern "C" fn metor_fsw_pack_def() -> RawSlice;
unsafe extern "C" fn metor_fsw_create(
    ty: RawSlice, params: RawSlice,
    inputs: RawSlice, outputs: RawSlice,
    error: *mut RawSlice,
) -> *mut c_void;
unsafe extern "C" fn metor_fsw_execute(instance: *mut c_void, now: i64) -> u32;
unsafe extern "C" fn metor_fsw_destroy(instance: *mut c_void);
```

Five exports with fixed names. The host resolves and calls
`metor_fsw_abi_version` first; a mismatch is `PackError::AbiMismatch`.
The version guards everything else: the slice types, the four remaining
signatures, the status word, and the descriptor's JSON shape. A bare
`u32` return assumes no layout, which is why the version is its own
export. Plain exports are what `nm` and a debugger show, and they map
one to one onto wasm exports in slice 6.

Binding happens on the pack side, because only the pack knows
`Inputs::bind`, so creation and binding are one call that takes rings.
A `Ctor` is a value, so the pack keeps it in the `SystemTable` it builds
and `create` looks the type up by name. Three exports per pack serve
every system in it.

### Instances

The `*mut c_void` is one system instance: the `Box<dyn Step>` the entry's
`make` returned, holding that system's state and bound ports, boxed once
more so the fat pointer fits a thin one. `ty` is consumed by `create`;
after that, dispatch is by handle.

```
host                                   pack
CoordinatorConfig::build
  per config entry:
    make(params, rings) ------------>  create(ty, params, inputs, outputs)
                                         table.get(ty).make(..) -> Box<dyn Step>
    DlStep { ptr, lib }  <-----------    Box::into_raw(Box::new(step))
Coordinator::step, per cycle:
    DlStep::execute(now) ----------->  execute(ptr, now)
                                         (*ptr).execute(now) under catch_unwind
drop(Coordinator):
    drop(DlStep) ------------------->  destroy(ptr)
```

Two config entries of one type are two `create` calls and two handles,
as they are two `Runner`s in-process.

### Descriptor

```rust
#[derive(Serialize, Deserialize)]
pub struct PackDef<'a> {
    #[serde(borrow)]
    pub systems: Vec<PackSystemDef<'a>>,
}

#[derive(Serialize, Deserialize)]
pub struct PackSystemDef<'a> {
    /// The table key: `register("nav", ..)`.
    pub ty: &'a str,
    pub def: SystemDef,
    /// The doc comment on `execute`, or empty.
    #[serde(borrow)]
    pub doc: Cow<'a, str>,
    /// JSON Schema of the params struct; `None` for a `Fn() -> S` ctor.
    #[serde(borrow)]
    pub params: Option<Cow<'a, RawValue>>,
}
```

The descriptor is JSON, written by `serde_json` in the pack and read by
it on the host. `SystemDef` and `PortDef` gain `Serialize` and
`Deserialize`. Their names are `&'static str`, which `serde_json` borrows
from `&'static [u8]` when the string has no escapes, and identifiers have
none; the host copies the descriptor out of the library once and leaks
the copy, which is the one init-time allocation. The same `SystemDef`
type then serves the pack, the wire, and the table. The doc string may
hold newlines, so it is a `Cow`. The schema is a `RawValue` on both
sides: `schemars` output boxed in the pack, borrowed on the host, and
handed to the renderer by `pack dev` as is.

`PortDef` gains `record: &'static str`, the `Record::NAME`. The build
compares ids; Python needs the name to spell a type. `SystemFn` gains
`const DOC: &'static str`, read by `#[system]` from the doc comment on
`execute`. Params field docs ride the schema as `description`, which
`schemars` fills from `///` lines.

### Params schema

```rust
impl<S, P: DeserializeOwned + JsonSchema, F: Fn(P) -> S> Ctor<S, (P,)> for F { .. }

pub trait Ctor<S, Marker> {
    fn make(&self, params: Params<'_>) -> Result<S, ParamError>;
    fn schema() -> Option<Box<RawValue>>;
}
```

A params struct derives `Deserialize` and `JsonSchema`. `schemars` is
already in the workspace and JSON Schema is what the Python side reads.
`register_system`, the trait path, has no params type and registers
`None`; its Python class takes `**params: Any`.

### Pack side

```rust
pub fn pack() -> SystemTable {
    let mut table = SystemTable::new();
    table.register("plant", PlantState::new);
    table.register("nav", NavState::new);
    table.register("ctrl", CtrlState::new);
    table.register("mode", Mode::default);
    table
}
metor_fsw_3::export_pack!(pack);
```

A pack is a `SystemTable` built inside the library. `export_pack!` emits
the five `#[unsafe(no_mangle)] extern "C"` fns, each a one-line call into
`pack::`. The table is built once behind a `OnceLock` on first use:
`pack::def` calls the table fn, installs
`tracing_subscriber::registry().with(log::layer())` as this library's
global default, and serializes the descriptor from the table's entries
into a second `OnceLock`. The library has its own `tracing` dispatcher
and its own thread-local log queue, so `FnSystem::execute` clears and
drains exactly as it does in-process and every line lands on the
system's own `log` ring.

`TableEntry` keeps its order of registration and grows `doc` and
`params_schema`, so the descriptor is a projection of the table. The
exports call plain functions in `pack/`:

- `create` looks `ty` up in the table, attaches every `RawRing` with
  `RingBuffer::attach_raw`, takes one `view(NoWake)` per input ring and
  the `writer(NoWake)` of each output ring, and calls the entry's `make`
  with the params decoded from the JSON. The result is `Box<dyn Step>`
  boxed again as `*mut c_void`. On a `ParamError` it writes the error as
  JSON into a thread-local buffer, points `error` at it, and returns
  null; the buffer lives until the next `create` on that thread. Attach
  handles are dropped after binding; writers and views outlive the
  handle they came from.
- `execute` calls `Step::execute(now)` inside `catch_unwind`. A panic
  becomes `Status::Panicked` after `Step::fault` has run, see below.
- `destroy` drops the box, also inside `catch_unwind`.

Every export is `catch_unwind` at its top. The pack allocates and frees
the instance box, the descriptor bytes, and the error buffer; the host
allocates and frees the rings and its copy of the descriptor.

### Status word

```rust
#[repr(u32)]
pub enum Status { Ok = 0, Panicked = 1 }
```

The host maps the returned `u32` through `Status::from_raw`; an
out-of-range word is `Panicked`.

## Panics

```rust
pub trait Step {
    fn execute(&mut self, now: Timestamp);
    /// Called once after `execute` panicked, with the payload's message.
    fn fault(&mut self, now: Timestamp, message: &str) {}
}

pub trait System {
    ..
    fn fault(&self, now: Timestamp, outputs: &mut Self::Outputs, message: &str) {}
}
```

`Runner<S>::fault` forwards to `System::fault`. `FnSystem` overrides it
to write `log.fault("panic", message)` on the system's `log` output, so a
panic reaches the ground the way every other fault does; the trait path
keeps the default and declares its own port if it wants one. After a
panic the caller latches the instance: it is never executed again,
`destroy` still runs at teardown, and the coordinator's status record
keeps writing a zero exec time for it. Only the log output is touched
after the panic, which is safe because user code cannot hold it across
the panic boundary.

The coordinator catches, faults, and latches the same way: `step` wraps
each entry's `execute` in the same `catch_unwind` helper the pack's
export uses, and `Entry` carries the latch. A static and a dylib registration of one system then behave
alike under a panic, which the parity test asserts, and a panic in one
system is not a loss of the vehicle. The panic message also goes to the
host's `tracing` at error level once, so a console run shows it.

## Host

```rust
pub struct Pack {
    lib: Rc<libloading::Library>,
    fns: PackFns,
    def: PackDef<'static>,
}

/// The exports after the version check, as bare fn pointers valid
/// while `lib` is loaded.
#[derive(Clone, Copy)]
struct PackFns { create: CreateFn, execute: ExecuteFn, destroy: DestroyFn }

impl Pack {
    /// # Safety
    /// `path` is a metor-fsw-3 pack built against this ABI; the library
    /// runs arbitrary code on load.
    pub unsafe fn open(path: &Path) -> Result<Pack, PackError>;
    pub fn systems(&self) -> impl Iterator<Item = &PackSystemDef<'static>>;
}

impl SystemTable {
    /// Registers every system in `pack` under `"{id}.{ty}"`.
    pub fn register_pack(&mut self, id: &str, pack: &Pack);
}

pub enum PackError {
    Open(libloading::Error),
    MissingSymbol(&'static str),
    AbiMismatch { found: u32, expected: u32 },
    Decode,
}
```

`open` loads the library, resolves and calls `metor_fsw_abi_version`,
resolves the other four exports into `PackFns`, calls `metor_fsw_pack_def`,
copies and leaks the descriptor, and decodes it. `register_pack` inserts
one entry per system whose `make` closure holds the `Rc<Library>` and a
copy of `PackFns`, builds the `RawPort` and `RawRing` arrays from the
rings it is handed, calls `create`, and wraps the returned pointer in a
`DlStep` that calls `execute`, latches on `Panicked`, and calls `destroy`
on drop. A `create` error decodes as the `ParamError` it is and surfaces
as `BuildError::Params { id, source }`, the same error a built-in raises.
Built-ins keep bare type names; the pack id prefix keeps two packs
apart.

### Table refactor

Today `TableEntry::make` receives `Vec<Vec<View>>` and `Vec<Writer>`
created by `bind_rings`. A view claimed on the host would consume the
reader slot the pack is about to claim, so `make` receives rings:

```rust
type SystemMakeFn = dyn Fn(Params<'_>, Vec<Vec<&RingBuffer>>, Vec<&RingBuffer>)
    -> Result<Box<dyn Step>, ParamError>;
```

In-process entries call `view(NoWake)` and `writer(NoWake)` themselves;
dylib entries call `region()`. Reader accounting stays one view per
edge, claimed on whichever side binds. `bind_rings` shrinks to the
lookup.

### Teardown

`Coordinator` drops `entries` before `rings`. Each `DlStep` destroys its
instance, releasing its reader slots and writer, and drops its `Rc`; the
library unloads when the last one goes, still before any ring it
attached to is freed. `Pack` itself may be dropped right after
`register_pack`.

## Python

`libs/metor-fsw-3/python/metor-config` is a new package: `Target`,
`System`, the port markers, `loop`, and `to_config()`. Stdlib only,
CPython 3.11+.

```python
from adcs_pack import Ctrl, Mode, MotorCmd, Nav, Plant
from metor_config import Target

fsw = Target(cycle_rate=100.0, namespace="cube_sat")
motor_cmd = fsw.loop(MotorCmd)
plant = fsw.add("plant", Plant(altitude=400e3, motor_cmd=motor_cmd))
nav = fsw.add("nav", Nav(imu=plant.imu, gain=0.2))
mode = fsw.add("mode", Mode())
ctrl = fsw.add("ctrl", Ctrl(est=nav.est, mode=mode.cmd))
motor_cmd.connect(ctrl.motor_cmd)
```

Inputs are constructor keywords beside params, as DESIGN.md spells it.
An input keyword takes one source or a sequence of sources; a sequence is
fan-in in the order given. A source is a producer's `OutPort[T]` or a
`Loop[T]`. `loop(T)` is typed at creation so pyright can check the
`Plant` call before `connect` names the producer. A handle's attribute is
its port.

`add` order is step order, and a system can only name ports of systems
that already exist, so `loop` is the one way to read a later system and
a one-cycle delay is always spelled. A loop never passed to a system, or
connected twice, is an error at `to_config()`.

`Target(cycle_rate, sim_dt=None, ring_depth=8, namespace=None)`.
`sim_dt` selects the simulated clock. `namespace` is carried in the
config for later slices.

### Typed module

`metor pack dev` renders `<module>/__init__.py` from the descriptor,
deterministic and free of absolute paths:

```python
# @generated by metor pack dev
PACK = Pack(id="adcs", lib="adcs_systems", libs=Path(__file__).parent / "_libs")

class Imu(Record): ...
class MotorCmd(Record): ...

class Nav(System):
    """<doc from execute>"""
    def __init__(self, *, imu: Source[Imu] | Sequence[Source[Imu]] = (),
                 gain: float = 0.1) -> None: ...
    est: OutPort[Est]
    log: OutPort[LogEvent]
    status: OutPort[SystemStatus]
```

One `Record` marker class per distinct `record` name across the pack's
ports, so an edge between different records is a pyright error. One
`System` subclass per entry, named as the PascalCase of `ty`. Params
render from the JSON Schema: `number`, `integer`, `boolean`, `string`,
arrays of those, and nested objects as nested `@dataclass`es, with
`default` values as keyword defaults and `description` as the docstring.
A param sharing a name with an input port is a `pack dev` error. `log`
and `status` are annotated as outputs like any other.

The pack module checks `PACK.abi_version` against
`$METOR_FSW_ABI_VERSION`, set by `metor run`, so a stale editable install
fails in the target file with a plain message.

### Config file

```json
{
  "config_version": 1,
  "packs": [{ "id": "adcs", "lib": "adcs_systems", "libs": "/abs/.metor/adcs_pack/_libs" }],
  "coordinator": {
    "clock": { "Wall": { "rate": 100.0 } },
    "ring_depth": 8,
    "systems": [
      { "id": "plant", "ty": "adcs.plant", "params": { "altitude": 400000.0 },
        "inputs": [{ "port": "motor_cmd", "from": [{ "system": "ctrl", "port": "motor_cmd" }] }] },
      ..
    ]
  }
}
```

`coordinator` is `CoordinatorConfig` as serde emits it, including the
externally tagged clock and `Duration` as `{secs, nanos}`. `packs` lists
each pack once with the library stem and the `_libs` directory the
module self-locates; the host picks `<libs>/<triple>/lib<stem>.<ext>` for
its own triple. `config_version` is read off the raw JSON first. The
file is the coordinator's config plus the packs that supply its types.

`to_config()` returns this dict; `emit()` writes it to
`$METOR_CONFIG_OUT` or stdout, and an `atexit` hook emits the file's
single `Target` so a target file is a script that ends.

### metor-build

A backend of two hooks. `build_editable` runs `metor pack dev <root>`
and writes a wheel holding one `.pth` line for `<root>/.metor`;
`build_wheel` and `build_sdist` raise until the wheel slice. The binary
is `$METOR_BIN`, else `metor` on `PATH`, else `cargo run -q -p
metor-fsw-3 --bin metor`. The wheel writer is the few lines of zip and
`RECORD` a `.pth`-only wheel needs.

```toml
[tool.metor.pack]
id = "adcs"          # pack id, the prefix in `ty`; default: module
crate = "adcs"       # cargo package; default: [package] name
lib = "adcs_systems" # cdylib stem; default: [lib] name, else crate with - as _
module = "adcs_pack" # Python module; default: distribution name as an identifier
```

Pack authors write `[tool.uv] cache-keys` over their sources; uv 0.11 is
the floor for `../` globs.

`metor-fsw-abi` is a code-free distribution whose version is
`ABI_VERSION`. `pack dev` stamps `metor-fsw-abi==<ABI_VERSION>` into the
editable wheel's requirements, so a pack and a host of different ABIs
fail inside `uv lock` before anything runs. It starts at 1 with fsw-3.

## CLI

`metor pack dev <root>`: reads `pyproject.toml`, runs `cargo build -p
<crate> --message-format=json` with cargo's stderr inherited, opens the
built library with `Pack::open`, renders the module, and lays out
`<root>/.metor/<module>/{__init__.py, py.typed, _libs/<triple>/<cdylib>}`.
Every file lands through `copy_atomic`, a ten-line fn: a temp file
beside the destination then `rename`, because macOS kill-caches code
signatures by inode and a dylib overwritten in place kills the next
process to `dlopen` it. The descriptor is read by `dlopen` here and
again at run, and the build's port and param validation is the
staleness check.

`metor run [target.py] [--cycles N] [--wall RATE | --sim-dt SECS]`:

1. Default the path to `./target.py`.
2. Refresh dev packs: every `[tool.uv.sources]` path entry with a
   `Cargo.toml` and a `[tool.metor.pack]` table gets `pack dev`.
3. Pick an interpreter: `$METOR_PYTHON`, else `$VIRTUAL_ENV/bin/python`,
   else `python3`. Put `$METOR_CONFIG_PY` on `PYTHONPATH` when set, plus
   each dev pack's `.metor`. Set `METOR_CONFIG_OUT` and
   `METOR_FSW_ABI_VERSION`. Run the file with stderr inherited; a Python
   traceback is the error surface.
4. Read the config, check `config_version`, apply clock overrides.
5. `Pack::open` each pack, `register_pack` into a table that already
   holds the built-ins, `CoordinatorConfig::build`.
6. `run` on stellarator until SIGINT or `--cycles`. Exit nonzero if any
   system latched.

The host installs a `fmt` layer for its own events; cargo prints its own
progress.

## Gate: the ADCS loop

`examples/adcs-fsw3/contracts` holds the frames and params structs from
`examples/adcs-fsw2/contracts`, with `JsonSchema` derived on the params.
`systems/adcs` is one crate, `crate-type = ["cdylib", "rlib"]`, with the
four fn systems `plant`, `nav`, `ctrl`, and `mode`, a `pub fn pack() ->
SystemTable`, and `export_pack!(pack)` behind an `export` feature the
rlib build turns off. `mode` publishes a fixed nadir `ModeCmd` until
slots exist. `target.py` is the file above; `plant` is added first and
reads `ctrl.motor_cmd` through a loop.

`tests/convergence.rs` evaluates `target.py` through the same code
`metor run` uses, builds under the simulated clock twice, once with
`pack()` registered statically and once through `Pack::open` on the
freshly built cdylib, and asserts for both that the pointing error falls
below a bound within a fixed cycle count, that `nav.log` carries the
first-fix line, and that the two runs' `ctrl.motor_cmd` streams are
identical.

Slice 2's T8 lands here: `03-authoring.md` gains the shipped deviations
(`Param` split into `append_defs`, `bind_in`, `bind_out`, and `get`;
`Cycle` as the extra parameter; `latest` over any record), and the
DESIGN.md ABI section is replaced with the shipped exports.

## Tests

- ABI: `SystemDef` round-trips through `serde_json` and borrows its
  names from the leaked bytes; a doc string with a newline decodes as an
  owned `Cow`; a descriptor with one system of every param kind decodes
  and its schema `RawValue` is byte-identical to `schemars` output; a
  `ParamError` round-trips through the error slice.
- Export: `export_pack!` on a test table exports the five names and
  `abi_version` returns the current version; `create` with an unknown
  `ty` returns null and an error; `create` with a bad key returns the
  same `ParamError` the in-process `register` raises; `execute` after a
  panicking system returns `Panicked` and the `log` ring holds one fault
  line; `destroy` releases the reader slots so the ring's table is free.
- Host: `Pack::open` on a library exporting the wrong version is
  `AbiMismatch` and resolves nothing else; a library missing
  `metor_fsw_destroy` is `MissingSymbol` naming it; `register_pack`
  prefixes `"{id}.{ty}"`; a dylib system in a pipeline sees same-cycle
  data; a dylib system latched by a panic keeps its status writes and is
  not executed again; a coordinator with a dylib entry drops the library
  before the rings, checked with a drop-order recorder.
- Runner: an in-process panic latches and writes one fault line, and the
  next system in the cycle still runs.
- Python: `to_config()` golden for the ADCS target; a loop never
  connected, a loop connected twice, and a fan-in sequence are the
  expected errors or edges; the rendered module for a two-system pack is
  golden and passes pyright with an edge between different records and a
  loop connected to a port of another record as the two errors.
- CLI: `pack dev` on the example lays out the four files and a second
  run replaces the dylib at a new inode; `run --cycles 3` on the example
  exits zero; a `config_version` of 0 exits with the version error.
- Gate: the convergence test above.

## Open decisions

None outstanding. Settled in review: five plain exports, `ABI_VERSION`
starting at 1, JSON for the descriptor and error, `loop(T)` typed at
creation, and panics caught in the coordinator's runner as well as at
the boundary.
