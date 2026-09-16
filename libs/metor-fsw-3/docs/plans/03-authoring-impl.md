# Authoring surface: implementation plan

Implements `03-authoring.md`. Eight tasks, in dependency order. Each task
ends with `cargo test -p metor-fsw-3` green, `cargo clippy --all-targets`
clean, and a commit. Later tasks build on the public surface the earlier
ones fix, so do not reorder. T4 and T6 are the large ones; the rest are
renames and one-file additions.

Layout after this slice:

```
src/
  record.rs        Record, EncodeError, DecodeError, postcard fns
  frame.rs         Frame: Record
  port.rs          Input<T>, Output<T>, SendError, RecvError
  system.rs        PortDef, SystemDef, System, bundle traits
  fn_system/       Param and its tuple impls, SystemFn, FnSystem, Ctor
  log.rs           LogEvent impl, Log handle, tracing layer, drain
  coordinator/     + params.rs (Params, ParamError), factories in table.rs
macros/src/
  record.rs        #[derive(Record)]
  system_attr.rs   #[system]
examples/adcs-fsw3/
```

## T1. Record

Files: `src/record.rs`, `src/frame.rs`,
`src/system.rs`, `src/coordinator/{build,error}.rs`, `src/lib.rs`,
`macros/src/{frame,record,lib}.rs`, `Cargo.toml`, `macros/Cargo.toml`.

1. `record.rs`: the `Record` trait from the design doc with `NAME`, `ID`,
   `MAX_LEN`, `ALIGN = 1`, `DEPTH = 1`, `type Read<'a>`, `encode`,
   `decode`. `EncodeError { Oversize { len, max }, Codec }` and
   `DecodeError { Truncated, Codec }`, both `Copy` and `thiserror`.
2. `record::postcard`, a `pub mod` at the bottom of `record.rs`:
   `encode<T: Serialize>(value, buf) -> Result<&[u8], EncodeError>` over
   `postcard::to_slice`, mapping buffer-full to `Oversize`;
   `decode<T: DeserializeOwned>(bytes) -> Result<T, DecodeError>` over
   `postcard::from_bytes`. About twenty lines. Nothing else in the crate
   names postcard.
3. `frame.rs`: `Frame: Record + <the component and zerocopy bounds>` with
   only `timestamp()`. `NAME` and `ID` move to `Record`.
4. `macros/src/frame.rs`: also emit `impl Record` with
   `MAX_LEN = size_of::<Self>()`, `ALIGN = align_of::<Self>()`,
   `Read<'a> = &'a Self`, `encode` returning `self.as_bytes()`, `decode`
   via `ref_from_prefix` mapped to `DecodeError::Truncated`.
5. `macros/src/record.rs`: `#[derive(Record)]` with
   `#[record(name, max_len, depth)]`. `name` defaults to the snake-cased
   ident, as `frame.rs` does. `max_len` defaults to
   `<Self as MaxSize>::POSTCARD_MAX_SIZE`. `Read<'a> = Self`; the two
   bodies call `record::postcard`. Reuse the `fsw_crate()` probe.
6. `system.rs`: `PortDef { name, id, max_len, alignment, depth }`.
   `Input::def` and `Output::def` read the `Record` consts.
7. `build.rs` and `error.rs`: `RingSpec.frame` and `FrameMismatch` become
   `id` and `IdMismatch`; `RingSpec` carries `depth`; `allocate_ring`
   passes `spec.depth * ring_depth` to `ring_capacity`.
8. Deps: `postcard` with `experimental-derive`, `serde`. `lib.rs`
   re-exports `Record`, the two errors, `postcard`, `serde`, and
   `postcard::experimental::max_size::MaxSize`, which postcard 1.1
   publishes as the trait and the derive under one name, so a message
   derives it through this crate.
9. `src/tests/utils.rs`: a `Note { text: String }` message with
   `max_len = 64` and a `Fixed { a: u32, b: f64 }` message with no
   attribute, for the tests below and T2.

Tests: `record.rs` bottom: `Fixed::MAX_LEN == Fixed::POSTCARD_MAX_SIZE`;
`Note::NAME == "note"`; a frame's `encode` output equals `as_bytes()`
and ignores `buf`; a frame's `decode` of a short slice is `Truncated`;
`Fixed` round-trips through `encode` and `decode`. `trybuild` under
`tests/ui/`: a message with a `String` field and no `max_len`.

