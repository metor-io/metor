# Plan: deployments, revision D (presets on the gateway)

Implements change 4 of
[design-deployment-revision.md](design-deployment-revision.md): preset,
dashboard, outline, and alarm builders take port handles from any member and
render the fully qualified component path themselves; the adcs example's
dashboards and presets move to `gw`; the panel changes nothing. Five work
packages. Each ends with a green tree. At the end the gateway fixture ships a
preset naming a component of another member, the panel sees it through the
gateway's db, and the example's `adcs-dashboard` opens from `gw`. Paths are
relative to `libs/metor-fsw-2` unless they start with `libs/`, `examples/`,
or `python/`.

Test commands used throughout:

```sh
cargo test -p metor-fsw-2                                    # WP1, WP3
cargo clippy -p metor-fsw-2 --all-targets                    # lints, house rule
cargo test -p metor-fsw-2 --lib pack_module                  # WP1 codegen
cargo test -p metor-fsw-2 fixture_dump -- --ignored          # WP1: regenerate python/tests/data/demo.py
cargo test -p metor-fsw-2 --lib alarm                        # WP3
cargo test -p metor-fsw-2 --test ir_contract --test py_eval  # WP2–WP4 goldens
(cd python && uv run python -m unittest discover tests)      # WP1–WP4; pyright optional
cargo test -p metor-fsw-2 --test gateway                     # WP4
cargo test -p adcs-fsw2                                      # WP5 (skip w/o python3)
cargo check -p metor-panel                                   # WP3 (IR_VERSION is shared)
```

## Sequencing

```text
WP1 ─┬─> WP2 ──┐
     └─> WP3 ──┴─> WP4 ─> WP5
```

WP1 (the reference model: what a handle carries and how a path renders) is
the root. WP2 (presets, dashboards, outline) and WP3 (alarms, the Rust prefix
removal, the IR bump) touch disjoint files and run in parallel after it; both
must land before WP4 (fixture, integration test, goldens, docs), which pins
the rendered paths end to end. WP5 (the example) needs WP4's proof.

## Deviations from the design doc

Found while grounding the plan. The design doc is corrected where noted.

1. The design's `Trace(plant_sim.sensors, element=1)` renders
   `plant.plant.sensors.gyro_b`, but a port handle names a frame, not a
   field: `plant_sim.sensors` is the `sensors` frame whose fields are
   `gyro_b`, `css`, `mag_b`. The field must be named. This plan makes
   `Component(plant_sim.sensors, "gyro_b", element=1)` the one form for a
   field inside a frame, for widgets and alarms alike, and rejects attribute
   access (`plant_sim.sensors.gyro_b`); see "Verified facts" for the pyright
   reasoning. Design doc corrected.
2. The path segment after the instance is the frame's `#[metor_fsw(name)]`
   (`prefix_vtable`, `core/src/descriptor.rs:382`; `frame_name`,
   `src/wiring/pack_module.rs:546`), not the port name. A Python handle knows
   only the port name, so the generated pack module gains a port-to-frame
   map (`_frames`) on each `System` subclass, and a handle with no generated
   class behind it (a slot, the coordinator, a bare `System(..)`) falls back
   to the port name. Every port in the adcs pack, the demo fixture, and the
   host's `system_status` has frame name equal to port name, so the example
   renders the same either way.
3. "`Presets` qualification is extended, not changed" holds for presets; it
   does not hold for alarms. `AlarmSystem::configure`
   (`src/alarm/mod.rs:458–470`) prefixes every target with `ctx.namespace`
   unconditionally, so a handle target on a gateway would render
   `gw.plant.plant.sensors.gyro_b`. The prefix moves to the Python builder
   at record time, where presets already do it, and the Rust side stops
   rewriting: one rule, "component references in params are fully qualified
   when recorded". A Rust-authored target (`WiringBuilder`) writes qualified
   names, as it already must for presets (`src/preset/mod.rs:12`). Because
   an existing bundle's alarm targets change meaning under the new rule,
   `IR_VERSION` goes `11 -> 12` (WP3). Design doc corrected.
4. The "Plans" list says the example's "fleet-level alarms" move to `gw`.
   The alarm engine resolves against its own member's registry
   (`resolve_targets` walks `output.all`, `src/alarm/mod.rs:595–660`), and
   before plan C a gateway holds no member rings, so a `gw` alarm on a
   member's component records fine and disables itself at init
   (`alarms_unresolved_target`). This plan adds the record-time rule that
   lets a gateway alarm name an ingested member's component and puts no alarm
   on the example's `gw`; the example's onboard alarms stay on `fsw`. Plan C
   makes the gateway case resolve and owns its integration test.
5. The example has no `Outline` (`examples/adcs-fsw2/target.py` imports
   none). "Dashboards, outline, and presets move to `gw`" is read as: the
   outline builder takes handles (WP2) and the example gains one outline tab
   on `gw` (WP5), which the reviewer may strike.
