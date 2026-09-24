# Sequences and slots: implementation plan

Implements `12-sequences.md`. Ten tasks, in order. Each ends with
`cargo test -p metor-fsw-3`, `cargo clippy --all-targets` clean, and a
commit. Python tasks also end with `uv run pytest` in
`python/metor-config`. T1 and T2 change the ABI and `Step`, and every
later task builds on them. T3 to T6 are in-process only. T7 and T8 add
the dl half. T9 and T10 are the surface and the gate.

## T1. Adapter and ABI 9 plumbing

Files: `src/coordinator/{config,table,build,error}.rs`,
`src/thread/mod.rs`, `src/pack/{mod,def}.rs`, `src/dl.rs`, `src/def.rs`,
`src/deploy/index.rs`, `src/link/*.rs` (make sites), `tests/golden/*.json`.

1. `config.rs`: add `Adapter { Thread(String), Channel(ChannelConfig) }`,
   `ChannelConfig { name, start: RunState }`, and `RunState { Running,
   Stopped }`, all `snake_case`. `SystemConfig::thread` becomes
   `#[serde(flatten, default, skip_serializing_if = "Option::is_none")]
   adapter: Option<Adapter>`. Re-export from `coordinator` and `lib.rs`.
2. `MakeCx::thread: &str` becomes `adapter: Option<&Adapter>`. Add
   `MakeCx::thread(&self) -> &str`, which returns the named thread or
   `DEFAULT_THREAD`, so `thread::place` reads it as before.
3. Refusals, each a `ParamError` variant that `build_error` maps:
   - `table::entry`: `Some(Thread)` → `ParamError::Thread`
     (`ThreadOnCyclic`, as now). `Channel` passes; T4 wraps it.
   - `thread::place`: `Some(Channel)` → `ParamError::Channel` →
     `BuildError::ChannelOnAsync { id }`.
4. `pack/def.rs`: `Instance::thread` becomes `adapter:
   Option<Adapter>`. `DefCxOwned` gains `params: serde_json::Value`.
5. `def.rs`: `DefCx` gains `params: &'a serde_json::Value`, defaulting to
   `&Value::Null` in `empty`, `of_inputs`, and `of_outputs`.
   `instance_def` passes `&system.params`. So do `DefCxOwned::of` and
   `borrow`, and the deploy index's `compute`.
6. `pack/mod.rs`: `ABI_VERSION = 9`. `python/metor-fsw-abi` pins the
   same number.
7. Build checks channel names: at most `SEQUENCE_CHANNEL_NAME_CAP` bytes
   (`BuildError::ChannelName`), unique (`BuildError::DuplicateChannel`).

Tests:
- `config.rs`: `{"thread":"gps"}` and `{"channel":{"name":"m"}}`
  round-trip, and a config with neither key parses. A config with both
  keys is refused; if serde's flatten takes one silently, add a
  `deserialize_with` that errors, and pin it.
- `build.rs`: `ChannelOnAsync` on an async system, `ThreadOnCyclic`
  unchanged, an over-length channel name, a duplicate channel name.
- `pack/tests.rs`: `DefCxOwned` round-trips params.
- Golden JSON updates are mechanical. Read every diff.

## T2. `Step` ends and loads

Files: `src/coordinator/{run,mod}.rs`, `src/pack/mod.rs`, `src/dl.rs`,
`src/thread/mod.rs`, new `src/coordinator/step.rs`.

1. Move `Step` to `coordinator/step.rs` with `End`, `LoadError`, and the
   new methods:
   ```rust
   fn ended(&self) -> Option<&End> { None }
   fn load(&mut self, _name: &str) -> Result<(), LoadError> { Err(LoadError::NotLoadable) }
   ```
   `LoadError { NotLoadable, Unknown(String), Mismatch(String), Hash(String),
   Open(String), Create(ParamError) }`, with `thiserror` messages. These
   become the event text.
2. Delete `latched()`. `catch_step` returns a `bool` today; it
   becomes:
   ```rust
   pub(crate) enum Stepped { Ran, Ended, Unwound }
   ```
   An ended step is not executed, and is not dropped. Only an unwind
   drops the slot. `Entry::run` skips an ended step with zero elapsed
   time and logs "system panicked" only for `Unwound` or `End::Panicked`,
   once. `Coordinator::latched()` becomes `ended()`, yielding each id
   with its `End`.
3. `Thread` (async adapter): its latch becomes
   `Some(End::Panicked)`.
