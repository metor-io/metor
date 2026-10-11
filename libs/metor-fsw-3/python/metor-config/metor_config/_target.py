"""`Target`, which collects a target's systems and builds its config."""

from __future__ import annotations

from typing import Any, TypeVar, cast

from ._config import ConfigError, check_abi, simulated_clock, wall_clock
from ._io import Kind, ResourceHandle, ResourceStatus, SimKind
from ._model import CHANNEL_PORTS, Loop, OutPort, Pack, Record, Source, System, _json, _sources

IO_SUBMIT = "io_submit"
IO_COMPLETE = "io_complete"
IO_TYPE = "fsw.io"

S = TypeVar("S", bound=System)
T = TypeVar("T", bound=Record)

_targets: list["Target"] = []


class SystemHandle:
    """A handle to a system added to a target.

    Reading an attribute returns the `OutPort` for that output. `Target.add`
    returns the handle typed as the `System` subclass it was given, so a type
    checker knows each port's record.
    """

    def __init__(self, name: str, target: "Target", system: System) -> None:
        self._name = name
        self._target = target
        self._system = system

    def __getattr__(self, port: str) -> Any:
        record = self._port_map().get(port)
        if record is None:
            raise AttributeError(f"system `{self._name}` has no output port `{port}`")
        return OutPort(self._name, port, record, self._target)

    def _port_map(self) -> dict[str, type[Record]]:
        """Returns every output of this system, including a channel's outputs
        when it has one."""
        ports = self._system._port_map()
        if self._target._channel(self._name) is None:
            return ports
        return {**ports, **CHANNEL_PORTS}


