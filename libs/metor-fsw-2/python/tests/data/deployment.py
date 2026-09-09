"""A two-member deployment, for the pyright gate: the typed surface a
multi-target file uses — `Deployment`, a generated pack entry, a `@system`,
and deferred-qualifying `Presets`."""

from demo import Widget

from metor_config import (
    Deployment,
    Downlink,
    Preset,
    Presets,
    Target,
    TcpServer,
    TimeSeriesPlot,
    Trace,
    f64,
    system,
)

plant = Target(cycle_rate=120.0, sim_dt=1 / 120, namespace="plant")
fsw = Target(cycle_rate=120.0, sim_dt=1 / 120, namespace="fsw")


@system("widget.sensors")
def sensor_norm(sensors) -> f64:
    return (sensors @ sensors) ** 0.5


plant_link = plant.state("link", TcpServer(addr="[::]:2240", name="plant"))
widget = plant.add("widget", Widget(count=3, gain=1.5))
plant.add("downlink", Downlink(plant_link))

fsw_link = fsw.state("link", TcpServer(addr="[::]:2241", name="fsw"))
fsw.add("sensor_norm", sensor_norm)
fsw.add("downlink", Downlink(fsw_link))
fsw.add(
    "presets",
    Presets(
        [
            Preset(
                name="ops",
                layout=TimeSeriesPlot([Trace("sensor_norm")]),
            )
        ]
    ),
)

deployment = Deployment(targets=[plant, fsw])
