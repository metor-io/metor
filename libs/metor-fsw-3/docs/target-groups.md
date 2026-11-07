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
from metor_config import Deployment, Group, Loop, OutPort, Peer, Publish, Subscribe, Target, group

class Gateway(NamedTuple):
    target: Target
    link: Publish
    cmd: OutPort[Cmd]
    est: Loop[Est]

@group
def gateway_node(g: Group) -> Gateway:
    t = g.target(cycle_rate=50.0)          # namespace: the group's path
    est = g.loop(Est)
    radio = t.add("radio", Radio(est=est))
    link = t.add("pub", Publish([radio], transport=g.listen()))
    return Gateway(t, link, radio.cmd, est)

class Cpu(NamedTuple):
    link: Publish
    est: OutPort[Est]

@group
def cpu(g: Group, *, gateway: Publish, cmd: OutPort[Cmd]) -> Cpu:
    t = g.target(cycle_rate=120.0)
    up = t.add("uplink", Subscribe(transport=Peer(gateway), items=[cmd]))
    gnc = t.add("gnc", gnc_composite(cmd=up.port(cmd)))
    link = t.add("pub", Publish([gnc], transport=g.listen()))
    return Cpu(link, gnc.est)

@group
def flight(g: Group) -> Flight:
    gws = [g.add(f"gw{i}", gateway_node()) for i in (1, 2, 3)]
    cpus = [g.add(f"cpu{i}", cpu(gateway=gw.link, cmd=gw.cmd)) for i, gw in enumerate(gws, 1)]
    for gw, c in zip(gws, cpus):
        # Each gateway reads its CPU's estimate, which did not exist when the
        # gateway was built.
        down = gw.target.add("down", Subscribe(transport=Peer(c.link), items=[c.est]))
        gw.est.connect(down.port(c.est))
    return Flight(gws, cpus)

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
| `g.loop(Record)` | a `Loop`, as `Target.loop` |
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

A system reads only ports on its own target. A port on another target reaches
it through a `Subscribe` on the reading target, which dials a `Publish` on the
producing target. Groups pass `Publish` handles and ports as arguments, and
every subscriber is written in the file. Passing another target's port as an
input stays a `ConfigError`.

### Cycles

When two sibling groups read each other, the one built first cannot name the
other's link. It returns its `Target` and a `Loop` that its systems read. Once
the other group exists, the parent adds a `Subscribe` to that target and
connects the loop to the subscriber's port, as `flight` does above. The
`adcs-fsw3` plant reads the controller the same way.

The subscriber runs after the systems that read the loop, so they read the
newest sample it received by the previous cycle. Across a link that sample may
be several cycles old, or missing if the producer is not running.

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
| `metor_config` | `group`, `Group`, `Deployment.add`, `listen`, `base_port`, `hosts` prefixes, dotted link ids, the nesting check |
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
- **Subscribers generated from loops, or a `Peer` filled in later.** Either
  closes a cycle inside the child group, but the first hides a link, and the
  second needs `Peer` to resolve at emit time and gives no `port()`.
- **Composites spanning targets.** A composite shares one cycle; hiding a link
  inside one would hide its latency.

## Open questions

1. How does a group hold a gateway? `gateway()` makes its own `Target`, but it
   only adds a `Subscribe` per peer and a `Db`. It could become a composite
   that goes on any target, with `gateway()` kept as a wrapper. That needs
   dotted link ids, since each subscriber is named for its peer's namespace.
2. How do links get ports? Peers resolve at build time, so a dialed `Publish`
   needs a fixed port, and a group added three times cannot hard-code one.
   Either `g.listen()` counts up from `base_port`, as above, or the resolver
   gives each dialed `Listen` on port 0 the next free port on its host. The
   second drops `listen` and `base_port` from the Python API, and is a small
   change in `deploy`; a port is then visible only after resolving.