class Target:
    """A target: a set of systems that run together, in a fixed order.

    Add systems with `add`. They run in the order they were added, once per
    cycle. With ``sim_dt`` (in seconds), the target runs on a simulated clock
    that advances that much each cycle. Without it, the target runs in real
    time at ``cycle_rate`` Hz. ``namespace`` is prefixed to the names the
    target announces to its peers. ``seed`` is the seed every `Rng` parameter
    draws from. ``epoch`` fixes the start of a simulated clock, in
    microseconds since the Unix epoch.
    """

    def __init__(
        self,
        cycle_rate: float,
        sim_dt: float | None = None,
        ring_depth: int = 8,
        namespace: str | None = None,
        seed: int = 0,
        epoch: int | None = None,
    ) -> None:
        if not 0 <= seed < 2**64:
            raise ConfigError(f"`seed` is an unsigned 64-bit integer, got {seed}")
        if epoch is not None and sim_dt is None:
            raise ConfigError("`epoch` fixes a simulated clock's start, so it needs `sim_dt`")
        self.epoch = epoch
        self.seed = seed
        self.cycle_rate = float(cycle_rate)
        self.sim_dt = sim_dt
        self.ring_depth = ring_depth
        self.namespace = namespace
        self._systems: list[tuple[str, System, dict[str, Any] | None]] = []
        self._sims: list[tuple[str, System, dict[str, Any] | None]] = []
        self._handles: dict[str, SystemHandle] = {}
        self._commands: list[Source[Any]] = []
        self._packs: dict[str, Pack] = {}
        self._resources: dict[str, ResourceHandle] = {}
        _targets.append(self)

    def resource(self, name: str, kind: Kind) -> ResourceHandle:
        """Creates a resource called ``name``, such as ``Tty("/dev/ttyS0")``.

        Grant it to a system by passing the returned handle as one of the
        system's `Cap` keywords. Several systems may share one resource.
        """
        if not name.isidentifier():
            raise ConfigError(f"a resource name is an identifier, got {name!r}")
        if name in self._resources:
            raise ConfigError(f"resource `{name}` is already created")
        if not isinstance(kind, Kind):
            raise ConfigError(f"resource `{name}` takes a kind such as `Tty`, got {kind!r}")
        handle = ResourceHandle(name, kind, self)
        self._resources[name] = handle
        return handle

    def loop(self, record: type[T]) -> Loop[T]:
        """Creates a feedback edge for ``record`` with a one-cycle delay.

        Pass the loop to a system that needs to read from a producer added
        later. Call `Loop.connect` once the producer exists.
        """
        return Loop(record)

    def add(
        self,
        name: str,
        system: S,
        thread: str | None = None,
        channel: str | None = None,
        start: str | None = None,
    ) -> S:
        """Adds ``system`` to the target under ``name``.

        Returns a handle whose attributes are the system's output ports.
        Systems run in the order they are added, so a system can only read
        ports of systems added before it. Use `loop` to read from a later one.

        ``thread`` runs an async system on the named thread. ``channel`` puts
        a cyclic system behind a channel with that name, so it can be started,
        stopped, and loaded by commands. This adds ``events`` and
        ``channel_status`` outputs. ``start`` sets the channel's state at boot:
        ``"running"`` (the default) or ``"stopped"``.
        """
        return self._add(self._systems, name, system, _adapter(thread, channel, start))

    def sim(self, name: str, system: S) -> S:
        """Adds ``system`` as a model: a system that holds the device end of
        sim resources.

        Models run between `io_submit` and `io_complete`, so a driver's
        request reaches its model the cycle it is sent, and the model's
        answer reaches the driver the next cycle.
        """
        return self._add(self._sims, name, system, None)

    def _add(
        self,
        systems: list[tuple[str, System, dict[str, Any] | None]],
        name: str,
        system: S,
        adapter: dict[str, Any] | None,
    ) -> S:
        if name in (IO_SUBMIT, IO_COMPLETE):
            raise ConfigError(f"`{name}` is the name of an io step")
        if name in self._handles:
            raise ConfigError(f"system `{name}` is already added")
        self._check_grants(name, system)
        packs = (system._pack, *system._record_packs)
        for pack in packs:
            check_abi(pack.id, pack.abi_version)
        for pack in packs:
            self._packs.setdefault(pack.id, pack)
        system._name = name
        system._target = self
        systems.append((name, system, adapter))
        handle = SystemHandle(name, self, system)
        self._handles[name] = handle
        return cast(S, handle)

    @property
    def io_status(self) -> OutPort[ResourceStatus]:
        """`io_complete`'s output of each resource's op counts.

        Only a target with grants has io steps to write it.
        """
        if not self._members(self._systems) + self._members(self._sims):
            raise ConfigError("no system holds a resource, so there is no io_status")
        return OutPort(IO_COMPLETE, "io_status", ResourceStatus, self)

    def _check_grants(self, name: str, system: System) -> None:
        """Raises `ConfigError` unless every `Cap` of ``system`` is granted a
        resource of this target that provides its interface."""
        caps = type(system)._caps
        for param, grant in system._grants.items():
            if param not in caps:
                raise ConfigError(f"`{name}` has no `Cap` parameter `{param}`")
            if not isinstance(grant, ResourceHandle) or grant.target is not self:
                raise ConfigError(f"`{name}.{param}` takes a resource of this target, got {grant!r}")
            if grant.kind.interface != caps[param]:
                raise ConfigError(
                    f"`{name}.{param}` takes a `{caps[param]}`, but `{grant.name}` "
                    f"provides `{grant.kind.interface}`"
                )
        for param in caps:
            if param not in system._grants:
                raise ConfigError(f"`{name}.{param}` is not granted a resource")

    def commands(self, *sources: Source[Any]) -> None:
        """Sends the `SequenceCommand`s from these ports to every channel.

        This applies to every channel on the target, whether it was added
        before or after this call.
        """
        self._commands.extend(_sources("commands", sources))

    def _channels(self) -> list[tuple[str, System]]:
        """Returns each channel's name and system, in step order."""
        return [
            (adapter["channel"]["name"], system)
            for _, system, adapter in self._systems
            if adapter is not None and "channel" in adapter
        ]

    def _channel(self, name: str) -> dict[str, Any] | None:
        for added, _, adapter in self._systems:
            if added == name:
                return adapter.get("channel") if adapter is not None else None
        return None

    def index(self, name: str) -> int:
        """Returns the position of the system `name` in step order."""
        for index, (added, _, _) in enumerate(self._systems):
            if added == name:
                return index
        raise ConfigError(f"target `{self.namespace}` has no system `{name}`")

    def to_config(self) -> dict[str, Any]:
        """Returns this target's entry in the deployment config.

        The entry lists the target's packs and its coordinator config. A pack
        with no library is built into the host, so it is not listed.
        """
        self._check_commands()
        self._check_ends()
        coordinator: dict[str, Any] = {
            "clock": self._clock(),
            "ring_depth": self.ring_depth,
            "namespace": self.namespace,
            "systems": self._steps(),
        }
        if self.seed:
            coordinator["seed"] = self.seed
        if self._resources:
            coordinator["resources"] = [r.to_json() for r in self._resources.values()]
        return {
            "packs": [
                pack.to_json()
                for pack in self._packs.values()
                if pack.lib
            ],
            "coordinator": coordinator,
        }

    def _members(self, systems: list[tuple[str, System, dict[str, Any] | None]]) -> list[str]:
        """Returns each system of ``systems`` that holds a `Cap`, in step order."""
        return [name for name, system, _ in systems if type(system)._caps]

    def _steps(self) -> list[dict[str, Any]]:
        """Returns every system's entry: the flight systems, `io_submit`, the
        models added with `sim`, then `io_complete`.

        Each io system lowers the ops of the systems since the previous one,
        and posts the completions of the systems up to the next one, wrapping
        from the last io system around to the first systems.
        """
        flight = [self._system(self._systems, index) for index in range(len(self._systems))]
        sims = [self._system(self._sims, index) for index in range(len(self._sims))]
        drivers, models = self._members(self._systems), self._members(self._sims)
        if not drivers and not models:
            return flight + sims
        submit = self._io(IO_SUBMIT, drivers, models, False)
        complete = self._io(IO_COMPLETE, models, drivers, True)
        return flight + [submit, *sims, complete]

    def _io(self, name: str, sqs: list[str], cqs: list[str], status: bool) -> dict[str, Any]:
        """An io system, reading the SQs of ``sqs`` and writing the CQs of
        ``cqs``."""
        entry: dict[str, Any] = {
            "id": name,
            "ty": IO_TYPE,
            "inputs": [{"port": m, "from": [{"system": m, "port": "io_sq"}]} for m in sqs],
        }
        if cqs:
            entry["outputs"] = [{"port": m, "record": "io_cqe"} for m in cqs]
        if status:
            entry["params"] = {"status": True}
        return entry

    def _check_ends(self) -> None:
        """Raises `ConfigError` unless each sim resource that is held is held
        on both ends."""
        held = [
            (grant.name, grant.end)
            for _, system, _ in self._systems + self._sims
            for grant in system._grants.values()
        ]
        for resource in self._resources.values():
            ends = {end for name, end in held if name == resource.name}
            if not isinstance(resource.kind, SimKind) or not ends:
                continue
            for end in ("host", "device"):
                if end not in ends:
                    raise ConfigError(f"sim resource `{resource.name}` has no holder on its {end} end")

    def _check_commands(self) -> None:
        """Raises `ConfigError` if the target has channels but no command
        source, since those channels could never be commanded."""
        channels = self._channels()
        if channels and not self._commands:
            raise ConfigError(
                f"channel `{channels[0][0]}` has no command source; call `commands(..)`"
            )

    def _system(
        self, systems: list[tuple[str, System, dict[str, Any] | None]], index: int
    ) -> dict[str, Any]:
        name, system, adapter = systems[index]
        entry: dict[str, Any] = {
            "id": name,
            "ty": f"{system._pack.id}.{system._ty}",
            "params": _json(system._params_for(self, index)),
        }
        if adapter is not None:
            entry.update(adapter)
        inputs = dict(system._inputs_for(self, index))
        if adapter is not None and "channel" in adapter and self._commands:
            inputs["commands"] = self._commands
        entry["inputs"] = [
            {
                "port": port,
                "from": [_resolve(self, name, port, source) for source in sources],
            }
            for port, sources in inputs.items()
        ]
        if system._grants:
            entry["grants"] = [_grant(param, grant) for param, grant in system._grants.items()]
        if type(system)._caps:
            posts = IO_SUBMIT if systems is self._sims else IO_COMPLETE
            entry["inputs"].append({"port": "io_cq", "from": [{"system": posts, "port": name}]})
        outputs = system._outputs_for(self, index)
        if outputs:
            entry["outputs"] = _json(outputs)
        return entry

    def _clock(self) -> dict[str, Any]:
        if self.sim_dt is not None:
            return simulated_clock(self.sim_dt, self.epoch)
        return wall_clock(self.cycle_rate)


