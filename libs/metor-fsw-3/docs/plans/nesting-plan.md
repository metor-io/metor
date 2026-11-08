# Nesting: implementation plan

Implements `docs/nesting.md`: shared path rules, `[P; N]: Param`,
`#[derive(SystemParams)]`, `#[derive(Fields)]`, and the Python stubs for
structured ports.

## Facts the plan relies on

- `Param` (`fn_system/param.rs`) has the wake sink on its associated types:
  `In<W>`, `Item<'a, W>`. Impls: `Input`, `Sample`, `Rng`, `Alarms`,
  `Records`, `Output`, `DynInputs`, `DynOutputs`, `Timestamp`, `Abort`, `Seq`,
  `Log`, tuples (`param.rs`), `Cap` (`io/param.rs`), `PublishInputs`
  (`link/publish.rs`), `SubscribeOutputs` (`link/subscribe.rs`).
- `InSet<S, W>` and `OutSet<S>` (`fn_system/set.rs`) hold `S::Params`'s `In<W>`
  and `Out`. Sequences use `In<NoWake>` (`sequence/step.rs`).
- `FnSystem::def` refuses `Sample`, `Cap`, `Rng`, and `Alarms` on async systems
  at def time with `DefError::{SampleOnAsync, IoOnAsync, RngOnAsync,
  AlarmsOnAsync}` (`fn_system/mod.rs:96`).
- Every async fn in the workspace writes `Input<T, Notifier>`. Cyclic fns and
  sequences write `Input<T>`.
- Names: `#[system]` emits `NAMES`, one per fn argument. `append_defs` and
  `bind_out` pop them with `param_name`. The tuple impl pops one per element,
  so a tuple fn argument consumes too many. `Input::def` and `Output::def` take
  `&'static str`; `PortDef::name` is a `Cow`.
- `bind_in` is positional; `InSet::from_views` asserts every binding was used.
- `#[system]` maps an argument type to its `Param` key with `key_type`
  (`macros/src/system_attr.rs:594`), stripping one reference. `check_kinds`
  rejects types by name per fn kind. `check_type` rejects named lifetimes but
  allows `'_`.
- Duplicate port names on a system already fail the build with
  `BuildError::DuplicateInput`/`DuplicateOutput` (`coordinator/build.rs:552`).
- `metor_fsw_3::Params` is the coordinator's config params type, re-exported
  at the crate root.
- Stubs: `module.rs` renders `module.py.jinja`. Inputs are `__init__` keywords
  typed `Sources[T]`, which is `Source[T] | Sequence[Source[T]]` (fan-in).
  Outputs are `OutPort[T]` attributes and keys of `_ports`.
  `System.__init__` (`_model.py`) takes `inputs: dict[str, Sources]`.
- `#[derive(Frame)]` (`macros/src/frame.rs`) calls metor-component's
  `struct_as_vtable_impl` with a frame id. That impl wraps every field in
  `with_timestamp` and `with_frame` inside `vtable_fields`.
  `RecordSchema::frame` (`record.rs:126`) is the only non-test caller of
  `vtable_fields` on a frame.
- Nothing in the workspace calls the derived `Componentize::sink_columns`,
  `Componentize::MAX_SIZE`, or a derived `Decomponentize`. The derived
  `Componentize` recurses without a path prefix.
- `nest` is used in `tests/utils.rs`, `tests/module.rs`, `mux/plan/tests.rs`,
  `examples/sysstats-fsw3/.../cpu/mod.rs`, and
  `examples/adcs-fsw3/contracts/src/frames.rs`. The four standalone derives
  are used in those plus `frame_fields/tests.rs`.
- `tests/golden` holds target, sim, and deploy JSON, which include frame
  schemas.
- Compile-fail tests use trybuild: `tests/compile_fail.rs`, `tests/ui/`.

## A. `Param<W>`

1. Change the trait to `Param<W: InputWake>` with `In`, `Out`, `Item<'a>`.
   Methods drop their `W` generics.
2. Impls:
   - `impl<T, W> Param<W> for Input<T, W>`; same for `DynInputs<W>`,
     `Abort<W>`, `PublishInputs<W>`.
   - `impl<W> Param<W>` for `Output<T>`, `DynOutputs`, `SubscribeOutputs`,
     `Timestamp`, `Log`, `Records`.
   - `Param<NoWake>` only for `Sample<T>`, `Rng`, `Alarms`, `Cap<K>`,
     `Seq<'static>`.
   - The tuple macro: `impl<W, $($P: Param<W>),*> Param<W> for (..)`.
   - `Refresh` and `SampleRefresh` are unchanged.
3. `FnSignature` gains `type Wake: InputWake` and bounds
   `Params: Param<Self::Wake>`. `SystemFn` and `SequenceFn` require
   `FnSignature<Wake = NoWake>`; `AsyncSystemFn` requires
   `FnSignature<Wake = Notifier>`. `InSet<S>` and `OutSet<S>` read
   `Param<S::Wake>`, so `InSet` drops its `W` parameter. (Deviation: an
   unbounded `Params` would need `S::Params: Param<W>` on every set, def, and
   `Ports` impl.)
