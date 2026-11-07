# Target groups: deployments as a tree of targets

A group is a function that adds several targets to a deployment as one
reusable piece. Groups nest, so a deployment is a tree, such as:

```
flight
  cpus      cpu1 cpu2 cpu3
  gateways  gw1 gw2 gw3
ground
  gse
  gateways  gw1 gw2 gw3
  rf        st1 st2
```

A group exists only in the Python file. The config lists its targets as
ordinary entries with dotted namespaces, such as `flight.cpus.cpu1`, so the
host is unaware of it. `metor run` starts any subtree, or a set of them, from
the same config.

Groups sit one level above composites:

| Level | Runs as | Crossing it costs |
|---|---|---|
| group | several processes, maybe several hosts | a link |
| target | one coordinator | nothing; ports are read in step order |
| composite | systems in one target | nothing |
| system | one step | |

## Python API

```python
from typing import NamedTuple
from metor_config import Deployment, Group, OutPort, Publish, Sources, group

class Cpu(NamedTuple):
    link: Publish
    est: OutPort[Est]

@group
def cpu(g: Group, *, uplink: Sources[Cmd]) -> Cpu:
    t = g.target(cycle_rate=120.0)          # namespace: the group's path
    gnc = t.add("gnc", gnc_composite(cmd=uplink))
    link = t.add("pub", Publish([gnc], transport=g.listen()))
    return Cpu(link, gnc.est)

@group
def flight(g: Group) -> Flight:
    uplink = g.loop(Cmd)
    cpus = [g.add(f"cpu{i}", cpu(uplink=uplink)) for i in (1, 2, 3)]
    gws = g.add("gateways", gateways(est=[c.est for c in cpus]))
    uplink.connect(gws.cmd)                   # a port on another target
    return Flight(cpus, gws)

d = Deployment(base_port=2240, hosts={"flight": "10.0.0.1", "ground.gse": "10.0.1.1"})
fl = d.add("flight", flight())
d.add("ground", ground(flight=fl))
```

`@group` works as `@composite` does: it hides `g` from the signature a type
checker sees, checks the arguments at the call, and runs the body when the
result is added. A group added twice runs twice. `add` returns what the
function returns; use a `NamedTuple`, as for a composite.

### Group

| Method | Result |
|---|---|
| `g.target(**kw)` | a `Target` named by the group's path, as `flight.cpus.cpu1` |
| `g.target("io", **kw)` | a `Target` named `{path}.io` |
| `g.add("cpus", group(..))` | the group's result; its targets go under `{path}.cpus` |
| `g.loop(Record)` | a `Loop`, which may connect to a port on another target |
| `g.listen(host="[::]")` | a `Listen` on the deployment's next free port |
| `g.path` | the group's dotted path |

`g.target` takes `Target`'s arguments except `namespace`.

### Deployment

`Deployment` gains `add`, `listen`, and `base_port`. `targets=` still takes
targets built outside any group, and both forms can be mixed.

- `d.add(name, group(..))` adds a group at the top of the tree.
- `base_port` is the first port `listen` hands out. Each call takes the next
  one, in the order the file makes them. The whole tree is always evaluated, so
  a link's port does not depend on which targets run. `listen` raises
  `ConfigError` without `base_port`. A hand-written `Listen` still works, and
  the build still refuses two links on one host and port.
- `hosts` keys may name a group. The longest matching path wins, so
  `{"flight": a, "flight.cpus.cpu3": b}` puts `cpu3` on `b` and the rest of
  `flight` on `a`. The emitted config lists one host per target, as now. A key
  that matches no target raises `ConfigError`.

### Names

- Group and target names are identifiers. A namespace is the dotted path to
  its target.
- Targets are leaves: no namespace may sit under another, so a group whose
  path is a target cannot hold other targets or groups. Otherwise
  `flight.cpus.cpu1` + system `io.x` and `flight.cpus.cpu1.io` + system `x`
  would name the same ports.

## Links between targets

### Loops

