# Definitions from config

A follow-up to slice 4, decided 2026-09-20. A system's definition is a
function of its instance config, computed by the type. The coordinator
keeps no port-completion logic and `SystemDef` keeps no dynamic markers.

## Why

Slice 4 shipped dynamic ports as two markers on `SystemDef`
(`dynamic_inputs`, `dynamic_outputs`, each the name of the parameter
that takes the extra ports) plus completion code in `build.rs` that
copies a producer's `PortDef` onto a dynamic input and looks a record
name up for a dynamic output. The type says "I have a sink"; the host
fills it. Two halves of one behavior in two places, and a tail rule in
`Bindings` to reunite them at bind.

One rule instead: every system computes its own def from its config.
A static bundle ignores the config. `DynInputs` and `DynOutputs` do
their own completion in their `Param` impls, where the parameter order
already is. Build validates edges against the def it is given, the same
way for every system.

## Shape

```rust
/// What a type sees of one instance's config when it computes its def.
pub struct DefCx<'a> {
    /// Each config input port and the def of the port feeding it.
    pub inputs: &'a [(&'a str, &'a PortDef)],
    /// Each config output port and the record it names.
    pub outputs: &'a [OutputConfig],
    /// Every record the table knows, by name; `None` where registrations disagree.
    pub records: &'a Records,
}

pub enum DefError {
    FanIn { port: String },
    UnknownRecord { port: String, record: String },
    RecordConflict { record: String },
}

trait System        { fn def(cx: &DefCx<'_>) -> Result<SystemDef, DefError>; }
trait AsyncSystem   { fn def(cx: &DefCx<'_>) -> Result<SystemDef, DefError>; }
trait SystemInputs  { fn defs(cx: &DefCx<'_>) -> Result<Vec<PortDef>, DefError>; }
trait SystemOutputs { fn defs(cx: &DefCx<'_>) -> Result<Vec<PortDef>, DefError>; }
trait Param {
    fn append_defs(names: &mut Names<'_>, cx: &DefCx<'_>, defs: &mut Defs) -> Result<(), DefError>;
    ..
}
```

`DefCx::empty()` is the context with nothing in it; `def(&DefCx::empty())`
is the static def the descriptor carries and the renderer reads.

- A static param appends its one port and ignores `cx`.
- `DynInputs` appends one `PortDef` per `cx.inputs` entry whose name no
  declared port took, cloned from the producer's def under the config's
  name. Build has already grouped edges by port, so a second producer on
  one name is `FanIn`.
- `DynOutputs` appends one `PortDef` per `cx.outputs` entry, from
  `cx.records`, under the config's name.
- `Defs` records how many ports each parameter appended. `Bindings` hands
  each param exactly that many, so the tail rule and `take_dynamic` go.
  A dynamic param may sit anywhere in the signature.

`SystemDef` is `{ name, inputs, outputs }`. `SystemInputs::dynamic()`
and the two markers are deleted.

## Build

`resolve` computes defs in config order. For each system: group the
config's input edges by port; resolve each producer to a def already
computed (a producer later in the config, which is what a `loop()`
edge is, is `BuildError::DynamicFromLater` when the consuming port is
undeclared, and fine when declared, since a declared port needs no
producer def); build the `DefCx`; call `entry.def(&cx)`; map `DefError`
into `BuildError::Def { id, source }`. Then `check_port_names`,
`check_alignment`, the reserved `status` check, and edge validation run
on the returned def as they do today. `add_dynamic_outputs` and the
dynamic branch of `input_port` are deleted. `Records` is built once from
the table before the loop.

## Pack ABI

Version 5. One new export:

```c
uint32_t metor_fsw_def(RawSlice ty, RawSlice cx, uint8_t* dst, size_t capacity, size_t* written);
```

`cx` is the `DefCx` as JSON (owned form, `DefCxOwned`); the result is the
`SystemDef` as JSON in caller-owned storage, with the status words
`pack_def` uses, plus one for a `DefError`, which is written through
`dst` as JSON in that case. `pack_def` still returns every type's static
def, computed as `def(&DefCx::empty())`, and `PackSystemDef` gains
`takes_inputs: bool` and `takes_outputs: bool` so the renderer knows a
type takes a port list. The pack computes those by walking its params
once with a probe context. `create` is unchanged: it still receives
`Instance { id, def, thread }`, now the def the host got back from
`def`.

`Pack::def(ty, cx)` on the host side mirrors `Pack::open`'s descriptor
read: a bounded buffer, decode owned, free on either outcome.
`register_pack`'s entry computes its def through that call.

## Python

The renderer emits `items: Sequence[SystemHandle | OutPort] = ()` for a
type with `takes_inputs` and `records: Sequence[type[Record]] = ()` for
`takes_outputs`, wired into the same `inputs`/`outputs` config shape the
built-in `Publish` and `Subscribe` use, by moving that expansion from
`_builtins.py` into `System`. The built-ins then subclass the same base
with `_takes_inputs`/`_takes_outputs` set.

## Tasks

1. `DefCx`, `DefError`, `Records`, and the trait signature change, with
   every static impl ignoring `cx`. Derives and `#[system]` updated. All
   tests green before any dynamic behavior moves.
2. `DynInputs`/`DynOutputs` complete themselves; `Bindings` by count;
   markers and `dynamic()` deleted; build's completion deleted; the
   `DynamicFromLater` rule and test. The existing dynamic-port tests are
   the net.
3. ABI 5: the `def` export, `Pack::def`, `PackSystemDef.takes_*`,
   version pins in Rust, `metor-fsw-abi`, `metor_config._config`, the
   golden. A fixture system with a `DynInputs` param, exercised through
   `tests/dl.rs`.
4. Renderer and `_builtins.py` unification; golden module update;
   Python tests for a generated type taking a port list.
5. Docs: `05-links.md` "Dynamic ports" and "Build changes" rewritten;
   `04-packs.md` ABI section gains the export; `TODO.md`.

Each task one commit, tests and clippy green, fmt on the touched files
only.
