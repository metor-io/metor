# Fn systems: implementation plan

Implements `11-fn-systems.md`. Four tasks, in order. Each ends with
`cargo test -p metor-fsw-3`, `cargo clippy --all-targets` clean, and a
commit. T2 is the large one and cannot be split: removing the impl-block
expansion and changing `register` break every call site at once.

## T1. Macro, fn form

Files: `macros/src/system_attr.rs`, `macros/src/lib.rs`.

Adds the fn expansion beside the impl-block one, dispatching on the
parsed item, so T1 lands green before any trait changes. The emitted
code targets the T2 traits, so nothing uses it until T2; T1 tests are
expansion snapshots only.

1. Parse `syn::Item`. `Item::Impl` keeps today's path; `Item::Fn` takes
   the new one; anything else errors "`#[system]` goes on a fn".
2. `Signature` checks, each a `syn::Error` at the offending span:
   - no receiver (a method errors "`#[system]` goes on a free fn");
   - every parameter a plain ident pattern;
   - no named lifetimes in parameter types;
   - no `impl Trait` in argument position;
   - at most one `#[state]` parameter, typed `&mut S`;
   - async fns end in `stop: Stop`, via the existing `take_stop`.
3. `#[state]` is stripped from the parameter before any emission. The
   state type is `S` from `&mut S`, or `()` without one.
4. Emission for `fn nav<G>(..) where ..`, hidden names fixed as
   `__MetorSystem` and `__metor_body` so user items cannot collide:
   - outer: the user's attrs (docs, `cfg`, `deprecated`, `must_use`),
     visibility, name, generics, and where clause; no parameters;
     returns `impl Ports<State = S> + SystemFn + Deref<Target = fn(..)>`,
     or `impl Ports<State = S> + AsyncSystemFn` for async;
   - `fn __metor_body<G>(..)`: the user's parameters, lint attrs, and
     block with original spans; `async` kept;
   - `struct __MetorSystem<G>(PhantomData<fn() -> (G,)>)`, with `Ports`
     (`NAME` from the fn ident, unchanged casing), `SystemFn::call` or
     `AsyncSystemFn::call` destructuring the items tuple into
     `__metor_body::<G>`, and for sync fns `Deref` to the fn pointer
     type built from the parameter types, returning
     `&(__metor_body::<G> as _)`;
   - the per-parameter `assert_param` calls, spanned as today.
   Without a state parameter, `call` takes `_: &mut ()` and does not
   pass it on.
5. On any error, emit the user's fn with `#[state]` stripped plus the
   `compile_error!`, so the IDE keeps its types.
6. `DOC` joins the fn's doc lines through the existing `doc` helper,
   generalized over `&[Attribute]`.

Tests: `macros` unit tests that expand a sync, async, generic, and
stateless fn and assert the emitted `NAMES`, `NAME`, `State`, and the
absence of `#[state]` in the output.

## T2. Traits, table, and every crate call site

Files: `src/fn_system/{mod,set,tests}.rs`, `src/coordinator/table.rs`,
`macros/src/system_attr.rs`, `src/lib.rs`, `src/link/{publish,subscribe,db,mod}.rs`,
`src/cli/config.rs`, `src/coordinator/build.rs`, `src/tests/utils.rs`,
`tests/{pipeline,link,dl,deploy}.rs`, `tests/fixtures/echo-pack/src/lib.rs`,
`tests/ui/**`.

1. `fn_system/mod.rs`:
   - `Ports` gains `type State: 'static`.
   - `SystemFn::call(state: &mut Self::State, items)` and
     `AsyncSystemFn::call(state: &mut Self::State, items, stop)`, both
     associated fns.
   - `FnSystem<S>` and `FnAsyncSystem<S>` keep `PhantomData<S>`; their
     `System::State` and `AsyncSystem::State` become `S::State`, and
     `execute`/`run` call `S::call(state, ..)`.
2. `table.rs`: the system value is a type witness and is dropped.
   ```rust
   pub fn register<S: SystemFn, M, C: Ctor<S::State, M> + 'static>(
       &mut self, ty: &str, _system: S, ctor: C) -> Result<(), BuildError>;
   pub fn register_async<S: AsyncSystemFn, M, C: Ctor<S::State, M> + Send + Sync + 'static>(
       &mut self, ty: &str, _system: S, ctor: C) -> Result<(), BuildError>;
   ```
   `S::State` needs whatever `Send` bound the async entry already puts
   on `A::State`; carry it over, do not add new ones.