A `Loop` connected to a port on another target becomes a link when the
config is emitted. For each target that reads the loop:

1. The port's target must have a `Publish` that carries the port. The first
   one in step order is used. If there is none, `ConfigError` names the port
   and its target.
2. The reading target gets one `Subscribe` for each link it reads through,
   added after its other systems, with the `Publish`'s `conn_cap`.
3. The loop reads that subscriber's copy of the port.

One loop can feed systems on several targets; each target gets its own
subscriber. This is how one uplink reaches all three CPUs of a triple string.
Passing another target's port as an input, without a loop, stays a
`ConfigError`.

A subscriber keeps one port per message record, so two loops that read
different ports of one message record through one link raise `ConfigError`.

A loop on one target reads the previous cycle's value. A loop across targets
reads the newest sample the subscriber has received, which may be several
cycles old, or none if the producer is not running.

The `adcs-fsw3` plant already reads the controller this way, by hand: a loop
connected to a `Subscribe` on its own target.

### Explicit links

A loop feeds only a system's inputs. It has no `field()`, cannot go in a port
list, and is not a port class such as `SensorsPort`. Use `Subscribe` and
`port()` for those, and on forward paths where an extra cycle of latency
matters, since a generated subscriber runs after the systems that read it.

### Link names

A link's id may be dotted, so links can sit inside composites, and `gateway`
can subscribe to a peer by its dotted namespace. The link's resource is its id
with `.` replaced by `_`, then `_ep`.

## Selecting targets

`metor run` evaluates the whole tree, then starts the selected targets.

```sh
metor run                                           # everything
metor run --target flight.cpus --target ground.gse  # two subtrees
metor run --target flight --except flight.cpus.cpu3
metor run deploy.json --target flight.cpus.cpu1     # one host, prebuilt config
```

- `--target P` selects each target whose namespace is `P` or starts with
  `P.`. It can repeat. `--except P` removes targets the same way. A pattern
  that matches nothing is an error.
- One selected target runs in this process; several run as children, as now.
- A link to a peer that was not selected keeps redialing and logging the
  failure, and the systems reading it get no samples.
- `metor show P` shows a group's targets; `metor show flight.cpus.cpu1.gnc`
  finds a system by the longest namespace that matches.
- `metor build` is unchanged. Evaluating the tree builds every target's packs,
  so a host that runs one target should run the prebuilt `.json`.

Replacing a target, such as running the CPUs against a simulated RF link,
is a function argument, such as `flight(rf=sim_rf)`, not a selection.

## What changes

| Part | Change |
|---|---|
| `metor_config` | `group`, `Group`, `Deployment.add`, `listen`, `base_port`, `hosts` prefixes, cross-target loops, dotted link ids, the nesting check |
| `metor` CLI | `--target` as a repeatable prefix, `--except`, `show` by longest namespace, completions |
| Config format, host, runtime | none |

## Not covered

- Lockstep simulated time across targets. Each target keeps its own clock.
- Redundancy logic. Voting and failover between strings are systems.
- Selecting by tag or wildcard, such as `*.gateways`. Two `--target` flags
  cover the example.

## Alternatives not taken

- **Nested `Deployment`s, or importing other target files.** A target file is
  a script with global state that emits at exit; a function composes and
  takes arguments.
- **Selecting in Python, from an env var.** The config, and so the ports,
  would change with the selection.
- **A separate `peer_loop`.** It would duplicate `Loop`, and `Peer` would have
  to resolve names later than it does.
- **Composites spanning targets.** A composite shares one cycle; hiding a link
  inside one would hide its latency.

## Open questions

1. What should a generated subscriber be called? Its name is in every copied
   port's path. This doc uses `from_{namespace}_{link}` with `.` as `_`, such as
   `from_flight_gateways_gw1_pub`, with no override.
2. How does a group hold a gateway? `gateway()` makes its own `Target`. It
   could take a `Group` or `Deployment` to add the target to.
3. Should `base_port` have a default, such as 2240?
