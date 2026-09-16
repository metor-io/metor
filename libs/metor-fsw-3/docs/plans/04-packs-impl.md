# Packs: implementation plan

Implements `04-packs.md`. Ten tasks, in dependency order. Each task ends
with `cargo test -p metor-fsw-3` green, `cargo clippy --all-targets`
clean, and a commit. T1 to T3 change the in-process crate and land
before any FFI exists, so the ABI is built on a surface that already
tests green. T4 and T5 are the boundary. T6 to T8 are Python and the
CLI. T9 is the gate.

Layout after this slice:

```
libs/metor-fsw-3/
  src/
    pack/
      mod.rs         export_pack!, Status, the five export bodies
      def.rs         PackDef, PackSystemDef, descriptor from a table
      raw.rs         RawSlice, RawRing, RawPort, and their unsafe views
    dl.rs            Pack, PackFns, DlStep, PackError, register_pack
    cli/
      mod.rs         clap surface
      build.rs       cargo build wrapper, artifact location, copy_atomic
      module.rs      typed module renderer
      pack_dev.rs    pack dev
      run.rs         target eval, config load, run
      config.rs      TargetConfig { config_version, packs, coordinator }
    bin/metor.rs
  python/
    metor-config/    metor_config/{__init__,_target,_model,_config}.py, tests/
    metor-build/     metor_build/{__init__,_wheel}.py
    metor-fsw-abi/
  tests/
    fixtures/echo-pack/   a two-system cdylib the dl tests load
    dl.rs                 builds the fixture, opens it, runs it
    golden/target.json    what to_config() emits for the example
examples/adcs-fsw3/
```

New dependencies, all already in the workspace lockfile: `libloading`
0.8, `schemars` 1, `clap` 4 with `derive`, `toml` 0.8, and `serde_json`'s
`raw_value` feature. `clap` and `toml` ride along into every pack; that
is a few hundred kilobytes and the core/host split in slice 6 removes
it.

## T1. Descriptor

Files: `src/system.rs`, `src/port.rs`, `src/fn_system/{mod,ctor}.rs`,
`src/coordinator/table.rs`, `src/pack/def.rs`, `macros/src/system_attr.rs`,
`src/lib.rs`, `Cargo.toml`.

1. `system.rs`: `PortDef` gains `record: &'static str`; `SystemDef` and
   `PortDef` derive `Serialize` and `Deserialize`. `Input::def` and
   `Output::def` fill `record` from `T::NAME`.
2. `fn_system/mod.rs`: `SystemFn` gains `const DOC: &'static str`.
   `system_attr.rs` collects the `///` lines on `execute`, joins them
   with newlines, and emits them; no doc is the empty string.
3. `ctor.rs`: `Ctor` gains `fn schema() -> Option<Box<RawValue>>`. The
   `Fn() -> S` impl returns `None`; the `Fn(P) -> S` impl bounds
   `P: JsonSchema` and returns `schemars::schema_for!(P)` serialized to a
   `RawValue`.
4. `table.rs`: `SystemTable` becomes `Vec<(String, TableEntry)>` plus a
   name index, so iteration is registration order. `TableEntry` gains
   `doc: &'static str` and `schema: Option<Box<RawValue>>`; `register`
   fills both, `register_system` leaves them empty. Add
   `pub(crate) fn entries(&self) -> impl Iterator<Item = (&str, &TableEntry)>`.
5. `pack/def.rs`: `PackDef<'a>` and `PackSystemDef<'a>` from the design
   doc, and `PackDef::from_table(&SystemTable) -> Vec<u8>`, which builds
   the descriptor and serializes it with `serde_json::to_vec`.
6. `lib.rs`: re-export `schemars` and `JsonSchema` so a pack derives it
   through this crate, as it does `serde`.
7. `src/tests/utils.rs`: give `NavFilter`'s params struct a `JsonSchema`
   derive and one `///` line, for the tests below.

Tests: `PortDef.record` equals the record's `NAME`; a `#[system]` block
with a doc comment yields `DOC` with the lines joined; `Ctor::schema` is
`None` for a unit ctor and holds the field's `description` for a params
ctor; `from_table` on the test table lists systems in registration
order; `PackDef` decodes from a leaked copy of those bytes with `ty`
and every port name borrowed (assert the pointer lies inside the
leaked slice); a doc string containing `\n` decodes as `Cow::Owned`.

## T2. Panics

Files: `src/coordinator/{run,mod,build}.rs`, `src/system.rs`,
`src/fn_system/mod.rs`, `src/tests/utils.rs`.