4. `pack::execute`: `Stepped::Unwound` → `Panicked`; otherwise the
   step's `ended()` as a word:
   ```rust
   pub enum Status { Ok = 0, Panicked = 1, Completed = 2, Aborted = 3, Failed = 4 }
   ```
   `Failed` loses its text at the boundary; the pack's step has already
   written it to the log. `from_raw` keeps unknown as `Panicked`.
5. `DlStep` keeps `end: Option<End>` from the last word:
   `Failed(Cow::Borrowed("see log"))` for 4. `ended()` returns it, and
   `execute` returns early once it is set.

Tests:
- `run.rs`: a step that ends stays in its slot, is not executed again,
  and its status keeps arriving with zero time. The existing panic
  tests are unchanged apart from `latched` → `ended`.
- `pack/tests.rs`: each `End` maps to its word and back; an unknown word
  is `Panicked`.

## T3. Records, `Abort`, and `ChannelStatus`

Files: new `src/channel/{mod,records}.rs`, `src/fn_system/param.rs`,
`src/lib.rs`, new `../metor-component/src/str.rs`,
`../metor-component/src/lib.rs`.

0. `FrameStr<const N: usize>` in `metor-component`, as in the design:
   `repr(transparent)` over `[u8; N]` with the zerocopy derives,
   `CAPACITY`, `EMPTY`, `Default`, `new` refusing text that is too long
   or holds a NUL (`FrameStrError`), and `to_str`. Implement:
   - `AsVTable`: one `raw_field` of `PrimType::U8`, shape `[N]`, named
     by the path, following `nox::Tensor`'s impl;
   - `Metadatatize`: `once(path.to_metadata().with_string())`;
   - `AsComponentView` and `FromComponentView` as `[u8; N]` has them in
     `metor-proto/src/com_de.rs`.
   Re-export it from `metor-fsw-3`'s root.

1. `records.rs`: hand `Record` impls for `SequenceCommand`,
   `SequenceChannelEvent`, and `SequenceRegistry`, in the shape of
   `LogEvent`'s in `log.rs`: postcard, `NAME` the wkt type's name,
   `timestamp` `None`. Pick `MAX_LEN`: 256 for commands and events. For
   the registry, compute it from the encoded params at make and check it
   in T6, since it grows with the channels.
2. `AbortRequest {}`: a postcard message, `NAME = "abort_request"`.
3. `ChannelStatus`: a `Frame`, as in the design: `state` and `loaded`
   as `u32`, `artifact: FrameStr<64>`, and `hash: [u8; 32]`. Pin the
   frame's layout in a test. `ChannelState` is an enum with a `code()` and the
   seven states.
4. `Abort` param (`param.rs`): wraps `Input<AbortRequest>`. `append_defs`
   pushes an input named `abort`, whatever the parameter is called.
   `requested(&mut self) -> bool` drains and latches. The item is
   `&mut Abort`.

Tests:
- `FrameStr`:
  - `new` round-trips through `to_str`;
  - text of exactly `CAPACITY` bytes fits, and one byte more is refused;
  - an embedded NUL is refused;
  - `to_str` on invalid UTF-8 errors rather than panicking;
  - a frame holding one has a single `U8` leaf of shape `[N]`, and its
    metadata has `is_string`.
- Each record's `schema().packet_id()` equals the wkt `::ID` the panel
  pins (`SequenceCommand::ID == [225, 247]`, and so on).
- `Abort::requested` is false with no producer, and latches after one
  record.
- A `#[system]` fn with `abort: &mut Abort` declares input `abort`.

## T4. The channel adapter

Files: `src/channel/{mod,apply,adapter}.rs`,
`src/coordinator/{table,build,error}.rs`.

1. `table.rs`: `TableEntry::make` becomes `Arc<MakeFn>`. `step.rs`
   gains `Occupant { index: u32, artifact: FrameStr<64>, hash: [u8; 32]
   }` and `Step::occupant(&self) -> Option<&Occupant> { None }`.
2. `apply.rs`: the pure state machine. It sits in its own file so Kani
   and the tests see only it:
   ```rust
   pub struct State { pub run: RunState, pub ended: bool, pub abortable: bool }
   pub enum Transition { Start, Stop, Abort, Reset, Load }
   pub enum Refusal { Running, NotRunning, Ended, NotAbortable }
   pub fn apply(state: State, kind: &SequenceCommandKind) -> Result<Transition, Refusal>;
   ```
   Add a `#[cfg(kani)]` harness over every `State` and a symbolic kind:
   a running state yields only `Stop` or `Abort`, and `Reset`/`Load`
   only come from a state that is not running.