## T2. Ports

Files: `src/port.rs`, `tests/pipeline.rs`, `src/coordinator/run.rs`.

1. `Output<T: Record>`: `try_new` allocates a `Vec<u8>` scratch of
   `T::MAX_LEN` and keeps the alignment check against `T::ALIGN`. `write`
   calls `encode` on the scratch, rejects a slice longer than `MAX_LEN`
   with `SendError::Oversize`, then `try_write`. `SendError { Oversize {
   len, max }, Encode(EncodeError), Ring(WriteError) }`.
2. `Input<T: Record>`: `drain` yields `Result<T::Read<'_>, RecvError>`
   with `RecvError { Ring(ReadError), Decode(DecodeError) }`. A decode
   failure yields the error and the iterator continues with the next
   record.
3. `Input<T: Record>`: `latest` over any record, ordered by
   `Record::timestamp`, returning `Latest<'_, T>` with `read()` and a
   `Deref` for frames. Amended after T6: the first cut kept `latest` on
   the `Frame` bound.
4. `run.rs`: the status write's discarded error type changes; nothing
   else.
5. `tests/pipeline.rs`: `write` return type only.

Tests: existing port tests updated; `Note` round trip through `write`
and `drain`; a `Note` longer than 64 bytes is `Oversize` and the ring's
`committed()` is unchanged; a hand-corrupted `Fixed` record is `Decode`
and the next record still arrives; a frame drains as `&F` and `Fixed`
as an owned value; the frame port over a `Note`-id ring is
`IdMismatch` at build, not a port-level check. Kani: `write` never
passes the ring a slice longer than `MAX_LEN`.

## T3. Params and factories

Files: `src/coordinator/{params,config,table,error,build}.rs`,
`src/coordinator/mod.rs`, `src/tests/utils.rs`, `Cargo.toml`.

1. `params.rs`: `Params<'a>(&'a serde_json::Value)` with
   `decode<P: DeserializeOwned>(self) -> Result<P, ParamError>`. `Null`
   decodes as an empty object. Decode through `serde_ignored`; the first
   ignored path is `ParamError::UnknownKey(String)`; a serde error is
   `ParamError::Decode(String)`.
2. `config.rs`: `SystemConfig.params: serde_json::Value`, `#[serde(default)]`.
3. `table.rs`: `SystemFactory { type System; fn def(&self) -> SystemDef;
   fn make(&self, params: Params<'_>) -> Result<(System, State),
   ParamError> }`, a blanket impl for
   `Fn(Params<'_>) -> Result<(S, S::State), ParamError>`, and
   `register_system(ty, factory)`. The erased `make` in `TableEntry` now
   takes `Params<'_>` and returns `Result<Box<dyn Step>, ParamError>`.
   `register` for fn systems arrives in T4.
4. `error.rs`: `BuildError::Params { id: String, source: ParamError }`.
5. `build.rs`: `bind_rings` becomes fallible and passes each entry's
   params to `make`.
6. `utils.rs`: the test table uses `register_system` with `|_| Ok(..)`
   closures.
7. Deps: `serde_json`, `serde_ignored`.

Tests: `Null` decodes a struct whose fields all have serde defaults; a
missing required field is `Params { source: Decode }`; an unknown key is
`Params { source: UnknownKey }` naming the key; a config literal with
`params` round-trips through serde.

## T4. Fn systems

Files: `src/fn_system/{mod,param,set,ctor}.rs`, `src/coordinator/table.rs`,
`src/lib.rs`.

1. `param.rs`: the `Param` trait from the design doc with `In`, `Out`,
   `Item<'a>`, `defs(name, ins, outs)`, `bind(views, writers)`, `get`.
   `Views` and `Writers` are `vec::IntoIter` aliases. Impls for
   `&mut Input<T>`, `&mut Output<T>`, and `Timestamp`. `Log` arrives in T6.