6. Two of the example's references name no component. `StateChip(
   "mode.mode_cmd", element=0)` and `element=1` address the ring entry
   `fsw.mode.mode_cmd` (the key `Coordinator::registry().view` takes,
   `examples/adcs-fsw2/tests/sequences.rs:80`), whose components are
   `mode_cmd.mode` and `mode_cmd.law` (`examples/adcs-fsw2/contracts/src/lib.rs:447–455`).
   They become `Component(mode.mode_cmd, "mode")` and `Component(mode.mode_cmd, "law")`
   (WP5).
7. "A record-time check that a handle-referenced member is in the
   deployment" needs the set of targets a layout references, and the render
   path threads only a namespace string through ~40 `_state`/`_widget`
   signatures. Rather than change every signature, a small generic walk over
   dataclass fields collects the handles once at bind (WP2). The check for a
   bare `Target` outside any `Deployment` is the same one `_resolve_peer`
   makes (`_deployment.py:59–63`).

## Verified facts

| Fact | Where | Consequence |
| --- | --- | --- |
| `_qualify(name, ns)` is `f"{ns}.{name}"`; every widget calls it on `component`, `pattern`, `path`, `root`, `rows`, `expanded`, `collapsed`, `Bind.component`, `VectorMarker.component`; `SequenceControl.channel`, `Pivot.fields/hidden/rows`, `FrameType.fields/order/hidden`, and raw `PaneState` dicts are never qualified | `python/metor-config/metor_config/_dashboard.py:23,84,111,131,152,211,239,279–285,340,376,406,427,449,476,586` | the substitution is `_qualify(x, ns)` -> `_ref_path(x, ns)` at exactly those sites |
| `_Presets._bind(target)` renders `p.to_json(target.namespace)` at `Target.add`; the namespace is the adding target's | `_builtins.py:207–221`, `_target.py:235` | a bare string keeps this; a handle renders from its own target |
| `Alarms(alarms)` is `static_system("Alarms", alarm=[a.to_json() …])`, rendered at construction, before any `_bind`; `Component(component: str, element: int | None)`; `Alarm.target: Component` | `_builtins.py:21–29,44–84` | `Alarms` becomes a `_bind`-rendering spec like `_Presets` |
| Every `Component(..)` call site passes `element=` by keyword | `examples/adcs-fsw2/target.py:120,131`; `python/tests/test_golden.py:222`; `python/tests/test_recorder.py:243,431,435` | `Component(ref, field=None, element=None)` breaks no caller |
| `AlarmSystem::configure` rewrites `spec.target.component = format!("{ns}.{}", ..)` and re-hashes; `TargetSpec { component: String, element: Option<usize> }`; the only Rust test of the prefix is `namespace_prefixes_alarm_targets` | `src/alarm/mod.rs:60–67,458–470`; `src/alarm/tests.rs:533–580` | WP3 deletes the rewrite and flips the test to an authored-qualified target |
| `PortRef(instance, port)` carries nothing else; `SystemHandle(name, target=None, spec=None).__getattr__` -> `PortRef(self.name, name)`; `Target.coordinator = SystemHandle(COORDINATOR)`, `slot` returns `SystemHandle(full)`, `ExprHandle` is built with no target and `_add_expr` sets only `entry["target"]` | `_model.py:139–182`; `_target.py:68,332,250–290`; `_program.py:66–82,107–118` | WP1 threads `target` into all four and `frame` into `PortRef` |
| A telemetered component's id is `ComponentId::new("<ns>.<instance>.<frame>.<field>")`: `prefix_vtable` prefixes the frame's metadata names with the instance, the metadata's leading segment is the frame's `#[metor_fsw(name)]`, and the port name is not involved | `core/src/descriptor.rs:216,382–400`; `src/wiring/pack_module.rs:236–260,546–551` | the handle must know the frame name, hence `_frames` |
| Frame name equals port name for all eleven adcs frames (`sensors`, `gps`, `wheels`, `body`, `world`, `disturb`, `torque_cmd`, `mtq_cmd`, `attitude_estimate`, `mode_cmd`, `system_status`), the demo fixture's three, and the host status port | `examples/adcs-fsw2/contracts/src/lib.rs:222–455`; `examples/adcs-fsw2/systems/adcs-systems/.metor/adcs_pack/__init__.py:143–150`; `src/wiring/pack_module/tests.rs:65–116,138–156`; `core/src/status.rs:32` | the example's paths are unchanged by the frame/port distinction |
| A `@system` output port is named by `_output_frame` (the declared frame's snake case, else the function name) and the sugar form's one component is `<ns>.<instance>.<fn>` with no field segment | `_program.py:80–104`; `examples/adcs-fsw2/tests/python_system.rs:29` | `Component(gyro_norm.out)` with no field renders `fsw.gyro_norm.gyro_norm` |
| `Coordinator::registry().view(id)` keys ring entries by `<ns>.<instance>.<frame>`; the alarm resolver walks each entry's vtable with `for_each_field` and matches leaf `component_id`s | `core/src/registry.rs:88,125,138`; `src/alarm/mod.rs:614–648`; `libs/metor-proto/src/vtable.rs:595` | the example's path audit (WP5) uses the same two calls |
| The pack module codegen emits `    name: OutPort[Marker]  # notes` per port and pins `python/tests/data/demo.py` byte-for-byte; the fixture is regenerated by the ignored `fixture_dump` test; `.metor/adcs_pack` is untracked and regenerated by `ensure_stubs`/`uv sync` | `src/wiring/pack_module.rs:188–229`; `src/wiring/pack_module/tests.rs:233–256`; `git ls-files` | `_frames` costs one codegen line, one fixture regeneration, no hand edits |
| `_Subscribe` copies `ty`, `artifact`, `artifact_decl` from `of.spec` and keeps `of`; the mirror's handle is `SystemHandle(full, fsw, _Subscribe)` | `_builtins.py:150–172`; `_target.py:248` | a mirror port renders under the subscriber's namespace (`fsw.plant.sensors`), which is what exists there |
| `PresetSystem` publishes once on the first execute, has no `configure`, and treats component ids as opaque | `src/preset/mod.rs:1–13,73–90` | nothing in Rust changes for presets |
| `Record` registers `Announce::Msg` metadata and pushes each message record with `db.push_msg`; the ground mirror of a gateway carries every non-command message log, latest record first, then live | `src/gateway/record.rs:94,122,169–170`; `docs/telemetry.md:258–262` | `PresetDefs` published on `gw` reaches a panel that connects at any time |
| The panel's `TargetPresetStore`, `AlarmStore`, and `WiringStore` are app-global over the one session db; the preset fold replaces the whole set per `PresetDefs` record; `apply_shipped_preset` applies `presets.first()` when no active connection has a saved layout and no window holds items; `AlarmDefs` folds per def id (`apply_def` inserts by `def.id`, never clears) | `libs/metor-panel/src/app.rs:1500–1505`; `src/presets.rs:139–183`; `src/alarms/mod.rs:142,567–571` | presets from one publisher need no panel change; onboard and fleet alarms coexist iff ids are distinct across members |
| A trace's `component_id` is the masked FNV-1a-64 of the qualified name, pinned against `ComponentId::new`; the panel resolves it against the session db whatever connection announced the component | `_dashboard.py:14–20`; `src/preset/tests.rs:40–49`; `libs/metor-panel/src/hydration.rs:28` | no panel assumption ties a preset to the connection that carries its components |
| The gateway design's fold-collision note names `AlarmDefs`, `SequenceRegistry`, `PresetDefs`, `WiringManifest` as latest-wins per message id | `docs/design-deployment-gateway.md:68–71,367–372,647–651` | with one `Presets` publisher the preset collision is moot; alarms fold by id, not by set |
| `Deployment.__init__` resolves `_subscribes`, `_ingests`, then `_check_command_overlap` per target | `_deployment.py:32–52,224–241` | `_check_references` sits beside them |
| `metor-config` is `0.4.3`; `IR_VERSION = 11` in both languages; the goldens carry `"ir_version": 11` eight times across three files | `_version.py`, `pyproject.toml:9`, `src/ir.rs:18`; `tests/golden/{target,deployment,deployment_gateway}.json` | `0.4.4`; `12` (WP3) |
| `test_emits_the_golden_gateway_deployment` and `golden_gateway_round_trips` pin `tests/golden/deployment_gateway.json`; `dashboard.json` and `outline.json` are pinned by calling `_state("sat1")` directly, with no `Target` | `python/tests/test_golden.py:368–384`; `tests/ir_contract.rs:479` | the gateway golden gains a gw `Presets`; the two pane goldens stay byte-identical |
| `tests/gateway.rs` runs five cases in one `#[test]`; `mirror_and_command` is case 1, `sources_live` re-runs it for the bundles; `log_sources` shows how to read a mirrored message log (`get_or_insert_msg_log`, `get_range`, `node.msgs()`) | `tests/gateway.rs:187–206,273,323,381` | a `preset_ids` helper copies `log_sources` over `PresetDefs::ID` |
| The example's dashboards (lines 143–338) and presets (340–395) precede the `mode` slot (399–420) and the `gw` member (445–452); `mode` is referenced only by string today | `examples/adcs-fsw2/target.py` | the block moves to the end of the file, after `gw`, so every handle exists |
| `build_sim_coordinator` and the sequence and Python-system suites select `fsw`; `bundle.rs` evaluates `fsw` and `gw`, both with `process = false` | `examples/adcs-fsw2/src/lib.rs:50`; `tests/bundle.rs:69,134,170` | moving presets off `fsw` touches none of their assertions |
| The gateway forwards every token of the ingested member's `Uplink` by default (`commands=None`); the example's `gw.add("fsw", Ingest(db, fsw_link))` forwards `SequenceCommand`, `AlarmAck`, `ReloadSequences` | `_deployment.py:91–136`; `examples/adcs-fsw2/target.py:397,450` | `SequenceControl("mode")` works from a panel on `gw`; the channel is not a component path and stays a string |

---

## WP1: what a handle carries, and how a path renders

Files:

- `python/metor-config/metor_config/_model.py`:

  ```python
  class PortRef:
      def __init__(self, instance: str, port: str, target: Any = None, frame: str | None = None)
  class SystemHandle:
      def __getattr__(self, name: str) -> Any   # PortRef(self.name, name, self.target, self._frame(name))
      def _frame(self, port: str) -> str        # (self.spec's `_frames` or {}).get(port, port)

  @dataclass(frozen=True)
  class Component:
      """A component: a path string, an instance handle, or a frame port, plus an optional
      field path inside the frame and an optional element index."""
      ref: "str | PortRef | SystemHandle"
      field: str | None = None
      element: int | None = None
      def path(self, namespace: str | None) -> str

  Ref = "str | PortRef | SystemHandle | Component"
  def _qualify(name: str, namespace: str | None) -> str          # moved from _dashboard.py
  def _ref_path(ref: Any, namespace: str | None) -> str
  def _referenced_targets(obj: Any) -> list[Any]
  ```

  `_ref_path`: a `str` qualifies with `namespace` (today's rule); a
  `SystemHandle` renders `<its target's namespace>.<name>`; a `PortRef`
  appends `.<frame>`; a `Component` renders its `ref` the same way and
  appends `.<field>` when set. A handle whose `target` is `None` raises
  `ValueError("… is not registered on a Target")`, the one gate. A handle
  whose target has no namespace renders bare, as `_qualify(name, None)` does.
  `_referenced_targets` walks dataclass fields, lists, and tuples, collecting
  `handle.target` from every `PortRef`/`SystemHandle`/`Component` it meets;
  it is what WP2 and WP3 check membership with. `Component` moves here from
  `_builtins.py` (with `_dashboard.py` importing it, `_builtins` importing
  `_dashboard`, it cannot stay there); `_builtins.py` and `__init__.py`
  import it from `_model`.
- `_program.py`: `ExprHandle.out` returns
  `PortRef(self.name, self._out, self.target, self._out)`.
- `_target.py`: `self.coordinator = SystemHandle(COORDINATOR, self)`;
  `slot` returns `SystemHandle(full, self)`; `_add_expr` sets
  `handle.target = self` beside `handle.name = full`.
- `_builtins.py`: `_Subscribe.__init__` copies
  `self._frames = getattr(spec, "_frames", {})` so a mirror's ports render
  the peer type's frame names; `Component` is re-exported from `_model`.
- `src/wiring/pack_module.rs`: after `port_annotations`, emit one line per
  `System` subclass, `    _frames = {"cmd": "cmd", "sensors": "sensors", "system_status": "system_status"}`,
  over the entry's Table ports (inputs and outputs, then the host status
  port) mapping port name to `frame_name(metadata).unwrap_or(port.name)`;
  Postcard ports are omitted. Signature: `fn frames_line(desc: &SystemDescriptor) -> String`.
- `python/tests/data/demo.py`: regenerated with
  `cargo test -p metor-fsw-2 fixture_dump -- --ignored`, not hand-edited.
- `src/wiring/pack_module/tests.rs`: `render_is_deterministic_and_structured`
  asserts the `_frames` line for `Widget`; `demo_fixture_matches_checked_in`
  passes once the fixture is regenerated.
- `python/metor-config/metor_config/__init__.py`: `Component` from `_model`;
  no new names.

Tests to add (`python/tests/test_recorder.py`, a `ReferenceTest` class):

- a pack entry's port renders `<ns>.<instance>.<frame>` under its own
  target's namespace, and `Component(port, "gyro_b")` appends the field;
  `Component("a.b")` on `namespace="sat1"` renders `sat1.a.b`;
  `Component(handle)` for an instance renders `sat1.<instance>`.
- a `System` subclass with `_frames = {"est": "attitude_estimate"}` renders
  `sat1.nav.attitude_estimate` for `nav.est`; a bare `System("Nav", art)`
  falls back to the port name.
- the coordinator, a slot, and an added `@system` all carry `target`;
  `Component(gyro_norm.out)` renders `fsw.gyro_norm.gyro_norm`; a `@system`
  handle that was never added raises the not-registered error.
- a mirror handle (`fsw.add("plant", Subscribe(sim))`) renders
  `fsw.plant.sensors`, the frame name taken from the peer type's `_frames`.
- `test_pack_module.py`: `demo.Widget._frames == {"cmd": "cmd", "sensors": "sensors", "system_status": "system_status"}`
  and `demo.Sink._frames` has no `events`.

What could go wrong: `SystemHandle.__getattr__` guards `_`-prefixed names
already (`_model.py:181`), so `self._frame` and `self.spec` resolve
normally. `_frames` on a `System` subclass is a class attribute pyright
infers as `dict[str, str]`; nothing in the recorder instantiates it. A
`Component` is a frozen dataclass holding a handle, which is unhashable and
fine (`eq=True` compares by handle identity through `SystemHandle`'s default
`__eq__`). The demo fixture's `system_status` port is appended by
`port_annotations`, so the map lists it too; keep the order inputs, outputs,
status so the fixture is deterministic.

Green when:

```sh
cargo test -p metor-fsw-2 --lib pack_module && (cd python && uv run python -m unittest discover tests) && cargo clippy -p metor-fsw-2 --all-targets
```

## WP2: presets, dashboards, and the outline take references

Files:

- `python/metor-config/metor_config/_dashboard.py`: import `Component`,
  `Ref`, `_qualify`, `_ref_path` from `_model` (the local `_qualify` is
  deleted). Field types: `Trace.component: Ref`, `Text.component: Ref`,
  `TrafficLight.component: Ref`, `TrafficLightGrid.pattern: Ref`,
  `Pivot.path: Ref`, `FrameType.rows: list[Ref] | None`,
  `Outline.root: Ref | None`, `Outline.expanded/collapsed: list[Ref] | None`,
  `Meter.component`, `Gauge.component`, `StateChip.component`,
  `VectorMarker.component`, `Attitude.component`, `Map.component`,
  `Bind.component: Ref`. Every `_qualify(x, namespace)` at the sites in
  "Verified facts" becomes `_ref_path(x, namespace)`; `_qualify_all` maps
  `_ref_path`. `Trace._state`'s label default is `t.label or t.component`
  today; it becomes `t.label or _ref_path(t.component, None) if not a str else t.component`,
  i.e. a string keeps its bare label and a handle labels with its rendered
  path. For the widgets that carry their own `element` (`Trace`, `Meter`,
  `Gauge`, `StateChip`, `Bind`), a `Component` whose `element` is set is
  rejected: `ValueError("<widget>: element is given on the Component and on the widget; name it once")`,
  raised inside `_ref_path`'s caller through one helper
  `_component_path(ref, namespace, owner: str) -> str`.
- `_builtins.py`, `_Presets`: `__init__` records
  `self.targets = _referenced_targets(presets)`; `_bind` unchanged.
- `_target.py`: `self._presets: dict[str, _Presets] = {}`; `add` records a
  `_Presets` there beside `_subscribes`/`_ingests`.
- `_deployment.py`: `_check_references(target)` after
  `_check_command_overlap`: every target a `_Presets` references is in
  `self.targets`, else
  `ValueError("`<instance>`: a preset names `<ns>.<name>`, which belongs to a target outside this deployment; list every member in `Deployment(targets=…)`")`;
  at most one member adds `Presets`, else
  `ValueError("`<a>` and `<b>` both add Presets; one member publishes the deployment's presets (the gateway, when there is one)")`.
  Both are deployment-scope facts, which is why they live here and not at
  `add`.
- `python/tests/data/deployment.py`: `fsw`'s `Presets` move to `gw`, with
  `Trace(Component(widget.sensors, "value"))` (the demo fixture's `Sensors`
  has `sensors.value`), `Text(fsw_sink.system_status)`, and an
  `Outline(root=widget, expanded=[widget.sensors])`, so the pyright gate
  types `Ref` on a generated class, a mirror, and a handle from another
  member.
- `python/tests/test_golden.py`, `build_gateway_deployment`: `gw` adds
  `Presets([Preset(name="fleet", layout=TimeSeriesPlot([Trace(Component(sim.sensors, "gyro_b"), element=1)]))])`
  where `sim` is the `plant` member's `System("Plant", …)` handle (a bare
  `System`, so the frame name is the port name). Regenerate
  `tests/golden/deployment_gateway.json`.