3. `adapter.rs`:
   - `channel::def(inner: SystemDef) -> SystemDef`: removes the `abort`
     input, appends `commands`, then `events` and `channel_status`.
   - `channel::make(entry: &TableEntry, cx: MakeCx) -> Result<Box<dyn Step>, ParamError>`:
     takes the outer rings, splits off `commands`, `events`, and
     `channel_status`, and allocates a private abort ring
     (`ring_capacity(AbortRequest::MAX_LEN, 4)`, one reader) when the
     inner def has an `abort` input. It rebuilds the inner input list
     with that ring in `abort`'s position. It clones every ring handle
     into `Recipe`, then calls `entry.make` with the inner def, the same
     adapter, and an empty `Threads` owned by the adapter. The initial
     `run` comes from `ChannelConfig::start`.
   - `Channel::execute`, as in the design: drain, apply, run the inner
     step through `catch_step`, report a new end once, and write
     `channel_status`. Events reuse one encode buffer.
   - `reset`: drop `inner`, call the recipe, then re-apply `loaded`. A
     make error ends the channel as `Failed(make error)`.
4. `build.rs`: when `system.adapter` is `Channel`, compute
   `channel::def(entry.def_for(cx))` in `instance_def`. `bind_rings`
   calls `channel::make` in place of `entry.make`. Reject inner ports
   named `commands`, `events`, or `channel_status`
   (`BuildError::ReservedPort`).

Tests, in `channel/tests.rs`, over a counting cyclic system and one
that panics:
- `apply` table: every kind from every `State`.
- `Stop` skips execution while commands still apply. `Start` resumes.
- `Reset` in a ring with `max_readers` equal to the edges: the re-made
  step claims its views, so the old ones were released.
- An inner panic ends the channel as `Panicked`, and the adapter lives.
  `Reset` then `Start` runs it again.
- `Abort` reaches the inner `Abort` param. `Abort` on a system without
  one is refused.
- A command for another channel produces no event.
- `channel_status` follows each transition.
- In `coordinator/tests.rs`: a config with a channel entry builds, its
  outer ports can be read by another system, and `ReservedPort` fires.

## T5. Sequences

Files: new `src/sequence/{mod,seq,step,tests}.rs`,
`src/fn_system/{mod,param}.rs`, `src/coordinator/table.rs`,
`macros/src/{lib,system_attr}.rs`, `tests/ui/**`.

1. `seq.rs`: `Seq { now: Cell<Timestamp>, aborted: Cell<bool> }`,
   plus `Sleep`, `WaitFor`, and `NextCycle`. Each stores `Option<armed
   value>` and arms it on its first poll. `SeqError` and its `Display`.
2. `Seq` as a `Param`: it declares no ports. Its item is `&Seq`, handed
   out from `Cycle`, which gains `seq: Option<&'a Seq>`. A `Seq`
   parameter in a `#[system]` fn is a compile error from the macro, not
   a runtime one.
3. `SequenceFn: Ports` with `fn call(state, items) -> impl Future<Output
   = Result<(), SeqError>>`. This is the `AsyncSystemFn` shape, with
   `NoWake` items and no `stop`.
4. `step.rs`: `SequenceStep` holds `Rc<Seq>` and the `Pin<Box<dyn
   Future>>` built from `async move` over the state, `InSet`, `OutSet`,
   and the handle, plus `abort: Option<Input<AbortRequest>>` and `end`.
   - The sequence adapter always declares `abort`: `SequenceDef` puts it
     first among the inputs, so a `#[sequence]` needs no `Abort`
     parameter.
   - `execute` follows the design, with the poll inside
     `log::with_log_port`. Poll with `Waker::noop()`.
5. `table.rs`: `register_sequence<S: SequenceFn, M, C: Ctor<S::State,
   M>>`. Its make refuses `Thread`, like a cyclic entry.
6. Macro: `#[sequence]` goes through `system_attr` with a `Kind::Sequence`
   flag:
   - it must be async, with no trailing `stop`;
   - it must return `Result<(), SeqError>`;
   - it rejects a `Timestamp` parameter with "read `seq.now()`";
   - it emits `impl SequenceFn`.
   `#[system]` rejects a `&Seq` parameter.

