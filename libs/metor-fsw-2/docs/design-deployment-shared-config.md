# Deployments: shared configuration

A deployment is a set of targets that are configured, built, and run
together. This document designs the shared-configuration part of
[the rough design](rough-deployment-design.md): the `Deployment` builder in
`target.py`, the IR it emits, and the `--target` selection on the CLI.

The other three parts, running and deploying, cross-target comms, and the
gateway, get their own documents. This one leaves them a seam and states what
it assumes about them. It does not design them.

## Design

A single `target.py` is already the unit the CLI evaluates, packages, and runs
(`src/cli.rs`, `src/wiring/py.rs`). The change is that the file's emission root
becomes a deployment, and a target is one member of it. A file with one bare
`Target` is a deployment of one. Nothing about a target, its `Wiring`, its
bundle, or its runtime manifest changes.

Three decisions follow from that:

1. A target's identity in a deployment is its `namespace`. It is already the
   prefix that keeps several targets' component ids disjoint in one db
   (`CoordinatorSpec::namespace`, `src/ir.rs`; `InitGraph::qualify`,
   `src/coordinator/init.rs`). Adding a second name would give one thing two
   names.
2. The IR grows an envelope, not a new target model. A deployment document is
   `{ ir_version, targets: [Wiring, …] }`. Each member is a complete `Wiring`,
   so `resolve`, bundles, `--check-ir`, and the panel's `WiringManifest`
   consumer (`libs/metor-panel/src/wiring/mod.rs`) keep reading the type they
   read today.
3. Selection happens once, in the CLI, right after ingestion. Everything
   downstream of `load_source` takes one `Wiring` as it does now.

## Goals

- One file defines several targets that share packs, a venv, and a pyproject.
- `metor-fsw run target.py --target fsw` runs one member of it.
- A single-target `target.py` is the degenerate case of the same mechanism,
  and keeps emitting a byte-identical `Wiring`.
- `build`, `package`, and `--check-ir` work per target with no new artifact
  formats.
- The `Deployment` object and the IR envelope are where the later sections
  attach.

## Non-goals

- Launching several targets from one command, log prefixing, and NixOS
  rendering (Running / Deploying).
- `Publish`/`Subscribe`, cross-target type checking, address allocation, and
  discovery (Cross target comms).
- The gateway target.
- A multi-target bundle. A bundle serves one resolved target
  ([packaging.md](packaging.md)) and still does.

## Python API

### `Deployment`

`Deployment` lives in a new `metor_config/_deployment.py` and is exported from
`metor_config/__init__.py`. It takes the targets it contains.

```python
from metor_config import Deployment, Downlink, Target, TcpServer
from adcs_pack import Fsw, Plant

plant = Target(cycle_rate=120.0, sim_dt=1 / 120, namespace="plant")
fsw = Target(cycle_rate=120.0, sim_dt=1 / 120, namespace="fsw")

plant_link = plant.state("link", TcpServer(addr="[::]:2240", name="plant"))
plant.add("plant", Plant(seed=42))
plant.add("downlink", Downlink(plant_link))

fsw_link = fsw.state("link", TcpServer(addr="[::]:2241", name="fsw"))
fsw.add("fsw", Fsw())
fsw.add("downlink", Downlink(fsw_link))

deploy = Deployment(targets=[plant, fsw])
```

`Deployment(targets=...)` records the list in order. The order is the order
the envelope lists them and the order `build` provisions them. A target may
belong to one deployment; a second `Deployment` naming it is a record-time
error, as is constructing two `Deployment` objects in one file.

Everything a target does today it still does inside a deployment: `add`,
`state`, `slot`, `connect`, `route`, `scope`, `Alarms`, `Presets`, `@system`.
No method moves to `Deployment`. Instance names stay bare inside each target;
`fsw.add("downlink", …)` and `plant.add("downlink", …)` do not collide,
because wiring resolves per target on bare names
([wiring.md](wiring.md), "Runtime manifest").

### Identity and namespaces