3. `system_attr.rs`: delete the impl-block path. `Item::Impl` errors
   "`#[system]` goes on a free fn; see docs/plans/11-fn-systems.md". The
   crate doc for the attribute in `macros/src/lib.rs` shows the fn form.
4. Hand-written impls in `fn_system/tests.rs` (`Doubler`, `Summer`,
   `Counter`, and the rest): `type State = Self`, `call(state, ..)`
   calls `state.execute(..)`. The module doc stays: these are the shape
   the attribute emits, written for a self-typed state.
5. Migrate every `#[system] impl` to a `#[system] fn`:
   - stateful `impl X { fn execute(&mut self, ..) }` becomes
     `fn x(#[state] s: &mut X, ..)`, body `self.` to `s.`; other
     methods stay in `impl X`;
   - unit-struct systems drop the struct and become stateless fns,
     registered with `Default::default`;
   - `async fn run(&mut self, .., stop)` becomes `async fn x(#[state] s:
     &mut X, .., stop)`;
   - `register("t", X::new)` becomes `register("t", x(), X::new)`.
   Sites: `src/lib.rs` doctest, `link/{publish,subscribe,db}.rs` and
   `link::register_builtins`, `cli/config.rs` tests, `build.rs` tests,
   `fn_system/tests.rs` (`Gain`, `Probe<T>` becomes `probe::<T>()`,
   `Link`, `Relay`, the log systems), `src/tests/utils.rs`,
   `tests/{pipeline,link,dl,deploy}.rs`, and the echo-pack fixture.
6. The fn name is the new `NAME`, so `SystemDef.name` changes where a
   fn name differs from the old snake-cased type (`NavState` gave
   `nav_state`). Update the pins that assert def names; the table key is
   unaffected.
7. `tests/ui`:
   - keep `system_bad_param` and `system_run_without_stop`, rewritten
     as fns;
   - `system_execute_and_run`, `system_no_execute`,
     `system_run_not_async`, `system_self_by_value` are deleted;
   - add `system_on_impl` (the migration error), `system_two_states`,
     `system_state_by_value`, `system_method` (a `&mut self` fn in an
     impl block), `system_named_lifetime`;
   - `pass/system_nav.rs` becomes the fn form and calls
     `nav()(Timestamp(0), ..)`; `pass/system_relay.rs` the async form;
     add `pass/system_generic.rs` calling `probe::<Imu>()` and
     `pass/system_reexport.rs` with `mod nav { #[system] pub fn nav(..) }
     pub use nav::nav;`.
   Regenerate `.stderr` with `TRYBUILD=overwrite` and read every diff.

Work order inside T2: steps 1 to 3, then `cargo check -p metor-fsw-3
--lib` until only call-site errors remain, then step 5 file by file.

Tests, beside the migrated ones, in `fn_system/tests.rs`:
- `Ports` consts read through `fn ports<P: Ports>(_: &P)` on a sync, an
  async, a generic, and a stateless system;
- a stateless system registers with `Default::default` and runs;
- a direct `nav()(..)` call writes its output;
- a generic system registered twice at two record types gives two defs.

## T3. ADCS example

Files: `examples/adcs-fsw3/systems/adcs/src/{lib,nav,ctrl,mode,plant}.rs`,
`examples/adcs-fsw3/tests/convergence.rs`.

The working tree has uncommitted edits in these files. Commit or
confirm them with the user before starting.

1. `NavState` to `fn nav(#[state] s: &mut NavState, ..)`, and likewise
   `ctrl`, `mode`, `plant`, each named for its module so `pub use
   nav::nav` works.
2. `lib.rs` registration takes `nav()` and friends.
3. Tests that called `execute` directly call `nav()(..)`.

Ends with `cargo test -p adcs-fsw3` (and its `systems/adcs` crate) and
the convergence test green, and pointing error unchanged from before.

## T4. Docs

Files: `src/lib.rs`, `macros/src/lib.rs`, `TODO.md`,
`docs/plans/03-authoring.md`.

1. Crate docs describe the fn form: `#[state]`, `Default::default` for
   stateless, `nav()(..)` for direct calls, async without `Deref`.
2. `03-authoring.md` gets one line under Builder: superseded by
   `11-fn-systems.md`. The body stays as the record of the decision.
3. `TODO.md` drops "`#[system]` impl blocks" for "`#[system]` fns".