Tests:
- `seq.rs`, polled by hand:
  - `sleep` arms on its first poll, not at construction;
  - `wait_for` resolves the poll its predicate holds, and times out at
    its deadline;
  - abort beats a deadline that falls on the same poll;
  - `next_cycle` ignores abort.
- `step.rs`:
  - a sequence completes, fails with text, and aborts;
  - a panic unwinds to `catch_step`;
  - a stopped then resumed channel continues at its await;
  - log lines from inside a poll land on the sequence's `log`.
- `tests/ui`: `sequence_not_async`, `sequence_with_stop`,
  `sequence_bad_return`, `sequence_now_param`, `system_seq_param`, and
  `pass/sequence_arm.rs`.

## T6. `ChannelRegistry`

Files: new `src/channel/registry.rs`, `src/link/mod.rs`
(`register_builtins`).

1. `#[system] fn channel_registry(#[state] s: &mut Registry, now:
   Timestamp, registry: &mut Output<SequenceRegistry>)`.
2. `Registry::new(params)` decodes `{ channels: [SequenceChannelSpec] }`
   and encodes the message once into `bytes`. It refuses params that
   encode past `MAX_LEN`. Cycle 0 and then every `period` cycles write
   `bytes`. `period` comes from the params (Python sends the cycle rate),
   defaulting to 100.
3. Register as `fsw.channel_registry`.

Tests: it writes on cycle 0 and then every period, the bytes decode to
the params, and oversized params are refused at make.

## T7. `DlSlot`

Files: new `src/dl/{slot,store}.rs` (`dl.rs` moves to `dl/mod.rs`),
`src/link/mod.rs`, `Cargo.toml` (blake3, already in the workspace).

1. Params:
   ```rust
   struct SlotParams { allow: Vec<Candidate>, initial: Option<String>, store: PathBuf }
   struct Candidate { name: String, ty: String, params: Value,
                      path: Option<PathBuf>, hash: Option<String>,
                      def: Option<SystemDef> }
   ```
   `path`, `hash` (hex), and `def` are `None` until `metor build` fills
   them. Make refuses a candidate missing any of them.
2. Def: `def_fn` reads `cx.params`, takes `allow[0].def`, and fails with
   `DefError::Unresolved` when it is missing. The table's static `def`
   is empty.
3. Make: refuses no adapter or `Thread` (`ParamError::SlotChannel`).
   Keeps the params, the ring handles, and the instance's `DefCx` parts,
   and loads `initial` if set.
4. `load(name)`:
   - find the candidate, or `Unknown`;
   - refuse a path outside `store`;
   - hash the file with `dl::store::hash` (added here, in a new
     `dl/store.rs` that T8 extends with `put`) and compare it with the
     candidate's `hash` (`Hash`), before anything is mapped;
   - `Pack::open(path)`, then check `pack.def(ty, cx)` against the
     candidate's `def` (`Mismatch`);
   - drop the occupant;
   - create through the same code `register_pack`'s make uses, which
     moves into a free fn `dl::create(fns, lib, ty, params, instance,
     inputs, outputs)` both call;
   - set the current occupant.
5. `occupant()`: at make, build one `Occupant` per candidate: its index
   in `allow` (which is its index in the registry's `available`), its
   path's file name as a `FrameStr`, and its decoded `hash`. Return the
   current one.
6. `ended()`: `Unloaded` while empty, otherwise the occupant's.
7. Register as `fsw.dl_slot`.

Tests (`tests/dl.rs`, against the echo-pack fixture):
- `load` of a listed candidate creates it; `load` of an unknown name
  refuses; a path outside the store refuses;
- a stale `def` refuses with `Mismatch`, and a file whose bytes changed
  refuses with `Hash`; both keep the occupant;
- `occupant()` reports the loaded candidate's index, file name, and
  hash, and the channel's `channel_status` carries them;
- no channel fails build;
- `Load` then `Reset` restarts the loaded candidate through the channel
  adapter.

## T8. Pack store and the build pass

Files: `src/dl/store.rs`, `src/deploy/{mod,slot}.rs`,
`src/cli/run.rs`.

1. `dl/store.rs` gains `put(store: &Path, artifact: &Path) ->
   io::Result<Stored>`, returning `Stored { path, hash }` with `path`
   `<store>/<hex>/<file name>`. It hashes with the `hash` T7 added and
   copies through `copy_atomic` only when the path is absent. `store`
   is `<target dir>/.metor/store`.
2. Python emits each candidate with a `pack: PackRef`. `deploy::slot`
   resolves every `fsw.dl_slot` in the envelope:
   - open the candidate's pack from its `PackRef` (the same path
     `table_for` builds);
   - compute its def with the slot's `DefCx` (id, namespace, params,
     edges from the target's index);
   - check that all candidates agree (`DeployError::SlotContract {
     slot, candidate }`);
   - refuse a file name longer than `FrameStr::<64>::CAPACITY`
     (`DeployError::ArtifactName`);
   - `put` the artifact, and write `path`, `hash`, `def`, and the slot's
     `store` into the params. Drop `pack`.