1. `system.rs`: `System::fault(&self, now, outputs: &mut Self::Outputs,
   message: &str)` with an empty default body.
2. `run.rs`: `Step::fault(&mut self, now, message: &str)` with an empty
   default; `Runner<S>::fault` forwards to `System::fault`.
3. `fn_system/mod.rs`: `FnSystem::fault` calls `outputs.log.begin(now)`
   then `outputs.log.fault("panic", message)`.
4. `run.rs`: a free fn `catch_step(step: &mut dyn Step, now) -> Result<(),
   String>` wrapping `execute` in `catch_unwind(AssertUnwindSafe(..))`,
   extracting the payload as `&str` or `String` and falling back to
   `"panic"`. `Entry` gains `latched: bool`. `Coordinator::step` skips a
   latched entry, and on `Err(message)` calls `step.fault`, sets the
   latch, and emits `tracing::error!(system = name, "system panicked:
   {message}")`. The status write happens either way.
5. `mod.rs`: `Coordinator::latched(&self) -> impl Iterator<Item = &str>`,
   for the CLI exit code.
6. `utils.rs`: a `boom` fn system whose `execute` panics on its second
   cycle, registered in the test table.

`catch_step` is shared with T4 so both sides catch identically.

Tests: a pipeline with `boom` runs the systems after it in the same
cycle; `boom.log` holds exactly one `Error` line with `kind = panic` and
the message; `boom.status` keeps arriving with `exec_time_ns == 0` after
the latch; `latched()` names `boom`; a trait-path system's panic
latches with nothing written, since it has no log port.

## T3. Table refactor

Files: `src/coordinator/{table,build}.rs`.

1. `table.rs`: `SystemMakeFn` takes `Vec<Vec<&RingBuffer>>` and
   `Vec<&RingBuffer>`. `register_system`'s closure calls
   `ring.view(NoWake)` per input ring and `ring.writer(NoWake)` per
   output ring before `bind`; the two `expect`s move here from
   `bind_rings` with their `PANIC Safety` lines.
2. `build.rs`: `bind_rings` collects ring references by index and calls
   `make`. The status writer stays where it is.

Tests: the existing build and run suites pass unchanged; a new test
asserts a ring with two edges has both slots claimed after build and
none free, via a third `view` failing with `FullReaderTable`.

## T4. Pack side

Files: `src/pack/{mod,raw}.rs`, `src/lib.rs`, `src/coordinator/params.rs`.

1. `raw.rs`: `RawSlice`, `RawRing`, `RawPort` as `repr(C)`. Unsafe
   helpers `RawSlice::as_bytes(&self) -> &[u8]`, `RawSlice::of(&[u8])`,
   and `RawPort::rings(&self) -> &[RawRing]`, each with a `# Safety`
   section naming the caller's contract.
2. `params.rs`: `ParamError` derives `Serialize` and `Deserialize`.
3. `pack/mod.rs`:
   - `ABI_VERSION: u32 = 1` and `Status { Ok = 0, Panicked = 1 }` with
     `from_raw(u32) -> Status`, unknown words mapping to `Panicked`.
   - `struct Exported { table: SystemTable, def: Vec<u8> }` behind a
     `OnceLock`, filled by `fn exported(build: fn() -> SystemTable) ->
     &'static Exported`, which also installs the log layer as the global
     default and ignores the error a second install returns.
   - `pub fn def(build) -> RawSlice`.
   - `pub unsafe fn create(build, ty, params, inputs, outputs, error) ->
     *mut c_void`: look the type up, `attach_raw` every ring, hand the
     handles to `make`, box the step. On `ParamError`, serialize it into
     a `thread_local! RefCell<Vec<u8>>`, point `error` at it, return
     null. An unknown type or a failed attach is a panic inside the
     `catch_unwind`, which returns null with an empty `error`.
   - `pub unsafe fn execute(instance, now) -> u32`: `catch_step` from T2
     on the boxed step; on `Err` call `fault` and return `Panicked`.
   - `pub unsafe fn destroy(instance)`: drop the box under `catch_unwind`.
   - `export_pack!($build:path)`: the five `#[unsafe(no_mangle)] pub
     unsafe extern "C" fn metor_fsw_*` one-liners calling the above.
4. `lib.rs`: `pub mod pack`, re-export `export_pack`.

The unit tests call `pack::create` and friends directly on a test
table, with no dylib involved; T5 covers the real boundary.