2. `param.rs` also implements `Param` for tuples of arity 0 to 16
   through one `macro_rules!`, each half the tuple of the elements'
   halves; names arrive as an iterator every leaf advances once.
   `set.rs` holds `InSet<S>` and `OutSet<S>`, which wrap the tuple's
   `In` and `Out` and implement `SystemInputs` and `SystemOutputs`.
   Amended after T6: the first cut had a separate `ParamSet` trait for
   tuples, which duplicated `Param`.
3. `mod.rs`: `SystemFn: 'static { type Params: Param; const NAMES:
   &'static [&'static str]; const NAME: &'static str; fn call(&mut self,
   now, items: Items<'_>) }`. `FnSystem<S>` implements `System` with
   `State = S`, `Inputs = InSet<S::Params>`, `Outputs = OutSet<S::Params>`;
   `execute` is `S::Params::get` then `state.call`.
4. `ctor.rs`: `Ctor<S, M> { fn make(&self, params: Params<'_>) ->
   Result<S, ParamError> }` with `impl<S, F: Fn() -> S> Ctor<S, ()>` and
   `impl<S, P: DeserializeOwned, F: Fn(P) -> S> Ctor<S, (P,)>`.
5. `table.rs`: `register<S: SystemFn, M>(ty, ctor: impl Ctor<S, M> +
   'static)` builds the `SystemDef` from `S::NAME`, `S::NAMES`, and
   `S::Params::defs`, and wraps the ctor in a `SystemFactory`.
6. `lib.rs`: export `Param`, `SystemFn`, `FnSystem`, `Ctor`.

Until T5 lands, tests implement `SystemFn` by hand for a two-port
struct; that hand impl is what the attribute emits, so it doubles as the
macro's expected-expansion fixture.

Tests: defs follow parameter order and skip `Timestamp`; outputs before
inputs binds; two `Input<Imu>` parameters bind to two rings and read
independently; a unit-struct system registers with `Default::default`; a
`Fn(P) -> S` ctor receives the decoded params; a `Fn() -> S` ctor
ignores a `Null` params; the three-system pipeline from `utils` rebuilt
on fn systems produces the same records as the trait version.

## T5. The `#[system]` attribute

Files: `macros/src/system_attr.rs`, `macros/src/lib.rs`, `tests/ui/`,
`tests/pipeline.rs`.

1. `#[proc_macro_attribute] system` on an `impl` block. Find the method
   named `execute`; a block without one is an error on the block. Take
   the receiver as `&mut self`; any other receiver is an error on it.
2. Collect every other parameter's ident and type in order. Emit the
   block unchanged, then `impl SystemFn for <Self>` with
   `Params = (<types>,)`, `NAMES = [<idents>]`, `NAME` as the snake-cased
   type ident, and `call` destructuring `items` and calling
   `self.execute(..)`. The attribute never inspects a type beyond
   quoting it back; a type that is no `Param` fails at the emitted impl,
   and the expansion spans that failure on the parameter.
3. Doc comments on `execute` and its parameters are collected but unused
   in this slice; keep them in the parsed struct so the packs slice does
   not reparse.
4. `tests/pipeline.rs`: rewrite `Gyro`, `NavFilter`, `ControlLaw` as
   `#[system]` blocks; the monitor keeps the trait path so both paths
   stay covered.

Tests: `trybuild` pass case with the doc's `nav` signature; fail cases
for a block without `execute`, a by-value `self`, and a parameter of
type `u32`, each asserting the error's location. Expansion equality
against the T4 hand impl through the pipeline test's records.

## T6. Log

Files: `src/log.rs`, `src/fn_system/{param,set,mod}.rs`, `Cargo.toml`,
`src/lib.rs`.

1. `log.rs`: `impl Record for LogEvent` with `name = "log"`,
   `MAX_LEN = 4096`, `DEPTH = 8`, postcard bodies. `Log` wraps
   `&mut Output<LogEvent>` plus `now` and offers `info`, `warn`, `fault`,
   each writing one stamped `LogEvent` with `source` set to the tracing
   target argument or empty.
2. Queue: a thread-local `RefCell<Vec<LogEvent>>` with capacity
   `MAX_LINES = 64` reserved once, and a dropped counter. `clear()`,
   `push(event)`, and `drain(now, out: &mut Output<LogEvent>)`, which
   writes every queued line stamped with `now`, then one `warn` line
   naming the dropped count when nonzero.