A deployment with more than one target requires every target to carry a
`namespace`. A deployment of one may leave it `None`, as a bare `Target` does
today. The rule is checked on the Rust side (below); the recorder emits what
it is given.

Namespaces must be distinct and none may be a dotted prefix of another.
`sat` and `sat.plant` would let `sat.plant.x` name a component in either
target. Dotted namespaces such as `fleet.sat1` and `fleet.sat2` are fine.

The mDNS node name is `TcpServer(name=)` today, falling back to the OS
hostname (`LinkParams::name`, `src/telemetry/link/mod.rs`). This document
does not change that default. The discovery section is expected to make the
namespace the advertised identity; see "Seams".

### The degenerate case

A file that constructs one `Target` and no `Deployment` emits a one-target
deployment. The example target `examples/adcs-fsw2/target.py` needs no edit.
`metor-fsw run` with no `--target` picks the only member. Its `Wiring`
inside the envelope is byte-identical to what the file emits today.

A `target.py` that is a member of a larger file cannot be run "standalone"
by pointing at a different file. There is one file; standalone is
`--target <ns>`. Splitting targets across files is rejected below.

### Emission

`_target.py`'s emission rule changes from "exactly one `Target`" to:

- one `Deployment` was constructed: emit it;
- none, and exactly one `Target` exists: emit `Deployment(targets=[it])`;
- otherwise: a `RuntimeError` naming the count, as today.

`emit()` accepts a `Deployment` or a `Target`. `_the_target()` is deleted;
its two callers are the emission rule and `Presets`, and both change.

The envelope moves the `metor_config_version` field up one level. It is
emitter provenance, not target identity, and the golden normalizer already
strips it (`python/tests/test_golden.py`).

### Process-global capture

`@system` and `Frame`/`State` capture is process-global (`_program` and
`_frames` in `_program.py`). `Target._program_ir` assembles every captured
entry whose `added` is set. In a two-target file that would put `fsw`'s
Python systems into `plant`'s program and vice versa.

The fix is one field. `Target._add_expr` records the owning target on the
entry (`entry["target"] = self`), and `_program_ir` includes a `@system`
entry only when its owner is `self`. Captured `Frame` and `State` classes
stay in every target's program; compile order is source order and an unused
class costs nothing on the vehicle. The "never added" warning moves to
deployment emission so it fires once, not once per target.

The program artifact id stays `program` inside each `Wiring`
(`PROGRAM_ARTIFACT`). It is an artifact id, scoped to its `Wiring` like an
instance name. Where two compiled modules land on disk is a build concern;
see "Build and packaging".

### `Presets` and namespace qualification

`Presets([...])` qualifies component references at construction by asking
`_the_target()` for the namespace (`_target.py`). With two targets that
question has no answer.

`Spec` gains a defaulted hook, `_bind(self, target: Target) -> None`, a
no-op on the base class. `Target.add` and `Target.slot` call it before
`_param_source()`. `Presets` returns a `Spec` subclass whose `_bind` renders
its params with `target.namespace`. The documented ordering constraint on
`Presets` ("the `Target` must exist first") goes away.

`Alarms` needs nothing: the alarm engine qualifies its targets from
`ctx.namespace` at configure time (`src/alarm/mod.rs`). Dashboards and
outlines reach the IR only through `Presets`, so they are covered by it.

### File convention and discovery

The file stays `target.py`. Zero-arg `run` keeps looking for `target.py` in
the current directory (`detect_target_in`, `src/cli.rs`); any `.py` path is
accepted explicitly. A file that defines a deployment is still the one file
that says what runs, and the CLI does not need its name to know how many
targets are in it. Whether to add `deployment.py` to the zero-arg lookup is
an open question.

The pyproject is shared. `refresh_source_packs` refreshes the path-source
packs of the directory the file sits in, for every target at once. A pack
that only one target uses is still refreshed; cargo makes that cheap.

## IR changes

### Shape