3. `unresolved` also reports a slot candidate without `path`.
   `prepare` resolves slots whenever one is unresolved, which needs the
   target's store directory. `prepare` already has the target path.
4. The slot's `def_fn` needs `allow[0].def` before the target's other
   systems resolve against it. The slot pass runs before the peer pass,
   so peer resolution sees slot outputs.

Tests:
- `store.rs`: the same bytes give the same path with no rewrite (check
  the mtime); different bytes give a new path.
- `deploy/tests.rs`: a slot resolves to store paths and defs, and two
  disagreeing candidates fail.
- `tests/deploy.rs`: `metor build` on a fixture with a slot writes store
  paths that exist.

## T9. Python surface

Files: `python/metor-config/metor_config/{_target,_builtins,_model,__init__}.py`,
`src/cli/module.py.jinja` (stub generation), tests.

1. `Target.add(name, system, thread=None, channel=None, start=None)`.
   `thread` with `channel` raises `ConfigError`. `start` without
   `channel` raises too. `_system` emits `"thread"` or `"channel":
   {"name", "start"}`.
2. `Target.commands(*sources)` records command sources. `_inputs_for` on
   every channel entry appends `commands` from them at emission, so
   later-added channels are wired too.
3. Every channel's handle exposes `events` and `channel_status`.
4. `ChannelRegistry()`: builtin whose `_params_for` walks the target's
   channels, taking each `available` from a `DlSlot`'s allow names, and
   `period` from the cycle rate.
5. `DlSlot(allow=[..], initial=None, **inputs)`: each candidate is a
   pack system instance. It takes its ports from the first candidate's
   class, emitting `pack` from the candidate's `_pack` and not adding
   those packs to the target's `packs`. It raises if `initial` is not
   in `allow`.
6. Stub generation: a `#[sequence]` type gets the same stub a system
   does; `module.py.jinja` needs no change beyond a test.

Tests (`tests/test_target.py`):
- channel emission; `thread` with `channel` raises; `commands` wires
  channels added before and after it;
- the registry lists channels with slot names;
- `DlSlot` emits candidates with their packs and keeps them out of
  `packs`;
- the golden `target.json` updates.

## T10. Fixture, end to end, docs

Files: `tests/fixtures/echo-pack/**`, `tests/{run,dl}.rs`,
`DESIGN.md`, `TODO.md`, `src/lib.rs`, `docs/plans/12-sequences.md`.

1. The echo pack gains `#[sequence] count_up` and `#[sequence]
   count_down`, with the same ports: an `Input<Ping>`, an
   `Output<Ping>`, and a `log`. `count_up` completes after N cycles;
   `count_down` waits on `wait_for` until aborted.
2. The fixture target gains a `DlSlot` over both, and a `Subscribe` of
   `SequenceCommand` wired through `fsw.commands`. A `Publish` carries
   `mode.events`, `mode.channel_status`, and the registry.
3. `tests/run.rs`: under `metor run`, a test client sends `Load
   count_up`, `Start`, and waits for `Completed`. Then it sends `Load
   count_down`, `Start`, and `Abort`, and sees `Aborted`. It reads the
   registry and `channel_status` off the `Publish`.
4. Docs:
   - `DESIGN.md`'s Slots and Sequences sections describe what shipped;
   - `TODO.md` item 5 is marked done;
   - the `lib.rs` crate doc gets a `#[sequence]` example;
   - the design doc gains a "Shipped deviations" section.

## Order and risk

- T1 and T2 touch every make site and the ABI; do them first, alone.
- T4 is the core. `build.rs` passes the outer def everywhere and the
  inner def only to `channel::make`; a mix-up there shows as port-index
  bugs, and the Reset reader-slot test catches them.
- T5's macro work is the largest by line count. The `async move` step
  follows `thread::AsyncLaunch`.
- T8 is the only task that runs `dlopen` in `metor build`. `prepare`
  already opens target packs there, so no new residency rule applies.
