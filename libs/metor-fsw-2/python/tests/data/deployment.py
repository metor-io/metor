"""A two-member deployment, for the pyright gate: the typed surface a
multi-target file uses — `Deployment`, a generated pack entry, a `@system`,
deferred-qualifying `Presets`, and a mirror of a peer's instance whose ports
type as that instance's own."""

from demo import Sink, Widget

from metor_config import (
    Deployment,
    Downlink,
    Preset,
    Presets,
    Publish,
    Subscribe,
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
plant_peers = plant.state("peer", TcpServer(addr="[::]:2242", name="plant-peers"))
widget = plant.add("widget", Widget(count=3, gain=1.5))
plant.add("downlink", Downlink(plant_link))
plant_publish = plant.add("publish", Publish(plant_peers, [widget]))

fsw_link = fsw.state("link", TcpServer(addr="[::]:2241", name="fsw"))
fsw_widget = fsw.add("widget", Subscribe(widget, via=plant_publish))
fsw_sink = fsw.add("sink", Sink())
fsw.connect(fsw_widget.sensors, fsw_sink.sensors)
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