```json
{
  "ir_version": 10,
  "metor_config_version": "0.4.1",
  "targets": [
    { "ir_version": 10, "coordinator": { "namespace": "plant", … }, … },
    { "ir_version": 10, "coordinator": { "namespace": "fsw", … }, … }
  ]
}
```

Each member is a `Wiring` as `src/ir.rs` defines it today, `ir_version`
included. The nested version is redundant with the envelope's and that is
deliberate: a `Wiring` stays self-describing, because bundles, the
`WiringManifest`, and `resolve` all handle one on its own. The envelope
version gates the document; the member version gates the member.

### Rust types

`src/ir.rs` gains one type:

```rust
/// A deployment: the targets one `target.py` declares.
pub struct Deployment {
    pub ir_version: u32,
    pub targets: Vec<Wiring>,
}
```

with `path_stripped()` mapped over the members and one accessor,
`target(&self, namespace: Option<&str>) -> Result<&Wiring, LoadError>`,
holding the selection rule the CLI uses:

- one member and no request: that member;
- one member, a request, and the member's namespace equals it: that member;
- several members and a request: the member whose `coordinator.namespace`
  equals it, else `LoadError::UnknownTarget { requested, available }`;
- several members and no request: `LoadError::TargetRequired { available }`.

`Wiring` does not change. `WiringBuilder` does not change; a Rust-built
target is one `Wiring`, and a test that needs an envelope wraps it.

### Validation

`src/wiring/validate.rs` gains `validate_deployment(&Deployment)`, run by
`ingest_ir` as the one gate an emitted document passes, before selection:

- envelope `ir_version == IR_VERSION`;
- at least one member;
- with more than one member, every member has a namespace, namespaces are
  unique, and none is a dotted prefix of another.

Per-member checks stay where they are: `resolve` runs `validate(&Wiring)` on
the selected member as it does today. The namespace rules live here and not
in Python, following the alarm precedent: the Rust deserialize path is the
one source of truth, and a hand-edited or bundled document gets the same
gate as an emitted one.

### Version policy

`IR_VERSION` goes from 9 to 10 in both `src/ir.rs` and
`metor_config/_version.py`. The policy in the code is exact match, no
migration (`check_ir_version`, `src/wiring/validate.rs`; `Wiring::ir_version`
is not serde-defaulted). A v9 document is a flat `Wiring`; a v10 document is
an envelope. `ingest_ir` (`src/wiring/py.rs`) reads `ir_version` off the raw
value before deserializing, so a stale recorder fails with the version
message instead of a "missing field `targets`" serde error.

Every existing frozen `wiring.json` is a v9 `Wiring`. Bundles do not carry
an envelope, so a repackage under v10 changes their `ir_version` and nothing
else; the `ir_sha256` in `meta.json` changes with it. That is the same cost
as any IR bump.

`metor-config` goes to `0.4.1`. Pack wheels pin `metor-config>=0.4,<0.5`
(`src/wiring/wheel.rs`); the surface a generated pack module imports
(`Artifact`, `Frame`, `InPort`, `Msg`, `OutPort`, `System`) is untouched, so a
published pack keeps resolving. The new module must be added to
`EMBEDDED_PACKAGE` in `src/wiring/py.rs` or a venv-less run will not find it.

## CLI

### Selection

`run`, `build`, and `package` gain `--target <NS>`. `load_source` returns a
`Deployment`; each command calls `validate_deployment` and then
`deployment.target(args.target)`. Every override (`--serve`, `--cycle-rate`,
`--wall`, `--sim-dt`, `--cycles`) applies to the selected member, exactly as
it applies to the whole `Wiring` today.

```sh
metor-fsw run target.py --target fsw
metor-fsw run --target plant          # zero-arg lookup, then select
metor-fsw run                         # one member: no flag needed
```

Error surfaces, all before any build:

```text
deployment `target.py` has 2 targets; pick one with --target (plant, fsw)
deployment `target.py` has no target `fws`; targets: plant, fsw
target `target.py` has no namespace; `--target sat` does not apply
```