- `tests/ir_contract.rs`, `golden_gateway_round_trips`: the `gw` member's
  `Presets` params contain `ComponentId::new("plant.plant.sensors.gyro_b").0`,
  the pattern of `golden_deployment_round_trips` (`ir_contract.rs:452–460`).

Tests to add (`python/tests/test_recorder.py`):

- `test_a_handle_trace_renders_the_other_members_namespace`: `gw` presets
  with `Trace(Component(plant_sim.sensors, "gyro_b"))` carry
  `component_id("plant.plant.sensors.gyro_b")`; a bare `Trace("a.b")` in the
  same preset carries `component_id("gw.a.b")`.
- `test_every_widget_takes_a_reference`: one `Dashboard` with `Meter`,
  `Gauge`, `StateChip`, `Text`, `TrafficLight`, `TrafficLightGrid(Component(port, "wheels.*.arm"))`,
  `Attitude` with a `VectorMarker(port2)`, `Map`, and a `Connector` with
  `Bind(Component(port, "wheels.1.arm"))`, all over handles of a second
  member; assert each rendered `component`/`pattern` string.
- `test_outline_paths_take_references`: `Outline(root=handle, expanded=[handle.wheels], pivots=[Pivot(Component(handle.wheels, "wheels"))], types=[FrameType("x", ["a"], rows=[handle])])`.
- `test_a_component_element_and_a_widget_element_conflict`.
- `test_a_preset_naming_a_non_member_is_rejected` and
  `test_two_members_may_not_both_add_presets`.