4. `#[system]`/`#[sequence]`: `assert_param::<P, W>()` with `NoWake` for cyclic
   fns and sequences, `Notifier` for async.
5. Remove the four `*OnAsync` errors and their checks in `FnSystem::def`, and
   `Defs::{sampled, rng, alarms}` if nothing else reads them. Keep `grants`
   for IO wiring.
6. Tests: move the `SampleOnAsync`/`RngOnAsync`/`AlarmsOnAsync` tests
   (`fn_system/tests.rs:744`, `:1011`, `tests/alarms.rs:254`) to trybuild:
   `async_sample.rs`, `async_rng.rs`, `async_alarms.rs`, `async_cap.rs`,
   `async_nowake_input.rs` (`Input<T>` on an async fn).

## B. Port paths

1. `ParamPath<'a>`: a borrowed chain of segments (`Root`, `Name(&str)`,
   `Index(usize)`), `fn child(&self, &str)`, `fn index(&self, usize)`,
   `fn to_name(&self) -> Cow<'static, str>`. A path of one `Name` with a
   `'static` str stays borrowed.
2. `append_defs(at: &ParamPath, cx, defs)` and `bind_out(at: &ParamPath,
   writers)` replace the `names` iterator. Delete `param_name` and
   `ParamNames`.
3. `Param` gains `const FLATTEN: bool = false`. The tuple impl names element
   `i` from the fn's argument names: `FnSignature::NAMES[i]`, or the parent
   path when `P::FLATTEN`. The tuple impl is reached only as
   `FnSignature::Params`; it reads the names from a root `ParamPath::args(NAMES)`.
4. `Input::def`, `Output::def`, `DefCx::port_path` take `&str` and
   `Cow<'static, str>` where they store it.
5. `Writers` and output naming (`OutputBinding`) take the composed name.
6. `#[system]`/`#[sequence]` reject a tuple argument.
7. Tests: names for leaf params unchanged (existing tests pass as-is); a
   trybuild case for a tuple argument; Kani: `ParamPath::to_name` equals the
   dotted join of its segments, for paths up to depth 3.

## C. Arrays

1. `ArrayParam` marker trait, implemented by `Input`, `Output`, `Sample`,
   arrays of `ArrayParam`, and `SystemParams` structs.
2. `impl<W, P: Param<W> + ArrayParam, const N: usize> Param<W> for [P; N]`:
   - `append_defs`/`bind_out`: element `i` at `at.index(i)`.
   - `bind_in`: `core::array::from_fn(|_| P::bind_in(views))`.
   - `bind_out`: collect into a `Vec`, then `try_into` the array (make time).
   - `get`: `input.each_mut()` zipped with `output.each_mut()`, mapped through
     `P::get`. `refresh` and `get_sampled` the same; `get_sampled` allocates
     nothing.
3. `key_type`: `[X; N]` maps to `[key_type(X); N]`, recursively. Move
   `key_type` into a module shared with the `SystemParams` derive.
4. `check_kinds`' list stays name-based; arrays of non-`ArrayParam`s fail on
   the trait bound. Add `#[diagnostic::on_unimplemented]` to `ArrayParam`.
5. Stubs and Python for lists (see E1–E3 for inputs and outputs). A list
   keyword defaults to `None`, which wires nothing; `()` stays the leaf
   default. A system with dotted inputs sets `_input_ports`, every input by
   name, which `System.__init__` flattens against. `get_sampled` fills an
   `[Option<R>; N]` of the elements' `Refresh`es as it builds the items.
6. Tests: unit tests in `fn_system/tests` for `[Input; 3]`, `[Sample; 2]`,
   `[Output; 2]`, `N = 0`, a mixed system; defs named `fss.0` … `fss.2`; records
   reach the right element. trybuild: `[Timestamp; 2]`, `[Log; 2]`.

## D. `#[derive(SystemParams)]`

1. `macros/src/system_params.rs`. Input: a struct with one lifetime and named
   fields. Struct attribute `#[system_params(async)]`. Field attributes:
   `#[fsw(rename = "..")]`, `#[fsw(flatten)]`.
2. Emits `impl Param<W> for S<'static>` with `W = NoWake`, or `Notifier` with
   `async`:
   - `In` = tuple of the fields' keys' `In`; `Out` likewise.
   - `Item<'a> = S<'a>`, built field by field from `get`.
   - `append_defs`/`bind_out` call each field at `at.child(name)`, or at `at`
     when flattened.
   - `const FLATTEN: bool = true`.
   - `impl ArrayParam for S<'static>`.
3. Re-export as `metor_fsw_3::SystemParams`. No conflict with the
   `Params` type: derives live in the macro namespace.
4. Nested structs: a field of type `Inner<'a>` maps to key `Inner<'static>`,
   the same rule `key_type` uses for `Seq`.