def _grant(param: str, grant: ResourceHandle) -> dict[str, Any]:
    """Returns a grant's entry, naming its end when it is not the host."""
    entry = {"param": param, "resource": grant.name}
    if grant.end != "host":
        entry["end"] = grant.end
    return entry


def _adapter(
    thread: str | None, channel: str | None, start: str | None
) -> dict[str, Any] | None:
    """Returns the adapter entry for a system's config, if it has one."""
    if thread is not None and channel is not None:
        raise ConfigError("a system takes `thread` or `channel`, not both")
    if start is not None and channel is None:
        raise ConfigError("`start` sets a channel's state at boot, so it needs `channel`")
    if start not in (None, "running", "stopped"):
        raise ConfigError(f"`start` is `running` or `stopped`, got {start!r}")
    if thread is not None:
        return {"thread": thread}
    if channel is None:
        return None
    return {"channel": {"name": channel, "start": start or "running"}}


def _resolve(target: Target, name: str, port: str, source: Source[Any]) -> dict[str, str]:
    if isinstance(source, Loop):
        if source.producer is None:
            raise ConfigError(
                f"the loop into `{name}.{port}` has no producer; call `connect` on it"
            )
        return _local(target, name, port, source.producer)
    return _local(target, name, port, source)


def _local(target: Target, name: str, port: str, source: OutPort[Any]) -> dict[str, str]:
    """Returns the reference to ``source``.

    Raises `ConfigError` if ``source`` belongs to another target, since a
    system can only read ports on its own target.
    """
    if source.target is not None and source.target is not target:
        raise ConfigError(
            f"`{name}.{port}` reads `{source.name}` on another target; subscribe to "
            "that link and pass the subscriber's `port()` instead"
        )
    return source.ref.to_json()