Tests: `def` bytes decode as the `PackDef` for the test table;
`create` with an unknown type returns null with an empty error; `create`
with a bad key returns null and the error decodes as the same
`UnknownKey` the in-process table raises; `create` on a two-system
pipeline followed by `execute` on both moves a record across, read back
through a host-side view; `execute` on `boom` returns `Panicked` on its
second cycle and the fault line is on its log ring; `destroy` frees the
reader slot, checked by a `view` that succeeds afterwards; `Status::
from_raw(7)` is `Panicked`.

## T5. Host

Files: `src/dl.rs`, `src/lib.rs`, `tests/fixtures/echo-pack/`,
`tests/dl.rs`, `src/cli/build.rs`, workspace `Cargo.toml`.

1. `dl.rs`: `Pack`, `PackFns`, `PackError`, and `Pack::open` as in the
   design doc. Resolve `metor_fsw_abi_version` first and call it; then
   the other four; then `metor_fsw_pack_def`, copy into a `Vec<u8>`,
   `Box::leak`, `serde_json::from_slice`. `Pack::systems()` iterates the
   decoded defs.
2. `dl.rs`: `DlStep { instance: *mut c_void, fns: PackFns, lib:
   Rc<Library>, latched: bool }` implementing `Step`: `execute` calls
   `fns.execute` and sets the latch on `Panicked`; `fault` is a no-op
   because the pack already wrote the line; `Drop` calls `fns.destroy`.
   `DlStep` is `!Send`, which the coordinator already is.
3. `SystemTable::register_pack(id, &Pack)`: one `register_raw` per
   system with `def` cloned from the descriptor and a `make` closure
   that builds `Vec<RawRing>` per input port, the `Vec<RawPort>`, the
   `Vec<RawRing>` of outputs, the params as `serde_json::to_vec`, calls
   `fns.create`, and on null decodes `error` as `ParamError` or, if
   empty, returns `ParamError::Decode("pack create failed")`.
4. `cli/build.rs`: `cargo_build(package, manifest_dir, release) ->
   Result<PathBuf, BuildError>` spawning `cargo build -p <package>
   --message-format=json`, inheriting stderr, and returning the cdylib
   path from the `compiler-artifact` messages. Landed here because
   `tests/dl.rs` needs it before T7 does.
5. `tests/fixtures/echo-pack`: a workspace member, `crate-type =
   ["cdylib"]`, `publish = false`, with `echo` (an `Input<Fixed>` to an
   `Output<Fixed>` copy), `boom` (panics on its second cycle), and
   `gain` (a params struct with one `f64` and a `///` line). Its
   `Cargo.toml` depends on `metor-fsw-3` by path.
6. `tests/dl.rs`: `cargo_build("echo-pack")` once per process behind a
   `OnceLock`, then the tests below. Run with `--test-threads` unbounded;
   the build is shared.

Tests: `Pack::open` on the fixture reports three systems with the
expected ports and `gain`'s schema; opening `libm` or another non-pack
library is `MissingSymbol("metor_fsw_abi_version")`; a coordinator with
`echo-pack.echo` between an in-process source and sink moves a record
within one cycle; a bad `gain` key is `BuildError::Params` naming the
system; `boom` latches after its second cycle and the coordinator keeps
running the sink; drop order is checked by a `Weak<Library>` that is
dead after the coordinator drops while a ring handle cloned before is
still alive. The version mismatch test patches the fixture: a second
fixture is too heavy, so `Pack::open` takes an `expected: u32` in a
`pub(crate) fn open_with` and the test passes `ABI_VERSION + 1`.

## T6. metor-config

Files: `python/metor-config/{pyproject.toml,metor_config/*.py,tests/*.py}`,
`tests/golden/target.json`, `src/cli/config.rs`.

1. `_model.py`: `Record` (a marker base class), `OutPort[T]`, `Loop[T]`,
   `Source = OutPort[T] | Loop[T]`, `PortRef(system, port)`, `System`
   base holding `_ty`, `_params: dict`, `_inputs: dict[str, list[Source]]`,
   `_pack`, and `Pack(id, lib, libs, abi_version)`. Generated modules
   subclass `System` and call `super().__init__(inputs, params)` from
   their `__init__`.
2. `_target.py`: `Target(cycle_rate, sim_dt=None, ring_depth=8,
   namespace=None)`, `loop(record) -> Loop`, `add(name, system) ->
   SystemHandle`, whose `__getattr__` returns `OutPort` refs for names in
   the system's declared outputs and raises `AttributeError` otherwise.
   `Loop.connect(port)` records the producer; a second call raises.
   `to_config()` walks systems in `add` order, resolves every `Source`
   to `{system, port}`, and raises `ConfigError` for a loop with no
   producer. `emit(path=None)` and the `atexit` hook.