- `test_presets_qualify_and_embed` and
  `test_presets_qualify_with_the_target_that_adds_them` stay as they are: the
  bare-string rule is unchanged.

What could go wrong: `Outline.root` is `str | None` today and the `_state`
guard is `if self.root else None`; a `SystemHandle` is truthy, so the guard
holds. `TrafficLightGrid.pattern` with a `*` renders through `_ref_path`
untouched (the field path is opaque text). The golden dashboard and outline
fixtures are rendered with `_state("sat1")` and string references, so they
do not change; do not regenerate them. `deployment.json` (the two-member
golden) keeps its `fsw` presets: with no gateway there, one publisher is
`fsw`.

Green when:

```sh
(cd python && uv run python -m unittest discover tests) && cargo test -p metor-fsw-2 --test ir_contract --test py_eval
```

## WP3: alarms take references; the Rust prefix goes; IR 12

Files:

- `python/metor-config/metor_config/_builtins.py`:

  ```python
  class Alarm:                      # unchanged fields; target: Component
      def to_json(self, namespace: str | None) -> dict[str, Any]   # target rendered through Component.path
  class _Alarms(Spec):
      def __init__(self, alarms: list[Alarm])                       # records self.alarms, self.targets = _referenced_targets(alarms)
      def _bind(self, target: Any) -> None                          # params = {"alarm": [a.to_json(target.namespace) …]}
  def Alarms(alarms: list[Alarm]) -> Spec
  ```

  `Component.to_json` is gone; `Alarm.to_json` writes
  `{"component": self.target.path(namespace), "element": self.target.element}`
  through `_drop_none`. A bare-string target on a namespaced member renders
  `<ns>.<path>`, byte-identical to what `AlarmSystem::configure` produced.