A bundle is one target and needs no flag. `--target` on a bundle is accepted
when it equals the frozen `coordinator.namespace` and is an error otherwise,
so a script that always passes the flag works on both source and bundles.

`run` with no `--target` on a multi-target deployment is the hook for the
Running section's launch-everything mode. Until that lands it is the error
above, worded as a missing flag, not as a rule.

### The triple flag

`build` and `package` already spell the Rust target triple `--target
<TRIPLE>` (`BuildArgs::triple`, `PackageArgs::triple`, `src/cli.rs`), as
sugar for `--cargo-arg --target …`. The rough design wants `--target <ns>`
for selection. Both cannot be `--target`.

This document renames the triple flag to `--triple <TRIPLE>` and gives
`--target` to selection. The `--cargo-arg --target <TRIPLE>` spelling still
works, `merge_target` is unchanged, and [cli.md](cli.md) and
[packaging.md](packaging.md) update their three examples. The alternative is
listed under open questions.

### `build`

`build` with no `--target` provisions every member in envelope order and
prints one block per namespace:

```text
  plant
    adcs-systems                 →  target/debug/libadcs_systems.dylib
  fsw
    adcs-systems                 →  target/debug/libadcs_systems.dylib
    program                      →  target/debug/fsw/program.wasm
```

`--target <ns>` builds one member. Shared crates build once; cargo's
incremental build makes the second provisioning a lookup.

### `package` and `--check-ir`

`package` requires `--target` on a multi-target deployment and writes a
bundle for that member. The bundle layout is unchanged: `wiring.json` is the
member `Wiring`, `target.py` is the whole file as provenance. Nothing else
identifies the member, and nothing else needs to: its namespace is in
`wiring.json`.

`package --check-ir <bundle>` re-evaluates the provenance copy to a
`Deployment`, selects the member whose namespace equals the frozen
`coordinator.namespace`, and diffs as today. A deployment of one with no
namespace selects its only member. `normalized_ir` moves from `Wiring` to
the member it is given; it does not change.

### Preflight

`print_preflight` (`src/cli/ui.rs`) shows the file name today. For a member
of a deployment it shows `target.py · fsw`. A one-target deployment with no
namespace shows the file name alone.

## Build and packaging

Provisioning is per `Wiring` and stays that way (`provision_artifacts`,
`src/wiring/build_driver.rs`). The one collision is the compiled Python
program: every member's program artifact is `program.wasm`, and
`wasm_out_dir` puts them in one directory. A deployment with Python systems
in two targets would overwrite one with the other.

`wasm_out_dir` gains the member's namespace as a subdirectory when the
member has one: `target/debug/fsw/program.wasm`. A bundle names its member
`program.wasm` as today (`member_artifact_name`, `src/wiring/bundle.rs`); one
bundle holds one member, so there is nothing to disambiguate there.

Pack wheels, `pack dev`, the `metor_build` backend
(`python/metor-build/metor_build/__init__.py`), and prebuilt selection do not
know about targets at all, and gain nothing here. A pack is shared by
whichever members import it; `refresh_dev_packs` runs once per file. A
cross-compiled `package --target fsw --triple aarch64-unknown-linux-gnu`
composes as its two halves do today.

## Stubgen and typing

Pack module generation (`src/wiring/pack_module.rs`) renders per pack, from a
manifest, with no target in scope. It is unchanged.

The typed builder surface gains `Deployment` and its `targets` attribute. The
`Spec._bind` hook is private. `Target.add`'s overloads, `connect`'s `F`
frame parameter, and the generated `System` subclasses do not change, so the
pyright gate (`python/tests/test_pyright.py`) needs only a `Deployment` in
`python/tests/data/demo.py` to cover the addition.

## Seams for later sections