3. `_config.py`: `CONFIG_VERSION = 1` and the JSON shape; the clock is
   `{"Wall": {"rate": r}}` or `{"Simulated": {"dt": {"secs", "nanos"}}}`.
   The abi check: `Pack.abi_version` against `$METOR_FSW_ABI_VERSION`
   when set, raising with both numbers.
4. `cli/config.rs`: `TargetConfig { config_version: u32, packs:
   Vec<PackRef>, coordinator: CoordinatorConfig }` and `PackRef { id,
   lib, libs: PathBuf }`; `TargetConfig::from_slice` reads
   `config_version` off a `serde_json::Value` first and returns
   `ConfigError::Version { found, expected }` before deserializing.
5. `tests/golden/target.json`: written by hand to the design doc's
   example; both sides pin to it.

Tests, Python (`python3 -m unittest discover -s python/metor-config/tests`):
the design doc's target file, with hand-written stand-in classes,
emits the golden byte for byte; a loop never connected raises
`ConfigError`; connecting a loop twice raises; a sequence input becomes
a `from` list in order; `add` twice under one name raises; the abi env
mismatch raises with both numbers. Rust: the golden deserializes as
`TargetConfig` and its `coordinator` builds against a table with stub
registrations for the four types; a `config_version` of 0 is
`ConfigError::Version`.

## T7. pack dev and metor-build

Files: `src/cli/{mod,module,pack_dev,build}.rs`, `src/bin/metor.rs`,
`python/metor-build/`, `python/metor-fsw-abi/`, `Cargo.toml`.

1. `cli/mod.rs`: `clap` derive with `Pack { Dev { root } }` and `Run`
   (T8). `bin/metor.rs` is `fn main() { metor_fsw_3::cli::main() }`.
2. `cli/build.rs`: `PackConfig::read(root)` parses `pyproject.toml`
   with `toml` for `[project] name` and `[tool.metor.pack]` with the
   four defaults from the design doc; `copy_atomic(src, dst)`; `triple()`
   from `std::env::consts` as `<arch>-<vendor>-<os>`, matching what cargo
   prints; `cdylib_name(stem)` per OS.
3. `cli/module.rs`: `render(pack: &PackRef, def: &PackDef) -> String`.
   Collect record names across all ports, sorted; one class each. Per
   system: class name as PascalCase of `ty`, the doc as docstring,
   `__init__` keywords for each input port then each schema property,
   the schema parsed here with `serde_json::from_str::<Value>` and
   walked for `type`, `default`, `description`, `$ref` into `$defs`
   rendered as `@dataclass`es above the class; class attributes for each
   output plus `log` and `status`. An input name colliding with a
   property is `PackDevError::NameClash`. The output is sorted and
   contains no paths, so the same descriptor renders the same bytes.
4. `cli/pack_dev.rs`: read the config, `cargo_build`, `Pack::open` the
   result, render, lay out `<root>/.metor/<module>/{__init__.py,
   py.typed, _libs/<triple>/<cdylib>}` through `copy_atomic`.
5. `python/metor-build`: `build_editable` and `build_wheel` hooks;
   `_run_metor(args)` with the three-step binary lookup; `_wheel.py`
   writing a wheel with `METADATA`, `WHEEL`, `RECORD`, and the `.pth`.
   `Requires-Dist: metor-fsw-abi==<n>` and `metor-config` are added to
   `METADATA`; `n` comes from `metor abi-version`, a third one-line
   subcommand.
6. `python/metor-fsw-abi`: `pyproject.toml` with version `1` and an
   empty module; a Rust test reads that file and asserts it equals
   `ABI_VERSION`.

Tests: `PackConfig::read` on a minimal `pyproject.toml` applies every
default and on a full one takes every override; `render` on the fixture
pack's descriptor equals `tests/golden/echo_pack.py`; the golden passes
`pyright` when the binary is on `PATH`, else the test is skipped with a
message; a colliding name is `NameClash`; `pack dev` on the fixture
lays out the four files, and a second run leaves the dylib at a
different inode; `copy_atomic` onto an existing file leaves no temp
file behind.

## T8. metor run

Files: `src/cli/run.rs`, `src/cli/mod.rs`.

1. `Run { target: Option<PathBuf>, cycles: Option<u64>, wall:
   Option<f64>, sim_dt: Option<f64> }`, `wall` and `sim_dt` exclusive.