- `_target.py`: `self._alarms: dict[str, _Alarms] = {}`; `add` records it.
- `_deployment.py`, `_check_references(target)`, the alarm half: every
  target an `_Alarms` references is a member; it is the adding target, or a
  target the adding member ingests (`any(i.source.target is that for i in target._ingests.values())`),
  else `ValueError("`<instance>`: alarm `<id>` targets `<ns>.<path>` on member `<ns>`; an alarm reads its own member's rings, so mirror the instance with `Subscribe` or place the alarm on the gateway")`.
  Alarm ids are distinct across the deployment, else
  `ValueError("alarm `<id>` is declared by `<a>` and `<b>`; the panel keys alarms by id across every member it sees")`.
- `src/alarm/mod.rs`: delete the `configure` impl (the `BuildSystem`
  default is `Ok(())`); `TargetSpec` doc: "the fully qualified component id
  text (`sat1.plant.sensors.gyro_b`); the recorder qualifies it, the engine
  never rewrites it". Module doc line on namespaces likewise.
- `src/alarm/tests.rs`, `namespace_prefixes_alarm_targets` ->
  `qualified_targets_resolve_under_a_namespace`: author
  `sat1.plant.gyro.rates.1`, drop the `configure` call, keep every
  assertion.
- `src/ir.rs`: `IR_VERSION = 12`; `_version.py`: `IR_VERSION = 12`,
  `__version__ = "0.4.4"`; `pyproject.toml:9`: `0.4.4`.