Cross-target comms. `Publish` and `Subscribe` are expected to be ordinary
systems added to a target, so a member `Wiring` and its bundle stay
self-contained. The `Deployment` object holds every `Target` in one
interpreter, which is where a `Subscribe(to=plant_publish)` handle can be
checked against the peer's port types at record time. Whatever the comms
section needs at deployment scope (a link table, a message allow-list)
attaches as a new envelope field beside `targets`, keyed by namespace. This
document assumes such a field is additive and bumps the IR version when it
lands.

Service discovery and addresses. Assumed to attach as a per-member host
table on the envelope, keyed by namespace, and to default the mDNS node name
from the namespace. Nothing here allocates an address; each target still
declares its own `TcpServer(addr=)`.

Running / Deploying. `metor-fsw run <file>` with several members and no
`--target` is the entry point. NixOS rendering is assumed to consume one
bundle per member plus the envelope; `package --target` produces exactly
those bundles.

Gateway. Assumed to be one more `Target` in the deployment, with its own
namespace and a built-in state. The envelope needs no field for it.

## Alternatives considered

One file per target plus a manifest that lists them. Rejected: cross-target
typing needs both targets' handles in one interpreter, and each file would
need its own pyproject and venv for what is one dependency set.

One IR document per member, written to a directory. Rejected: the recorder's
atomic single-file emit, one digest, and one `METOR_IR_OUT` are simpler, and
selecting a member from a list is one line.

A deployment-assigned key, `Deployment(fsw=fsw, plant=plant)`, separate from
`namespace`. Rejected: the namespace already is the id-space prefix that
keeps members disjoint; a second key would need a rule tying the two
together.

Flatten every member into one `Wiring` under scopes. Rejected: a `Wiring` is
one coordinator, one clock, and one process; members are separate processes
on separate hosts.

Keep `IR_VERSION` at 9 and treat a document without `targets` as a one-member
deployment. Rejected: the code's policy is that version drift fails loudly,
and a document that is sometimes a `Wiring` and sometimes an envelope is a
dynamic catch-all.

Apply Python-side namespace checks as well as Rust-side. Rejected in favour
of one gate; the Rust error names the namespaces, which is what a user needs
to fix the file.

## Decisions

The review accepted the proposals above as written:

1. The triple flag becomes `--triple`; `--target <ns>` selects a member.
2. `target.py` stays the only zero-arg lookup name.
3. `metor-config` goes to `0.4.1`.
4. A one-member deployment may omit its namespace.
5. Captured `Frame`/`State` classes compile into every member's program.
6. Envelope order is preserved as given and `build` provisions in that
   order. Nothing else assigns meaning to it.

The implementation plan is [plan-deployment-shared-config.md](plan-deployment-shared-config.md).

## Testing strategy

Golden IR. A new `tests/golden/deployment.json` with two members, one of
them carrying a Python system, a `Presets`, and a scope, pinned by both
`python/tests/test_golden.py` and `tests/ir_contract.rs`. `target.json`
stays as the `Wiring`-level fixture and gets its `ir_version` bumped; the
Python golden test wraps it in a one-member envelope to prove the degenerate
path emits it unchanged.

Python. Emission rules: implicit one-member deployment, two `Target`s with
no `Deployment` error, two `Deployment`s error, one target in two
deployments error. Program partitioning: two members each adding a `@system`
emit disjoint `program.decls`, and the never-added warning fires once.
`Presets` deferred qualification: a preset added to a namespaced member
qualifies with that namespace, whichever target was constructed first.

Rust. `validate_deployment`: version, empty, missing namespace with several
members, duplicate, dotted-prefix overlap. `Deployment::target`: all four
selection arms. `ingest_ir`: a v9 flat document fails on version, not on
shape. `py_eval.rs`: a `deployment_target.py` fixture with two members
evaluates and selects. `wasm_out_dir` puts two members' programs in two
directories.

CLI. `detect_target_in` unchanged. `run --target` unknown, missing, and
bundle-mismatch errors. `package --target` writes a member bundle whose
`coordinator.namespace` is the request, and `--check-ir` on it passes. The
example bundle test (`examples/adcs-fsw2/tests/bundle.rs`) keeps passing
with no edit to `target.py`.