5. Tests: a struct with arrays, a nested struct, an array of structs,
   `rename`, `flatten`; cyclic, sequence, and async (`#[system_params(async)]`)
   systems; port names match the doc's example. trybuild: `Sample` in an async
   struct, an async struct on a cyclic fn, a struct with two lifetimes.

## E. Stubs and Python

1. `module.rs`: build a port tree from each system's dotted input and output
   names. Leaves, index runs `0..N`, and named groups. A run that is not
   contiguous from 0 stays a named group.
2. `module.py.jinja`:
   - Inputs: leaf `Sources[T]`, list `Sequence[Sources[T]]`, group
     `<System>.<Path>` class with keyword-only children.
   - Outputs: leaf `OutPort[T]`, list `tuple[OutPort[T], ...]`, group
     `<System>.<Path>Out` class with `OutPort` properties.
   - `_ports` stays flat, keyed by dotted name.
3. `_model.py`:
   - `PortGroup` base for input group classes: validates keywords, keeps
     children.
   - `System.__init__` flattens lists and groups to dotted names before
     `_sources`; raises `ConfigError` on a wrong list length.
   - Output attributes: a group or list attribute resolves to the dotted
     `_ports` entries.
4. Tests: `module.rs` stub text for a list input, a group input, a list of
   groups, and the matching outputs; Python unit tests for flattening, length
   errors, and unknown keywords; pyright on a fixture using the stubs.

   As built: `cli/port_tree.rs` rebuilds the tree, `cli/port_stub.rs` renders
   keywords, attributes, and group classes. A group of inputs is a
   `PortGroup` subclass with `_input_ports` (paths under the group) and an
   `__init__` declared under `TYPE_CHECKING`, so the runtime constructor
   raises `ConfigError` on an unknown keyword. A group of outputs is an
   `OutGroup` subclass of annotations; the handle returns a plain `OutGroup`,
   whose `__getattr__` is hidden from type checkers. A port beside its own
   children, a segment Python cannot name (a keyword or `self` included), or
   two different groups of one class name fail stubgen with
   `PackDevError::PortShape` (M0409). The pyright fixture and a wiring check
   live in `tests/port_stubs.rs`, apart from `tests/module.rs`, which F
   touches.

## F. `#[derive(Fields)]` and frame changes

1. `Frame` gains `const STAMP: (u32, u32)`, the stamp field's offset and size.
   The `Frame` derive sets it.
2. `RecordSchema::frame` wraps every field from `T::vtable_fields(root)` in
   `with_timestamp(raw_table(STAMP))` then `with_frame(T::ID)`, in today's
   order. The `Frame` derive stops passing a frame id and a timestamp into
   `struct_as_vtable_impl` for its own `AsVTable`.
3. A field marked `timestamp` in a `Fields` struct is a compile error. A frame
   used as a field contributes its fields only; its stamp field is already
   filtered out of the components.
4. `#[derive(Fields)]` in `macros/src/fields.rs`: `AsVTable` and
   `Metadatatize` through the existing impls; `#[fsw(..)]` attributes.
5. `#[derive(Frame)]` stops emitting `Componentize`/`Decomponentize`; drop those
   bounds from `Frame`. Remove `nest` from `Field`. Add `flatten` and `rename`
   to `Field` (`rename` is the existing `component_id`; keep it as an alias).
   `flatten` chains no segment. Detect duplicate leaf names at derive time
   among a struct's own fields. The derive cannot see a flattened field's
   leaves, so the coordinator build also refuses a port whose frame holds
   one component id twice: `BuildError::DuplicateLeaf` (M0338). It reads the
   vtable's component ops, so a group's metadata entry is not a leaf.
   `struct_as_vtable_impl` takes a `Stamp`: `Omit` for `Fields` and `Frame`,
   `Attach` for the standalone `#[derive(AsVTable)]`, which keeps its
   behavior.
6. Accept `#[fsw(..)]` on `Frame` with `#[frame(..)]` as an alias.
7. Migrate the five `nest` users and the standalone-derive users to `Fields`.
8. Tests:
   - Golden: `tests/golden` unchanged, and a unit test that every frame in
     `tests/utils.rs` has the same `packet_id` and metadata before and after.
     Record the ids on the branch before step F starts.
   - A frame used as a field announces the outer stamp and frame id only.
   - `flatten`, `rename`, `skip` on `Fields`.
   - trybuild: `timestamp` in `Fields`, a `rename` collision, `nest`.
   - A flatten collision fails the build with `DuplicateLeaf`.

## G. Integration and docs

1. `tests/`: a pack with eight `Fss` producers at different rates feeding one
   `[Input<Fss>; 8]` cyclic system, wired from Python; the same through an
   async `#[system_params(async)]` struct.
2. Update `lib.rs` crate docs, the `#[system]` doc in `macros/src/lib.rs`, and
   `DESIGN.md` where they describe params or nesting.
3. Adversarial review by another agent.

## Order

A → B → C → D → E (inputs and outputs for lists land with C; groups with D)
→ F → G. F does not depend on A–E and can run in parallel in its own
worktree.

Each step leaves the workspace building and all suites green, and lands as
its own commit.