2. `dev_packs(target_dir) -> Vec<PathBuf>`: `[tool.uv.sources]` path
   entries whose directory holds a `Cargo.toml` and a `[tool.metor.pack]`
   table. Run `pack_dev` on each.
3. `eval_target(path, dev_packs) -> Result<TargetConfig, RunError>`:
   pick the interpreter, build `PYTHONPATH`, set `METOR_CONFIG_OUT` to a
   temp file and `METOR_FSW_ABI_VERSION`, spawn with inherited stderr, a
   nonzero exit is `RunError::Python(status)`, then
   `TargetConfig::from_slice`.
4. `load(config) -> Result<Coordinator, RunError>`: apply clock
   overrides, `Pack::open(<libs>/<triple>/<cdylib>)` per pack, a table
   with `register_pack` per pack, `build`.
5. `run`: install `fmt` plus `log::layer()`, `stellarator::run` the
   coordinator with a stop future that is SIGINT or `--cycles`, then
   exit nonzero if `latched()` is non-empty, naming them.

Tests: `dev_packs` on the example's `pyproject.toml` finds one root;
`eval_target` on a file that raises returns `Python` with the status
and the traceback reaches the test's stderr; `--wall` and `--sim-dt`
together is a clap error; `run --cycles 3` on the fixture's own
`target.py` under `tests/fixtures/echo-pack/` exits zero.

## T9. ADCS gate

Files: `examples/adcs-fsw3/{pyproject.toml,target.py,contracts/,systems/adcs/,tests/convergence.rs,Cargo.toml}`,
workspace `Cargo.toml`.

1. `contracts`: the frames and params from `examples/adcs-fsw2/contracts`
   under `#[derive(Frame)]` and `#[frame(..)]`, params under `Deserialize
   + JsonSchema`, keeping the `#[serde(default = ..)]` lines and the
   `///` field docs. Drop the fsw-2 schema and docs derives.
2. `systems/adcs`: `crate-type = ["cdylib", "rlib"]`, `export` feature on
   by default gating `export_pack!(pack)`. `plant`, `nav`, `ctrl` as
   `#[system] impl` blocks over the fsw-2 state structs, `write` in place
   of `publish`, `tracing` calls kept. `mode`: a unit struct writing a
   nadir `ModeCmd` each cycle. `pub fn pack() -> SystemTable`.
   `pyproject.toml` with `[tool.metor.pack] id = "adcs"`, `[tool.uv]
   cache-keys` over `src` and `../../contracts`, and `metor-build` as a
   path source.
3. `pyproject.toml` and `target.py` at the example root, as in the
   design doc, with the fsw-2 target's params.
4. `tests/convergence.rs`: `eval_target` on `target.py`; build once with
   `adcs::pack()` registered under the `adcs.` prefix and once through
   `Pack::open` on `cargo_build("adcs")`; run both under the simulated
   clock for a fixed cycle count; tap `ctrl.motor_cmd` and `nav.log`
   through a fifth in-process recorder system appended to the config.
   Assert the pointing error at the end is below a bound taken from a
   first run and pinned with margin, both numbers in the commit message;
   assert the first-fix line; assert the two `motor_cmd` streams are
   identical.

Done when `cargo test -p adcs-fsw3` passes and, from the example
directory, `uv sync && metor run` prints log lines at a steady rate.

## T10. Docs

Files: `docs/plans/03-authoring.md`, `DESIGN.md`, `TODO.md`, `src/lib.rs`,
module docs.

1. `03-authoring.md`: fold in `Param`'s four-method split, `Cycle`, and
   `latest` over any record, in the doc's own words.
2. `DESIGN.md`: replace the ABI section with the five exports and the
   descriptor, and the build system section's `pyproject.toml` with the
   fsw-3 one.
3. Crate docs: a `pack()` and `export_pack!` example; module docs for
   `pack`, `dl`, and `cli` saying what shipped.
4. `TODO.md`: mark slice 3 done, note anything deferred.

## Review

After T10, an antagonistic review by a second agent against
`04-packs.md` and the style guide, focused on: every `unsafe` block's
`# Safety` matching what the caller can guarantee; no allocation on the
`execute` path across the boundary; every export catching unwinds; the
drop order between `DlStep`, `Library`, and rings; that reader slots are
claimed exactly once per edge across both sides; that the rendered
module is byte-deterministic; and that nothing in `python/` or `cli/`
reaches beyond what the design doc names.
