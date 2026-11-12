# Composites: combining systems

A composite is a function that adds several systems to a target as one
reusable piece. It exists only in the target's Python file. The config lists
its systems as ordinary entries with dotted ids, so the host is unaware of it.

## Python API

```python
from typing import NamedTuple
from metor_config import OutPort, Scope, Sources, composite

class Gnc(NamedTuple):
    nav: Nav
    torque: OutPort[TorqueCmd]

@composite
def gnc(s: Scope, *, sensors: Sources[Sensors], gain: float = 0.1) -> Gnc:
    nav = s.add("nav", Nav(sensors=sensors, gain=gain))
    ctrl = s.add("ctrl", Ctrl(estimate=nav.attitude_estimate))
    return Gnc(nav, ctrl.torque_cmd)

g = fsw.add("gnc", gnc(sensors=imu, gain=0.2))  # systems gnc.nav, gnc.ctrl
g.torque
g.nav.status
fsw.add("pub", Publish(g, transport=Listen("[::]:2240")))
```

## Inputs and params

A composite takes ordinary Python parameters. `Sources[T]` marks an input, and
any other type is a param. `@composite` removes `s` from the signature a type
checker sees and keeps the rest, so each call is checked. At runtime, arguments
that do not fit raise `TypeError` at the call.

An input may be optional, `Sources[T] | None`. The body decides what `None`
means, such as adding a model only when given truth. Passing `None` on to a
system input leaves that input unwired.

Calling `gnc(..)` stores the arguments. The body runs when the result is
added, because a port needs its system's final name and target when it is
created. A `Composite` added twice runs its body twice.

Its systems take their place in step order where the composite is added, so an
input comes from an earlier system or a `Loop`.

## Result

`add` returns whatever the function returns. Use a `NamedTuple` of handles and
ports:

- Return a handle to expose all of a system's ports, or a single port under a
  name of your choice. A composite cannot return one flat set of ports, since
  every system has a `status` port and the names would clash.
- A `NamedTuple` of handles and ports is a port list, so `Publish(g, ..)` reads
  each port it holds. A port listed twice is read once. A result holding other
  results is not a port list; pass `[*g.a, *g.b]`. A type checker does not catch
  this, and `Publish` raises `ConfigError`.
- Annotate the return type: mypy treats an unannotated function's result as
  `Any`.

## Scope

| Method | Name in the config |
|---|---|
| `s.add("nav", ..)` | system `gnc.nav` |
| `s.add("inner", composite(..))` | prefix `gnc.inner` |
| `s.sim("model", ..)` | system `gnc.model` |
| `s.add(.., channel="seq")` | channel `gnc.seq` |
| `s.add(.., thread="io")` | thread `io`, shared between instances |
| `s.resource("uart", ..)` | resource `gnc_uart` |
| `s.add("pub", Publish(..))` | link `gnc.pub`, resource `gnc_pub_ep` |
| `s.loop(Record)` | unnamed |

`Target.commands` and `Target.wasm` apply to a whole target, so a target file
calls them outside the composite.

## Limits

- `fsw.add("gnc", composite)` takes no `thread`, `channel`, or `start`. The
  body sets them on its systems.
- A composite's systems share one target.
- One system's id cannot sit under another's: `a` and `a.b` cannot both be
  systems, since `a`'s port `b` and `a.b`'s ports would share component paths.
  Nothing can be added under a composite's name once it has been added.
- A channel name holds at most 48 bytes, prefix included.
- An alarm id, `{ns}.{system}.{name}`, holds at most 96 bytes, so deep nesting
  can fail with `AlarmPrefixTooLong`.