- `tests/golden/target.json`, `deployment.json`, `deployment_gateway.json`:
  `"ir_version": 12` (eight sites; regenerate rather than edit, the Python
  golden builders emit it).
- `docs/alarms.md`, "Component references": the namespace is added by the
  recorder when the target is a bare string; a `Component(handle.port, "field", element=)`
  form names any member's component and renders its namespace; the engine
  resolves what it is given. "Build checks": the record-time rules above.
- `docs/wiring.md`, "Deployments": the sentence at line 111 becomes
  "Component references in `Presets` and `Alarms` render fully qualified at
  record time: a bare string takes the namespace of the target that adds it,
  a port handle the namespace of the target it belongs to."

Tests to add:

- `python/tests/test_recorder.py`: `test_alarms_emit_the_rust_field_names`
  binds to a bare `Target` first (`spec._bind(Target(cycle_rate=1.0))`) and
  keeps its assertions; new: a bare string on `namespace="sat1"` renders
  `sat1.plant.gyro`; `Component(sim.sensors, "gyro_b", element=1)` on the
  mirror handle renders `fsw.plant.sensors.gyro_b`; a `gw` alarm on
  `plant_sim.sensors` is accepted when `gw` ingests `plant` and rejected on
  a member that does not; duplicate ids across members are rejected.
- `tests/ir_contract.rs`: `golden_fixture_round_trips` already reads the
  un-namespaced `target.json`; add an assertion in
  `golden_gateway_round_trips` if WP4 puts an alarm on the fixture (it does
  not; see WP4), otherwise none.
- `src/wiring/validate.rs` tests at `:842` use `IR_VERSION - 1`; they need
  no edit.

What could go wrong: any `.py` fixture under `tests/fixtures/` that adds
`static_system("Alarms")` without params is unaffected. The panel imports
`IR_VERSION` from this crate (`libs/metor-panel/src/wiring/mod.rs:22`), so
`cargo check -p metor-panel` proves the bump compiles there; a panel built
before the bump rejects the new `WiringManifest` until rebuilt, which is
what the version is for. Existing bundles must be re-packaged. `Alarm` is a
frozen dataclass; `to_json` gaining a parameter is not a field change.