3. `layer()`: a `tracing_subscriber::Layer` whose `on_event` builds a
   `LogEvent` from the event's level, target, message, fields, file, and
   line, and pushes it. Span scope is left `None` in this slice.
4. `param.rs`: `impl Param for &mut Log` with `In = ()`, `Out = ()`; its
   `get` borrows the `log` slot, which the tuple `Param::get` passes down as an
   extra argument.
5. `set.rs`: `OutSet::bind` takes the trailing writer into the `log`
   slot, and `OutSet::defs` appends the `log` `PortDef` last.
6. `mod.rs`: `FnSystem::execute` is `queue::clear()`, `S::Params::get`,
   `state.call`, `queue::drain(now, log)`.
7. Deps: `tracing`, `tracing-subscriber` with `registry`.

Tests: `Log::info` and `tracing::info!` inside `execute` both land on
`<id>.log` with the cycle stamp, in that order; a tracing event emitted
between two systems' executes is not on either ring; the 65th line in a
cycle is dropped and one warning line reports `1`; a fn system without
a `Log` parameter still has a `log` output the monitor can read; the
`log` ring holds eight times `ring_depth` records. The tracing tests
install the layer with `tracing::subscriber::with_default`.

## T7. ADCS gate

Deferred to slice 3 with T8: the example is rewritten for packs there,
so it lands once and against the ABI.

Files: `examples/adcs-fsw3/{Cargo.toml,src/{lib,contracts,plant,nav,ctrl,mode,main}.rs,tests/converge.rs}`,
workspace `Cargo.toml`.

1. One crate, `adcs-fsw3`, a workspace member, depending on
   `metor-fsw-3`, `metor-adcs`, `nox`, `nox-frames` (`earth`), `wmm`,
   `hifitime`, `rand`, `rand_distr`, `serde`, `tracing`.
2. `contracts.rs`: the frames and params structs from
   `examples/adcs-fsw2/contracts/src/lib.rs`. Mechanical edits:
   `#[metor_fsw(..)]` to `#[frame(..)]`; drop the `Schema` and
   `ParamsDocs` derives; keep `#[serde(default = ..)]` attributes.
3. `plant.rs`, `nav.rs`, `ctrl.rs`: the three systems from
   `examples/adcs-fsw2/systems/adcs-systems/src`. Each execute fn
   becomes the `execute` method of a `#[system] impl <State>` block;
   `publish(&f)` becomes `write(&f)` with the error discarded as before;
   `tracing` calls stay.
4. `mode.rs`: a unit struct whose `execute` writes a `ModeCmd` for
   nadir pointing every cycle.
5. `lib.rs`: `pub fn table() -> SystemTable` registering the four, and
   `pub fn config(clock) -> CoordinatorConfig` with `plant` first and the
   edges from the fsw-2 target's `connect` lines, `ctrl` outputs feeding
   `plant` as the backward edge.
6. `main.rs`: install the log layer beside a `fmt` layer, build, `run`
   until Ctrl-C on the wall clock.
7. `tests/converge.rs`: simulated clock at 120 Hz with the fsw-2
   target's params, wheels armed at boot, step a fixed cycle count, and
   assert the tracking error to the nadir target at the end is below a
   bound. fsw-2 no longer ships such a test, so take the bound from a
   first run, pin it with margin, and record both numbers in the commit
   message. Also assert `nav.log` carries the first-fix line.

Done when `cargo test -p adcs-fsw3` passes and `cargo run -p adcs-fsw3`
prints log lines at a steady cycle rate.

## T8. Docs

Files: `src/lib.rs`, module docs, `docs/plans/01-base.md`.

1. Crate docs: the `#[system]` example from the design doc as a doctest
   that builds a two-system table and steps once.
2. Module docs for `record`, `fn_system`, and `log` say what shipped, in
   the design doc's words, no more.
3. `01-base.md` gets a one-line pointer at the top to `03-authoring.md`
   for the port and system surface, as it already has for the ring.

## Review

After T8, an antagonistic review by a second agent against
`03-authoring.md` and the style guide, focused on: allocation on the
`execute` path outside log lines; panics reachable from config or from a
malformed record; `Param::get` lifetime soundness across the tuple;
whether the attribute inspects any type; and that postcard appears only
in `record::postcard` and `LogEvent`'s impl.
