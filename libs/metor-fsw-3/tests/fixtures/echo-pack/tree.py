"""The fixture's tree of targets: `flight.s1` holds `cpu` and `gw`, and
`ground` stands alone. No target dials another, so any subset runs."""

from echo_pack import Sampler
from metor_config import Deployment, Group, group


@group
def string(g: Group) -> None:
    g.target("cpu", cycle_rate=1000.0).add("sampler", Sampler())
    g.target("gw", cycle_rate=1000.0).add("sampler", Sampler())


@group
def flight(g: Group) -> None:
    g.add("s1", string())


@group
def ground(g: Group) -> None:
    g.target(cycle_rate=1000.0).add("sampler", Sampler())


d = Deployment()
d.add("flight", flight())
d.add("ground", ground())