Green when:

```sh
cargo test -p metor-fsw-2 --lib alarm && cargo test -p metor-fsw-2 && cargo clippy -p metor-fsw-2 --all-targets && cargo check -p metor-panel && (cd python && uv run python -m unittest discover tests)
```

## WP4: fixture, integration, goldens, docs

Files:

- `tests/fixtures/gateway_target.py`: `gw` adds
  `Presets([Preset(name="fleet", layout=TimeSeriesPlot([Trace(Component(a.coordinator.system_status, "cycles"), label="a")]))])`
  after `Record(db)`. The coordinator handle carries `a` after WP1, the
  status port's frame name is `system_status`, and `cycles` is a field
  (`core/src/status.rs:38`).
- `tests/gateway.rs`: `fn preset_ids(db: &Arc<DB>) -> Vec<u64>`, the
  `log_sources` shape over `PresetDefs::ID`, decoding each record, parsing
  every preset's `layout` JSON, and collecting `traces[].component_id` from
  each pane item's `state`. `mirror_and_command` (case 1) also waits until
  `preset_ids(&db)` contains `ComponentId::new("a.coordinator.system_status.cycles").0`;
  `sources_live` (case 5) inherits it. No new case: the gateway alone (case
  4) publishes the preset too, and nothing there needs it.
- `docs/telemetry.md`, "Gateway": a paragraph after the command paragraph:
  presets live on the gateway and name any member's components by handle;
  `Record` stores the `PresetDefs` snapshot with the gateway's other
  messages, and the ground mirror carries it like any message log. One
  member publishes presets; the recorder enforces it.
- `docs/wiring.md`, "Deployments": the example gains
  `gw.add("presets", Presets([...]))` with one handle trace, and the
  sentence from WP3.
- `docs/README.md`, "Built-in services": the gateway line adds "and serves
  the deployment's presets".
- `libs/metor-panel/src/connections/mod.rs:5`: no code change; the module
  doc's "assumed largely disjoint" sentence may add "presets come from one
  member" if the reviewer wants the assumption recorded, per gateway
  decision 3. Optional; the panel is otherwise untouched by this plan.
- `docs/design-deployment-revision.md`, change 4: the corrections from
  "Deviations" 1 and 3.

Tests to add: the `preset_ids` wait above; nothing else. The pyright fixture
and goldens landed in WP2/WP3.

What could go wrong: `PresetDefs` is a snapshot the gateway publishes once
at its first execute; `Record` pushes it once. The mirror's message sync
backfills history before live, so the wait sees it whenever the test
connects. If `get_or_insert_msg_log` for `PresetDefs::ID` returns an empty
log because the mirror has not yet synced that id, the `wait_for` loop
covers it, the same as `log_sources`. The trace's `state` is a JSON string
inside the layout JSON string (two levels), as `test_presets_qualify_and_embed`
decodes it.

Green when:

```sh
cargo test -p metor-fsw-2 --test gateway && cargo test -p metor-fsw-2 && cargo clippy -p metor-fsw-2 --all-targets
```

## WP5: the adcs example

Files:

- `examples/adcs-fsw2/target.py`:
  - The dashboard constants and `Place`s (lines 143–338) and the `presets`
    block (340–395) move to the end of the file, after `gw` is declared and
    before `Deployment(...)`; `presets = gw.add("presets", Presets([...]))`.
    The `mode` slot, `nav`, `ctrl`, `plant_sim`, and the `fsw` mirror `sim`
    all exist there. The comment at lines 91–93 ("The mirror is named
    `plant`, so every component path …") is replaced: the mirror is named
    `plant` so `fsw`'s alarms and edges read `plant.*` as the onboard view;
    the dashboards on `gw` name the source member's components directly.
  - Every string reference becomes a handle. The source member is chosen
    over the `fsw` mirror where both exist, as the design states; the
    rendered path and the member that announces it:

    | Today | Becomes | Renders | Announced by |
    | --- | --- | --- | --- |
    | `StateChip("mode.mode_cmd", element=0)` | `StateChip(Component(mode.mode_cmd, "mode"), …)` | `fsw.mode.mode_cmd.mode` | fsw (deviation 6) |
    | `StateChip("mode.mode_cmd", element=1)` | `StateChip(Component(mode.mode_cmd, "law"), …)` | `fsw.mode.mode_cmd.law` | fsw (deviation 6) |
    | `StateChip("plant.world.illuminated")` | `Component(plant_sim.world, "illuminated")` | `plant.plant.world.illuminated` | plant |
    | `TrafficLightGrid("plant.wheels.wheels.*.arm")` | `Component(plant_sim.wheels, "wheels.*.arm")` | `plant.plant.wheels.wheels.*.arm` | plant |
    | `Attitude("nav.attitude_estimate.q_hat_b_eci")` | `Component(nav.attitude_estimate, "q_hat_b_eci")` | `fsw.nav.attitude_estimate.q_hat_b_eci` | fsw |
    | `VectorMarker("plant.sensors.mag_b")` | `Component(plant_sim.sensors, "mag_b")` | `plant.plant.sensors.mag_b` | plant |
    | `Gauge("plant.sensors.gyro_b", element=i)` ×3 | `Component(plant_sim.sensors, "gyro_b")` | `plant.plant.sensors.gyro_b` | plant |
    | `Meter(f"plant.wheels.wheels.{w}.ang_momentum", element=w)` ×3 | `Component(plant_sim.wheels, f"wheels.{w}.ang_momentum")` | `plant.plant.wheels.wheels.{w}.ang_momentum` | plant |
    | `SequenceControl("mode")` | unchanged | (a channel, not a component) | — |
    | `Meter("ctrl.mtq_cmd.dipole_b", element=i)` ×3 | `Component(ctrl.mtq_cmd, "dipole_b")` | `fsw.ctrl.mtq_cmd.dipole_b` | fsw |
    | `Trace("plant.sensors.gyro_b", element=i)` ×6 (both plots) | `Component(plant_sim.sensors, "gyro_b")` | `plant.plant.sensors.gyro_b` | plant |
    | `Trace(f"plant.wheels.wheels.{w}.ang_momentum")` ×6 | `Component(plant_sim.wheels, f"wheels.{w}.ang_momentum")` | `plant.plant.wheels.wheels.{w}.ang_momentum` | plant |
    | `Bind("plant.wheels.wheels.1.arm")` | `Component(plant_sim.wheels, "wheels.1.arm")` | `plant.plant.wheels.wheels.1.arm` | plant |
    | `Trace("nav.attitude_estimate.omega_b", element=1)` | `Component(nav.attitude_estimate, "omega_b")` | `fsw.nav.attitude_estimate.omega_b` | fsw |
    | `Map("plant.gps.lla")` | `Component(plant_sim.gps, "lla")` | `plant.plant.gps.lla` | plant |
    | alarm `Component("plant.sensors.gyro_b", element=1)` (stays on fsw) | `Component(sim.sensors, "gyro_b", element=1)` | `fsw.plant.sensors.gyro_b` | fsw (the mirror; same as today) |
    | alarm `Component("plant.wheels.wheels.0.ang_momentum", element=0)` | `Component(sim.wheels, "wheels.0.ang_momentum", element=0)` | `fsw.plant.wheels.wheels.0.ang_momentum` | fsw (the mirror; same as today) |

    `@system("plant.sensors.gyro_b")` on `gyro_norm` is a compiler bind
    string the host resolves against `fsw`'s registry
    (`docs/python-systems.md:62`); it is not a builder reference and stays.
  - `adcs-ops` gains one tab beside `AlarmList()` and `SequenceList()`:
    `Outline(root=plant_sim, expanded=[plant_sim.wheels], pivots=[Pivot(Component(plant_sim.wheels, "wheels"), fields=["ang_momentum", "speed", "arm"])])`
    (deviation 5; strike if unwanted).
  - Imports: `Outline`, `Pivot`.
- `examples/adcs-fsw2/README.md:139–146`: "the preset this target ships"
  becomes "the preset the `gw` member ships; a panel on `2241` alone sees no
  preset"; the alarms sentence (169–170) is unchanged.
- `examples/adcs-fsw2/tests/presets.rs` (new): the path audit the task
  asks for, so a renamed frame field fails a test rather than an operator.
  Evaluate `target.py` (`eval_python_deployment`), build `plant` and `fsw`
  in-process as `bundle.rs::eval_and_build` does (`process = false`,
  `provision_artifacts`, `resolve`), and collect every leaf `component_id`
  each coordinator's `registry().entries()` announces (`entry.announce()`
  then `vtable.for_each_field(None, ..)` over `rf.component_id`, the alarm
  resolver's walk). Then walk the `gw` member's `Presets` params: every
  `component_id` in a trace and every `component` string in a widget config
  (hash it) must be in the union; a `pattern` with `*` is checked by
  expanding `*` over `0..3` for this example, or skipped with a comment.
  Skips like the other suites when Python or the build is unavailable.

Tests: `presets.rs` above; `cargo test -p adcs-fsw2` (bundle, sequences,
python-system) unchanged in assertions.

What could go wrong: the audit builds two members in one test binary, each
binding its `TcpServer`; `common::ensure_stubs` and the fixed-port note in
`tests/common/mod.rs:13` apply, so run it after the others or bind nothing
by resolving without running (resolve binds the listener at state
construction, plan gateway-2 deviation 2; if that clashes with a parallel
suite, mark the test `#[serial]`-style by sharing the existing lock). The
`fsw` mirror `fsw.plant.*` and the source `plant.plant.*` both exist in
`gw`'s db; the dashboards use the source so a plant-only panel connection
on `2240` still shows most widgets. Once the block moves below `uplink`, the
`presets` name no longer precedes `uplink` in `fsw`'s step order; nothing
depended on that.

Green when:

```sh
cargo test -p adcs-fsw2 && cargo test -p metor-fsw-2 --test gateway
```
