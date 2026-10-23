"""`Target`, which collects a target's systems and builds its config."""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, TypeVar, cast

from ._config import ConfigError, simulated_clock, wall_clock
from ._generated import CHANNEL_PORTS, COMMANDS_PORT, Io, ResourceStatus
from ._io import Kind, ResourceHandle, SimKind
from ._model import Loop, OutPort, Pack, Record, Source, System, _json, _sources

IO_SUBMIT = "io_submit"
IO_COMPLETE = "io_complete"
IO_TYPE = f"{Io._pack.id}.{Io._ty}"

S = TypeVar("S", bound=System)
T = TypeVar("T", bound=Record)

_targets: list["Target"] = []


@dataclass(frozen=True)
class _Thread:
    """Runs an async system on the thread `name`."""

    name: str

    def to_json(self) -> dict[str, Any]:
        return {"thread": self.name}


@dataclass(frozen=True)
class _Channel:
    """Puts a cyclic system behind the channel `name`, in state `start` at boot."""

    name: str
    start: str

    def to_json(self) -> dict[str, Any]:
        return {"channel": {"name": self.name, "start": self.start}}


@dataclass(frozen=True)
class _Entry:
    """A system added to a target, under its name, and its adapter if any."""

    name: str
    system: System
    adapter: _Thread | _Channel | None


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

    def port(self, source: OutPort[T]) -> OutPort[T]:
        """Returns this system's copy of ``source``. See `Subscribe.port`.

        Raises `ConfigError` unless the system is a `Subscribe`.
        """
        from ._builtins import Subscribe

        if not isinstance(self._system, Subscribe):
            raise ConfigError(f"`{self._name}` copies no ports; only a Subscribe with a peer does")
        return self._system._port(source, self._name, self._target)


class Target:
    """A target: a set of systems that run together, in a fixed order.

    Add systems with `add`. They run in the order they were added, once per
    cycle. With ``sim_dt`` (in seconds), the target runs on a simulated clock
    that advances that much each cycle. Without it, the target runs in real
    time at ``cycle_rate`` Hz. A ring holds ``ring_depth`` records per unit of
    its port's depth, capped at ``ring_bytes`` bytes and never below two
    records. ``namespace`` is prefixed to the names the target announces to
    its peers. ``seed`` is the seed every `Rng` parameter draws from.
    ``epoch`` fixes the start of a simulated clock, in microseconds since the
    Unix epoch.
    """

    def __init__(
        self,
        cycle_rate: float,
        sim_dt: float | None = None,
        ring_depth: int = 8,
        ring_bytes: int = 8 << 20,
        namespace: str | None = None,
        seed: int = 0,
        epoch: int | None = None,
    ) -> None:
        if epoch is not None and sim_dt is None:
            raise ConfigError("`epoch` fixes a simulated clock's start, so it needs `sim_dt`")
        if ring_bytes < 1:
            raise ConfigError(f"`ring_bytes` is at least 1, got {ring_bytes}")
        self.epoch = epoch
        self.seed = seed
        self.cycle_rate = float(cycle_rate)
        self.sim_dt = sim_dt
        self.ring_depth = ring_depth
        self.ring_bytes = ring_bytes
        self.namespace = namespace
        self._systems: list[_Entry] = []
        self._sims: list[_Entry] = []
        self._handles: dict[str, SystemHandle] = {}
        self._commands: list[Source[Any]] = []
        self._packs: dict[str, Pack] = {}
        self._wasm: dict[str, dict[str, Any]] = {}
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
        systems: list[_Entry],
        name: str,
        system: S,
        adapter: _Thread | _Channel | None,
    ) -> S:
        if name in self._handles:
            raise ConfigError(f"system `{name}` is already added")
        self._check_grants(name, system)
        system._bind(self, name)
        for pack in (system._pack, *system._record_packs):
            self._packs.setdefault(pack.id, pack)
        system._name = name
        system._target = self
        systems.append(_Entry(name, system, adapter))
        handle = SystemHandle(name, self, system)
        self._handles[name] = handle
        return cast(S, handle)

    @property
    def io_status(self) -> OutPort[ResourceStatus]:
        """`io_complete`'s output of each resource's op counts.

        Only a target with grants has io steps to write it.
        """
        return OutPort(IO_COMPLETE, "io_status", ResourceStatus, self)

    def _check_grants(self, name: str, system: System) -> None:
        """Raises `ConfigError` unless every grant of ``system`` is a resource
        of this target, since the config names a resource only by its id."""
        for param, grant in system._grants.items():
            if not isinstance(grant, ResourceHandle) or grant.target is not self:
                raise ConfigError(f"`{name}.{param}` takes a resource of this target, got {grant!r}")

    def wasm(self, pack: Pack, *, fuel: int | None = None, memory: int | None = None) -> None:
        """Runs ``pack`` as a WebAssembly module on this target.

        Each of the pack's systems runs in its own sandboxed guest. ``fuel``
        limits how much work each step may do, and ``memory`` limits each
        guest's memory in bytes. When unset, the host's defaults apply.
        """
        if not pack.wasm:
            raise ConfigError(
                f"pack `{pack.id}` has no module; set `wasm = true` under "
                "`[tool.metor.pack]` and rebuild it"
            )
        choice = {"kind": "wasm", "fuel": fuel, "memory": memory}
        self._wasm[pack.id] = {key: value for key, value in choice.items() if value is not None}

    def commands(self, *sources: Source[Any]) -> None:
        """Sends the `SequenceCommand`s from these ports to every channel.

        This applies to every channel on the target, whether it was added
        before or after this call.
        """
        self._commands.extend(_sources(COMMANDS_PORT, sources))

    def _channels(self) -> list[tuple[str, System]]:
        """Returns each channel's name and system, in step order."""
        return [
            (entry.adapter.name, entry.system)
            for entry in self._systems
            if isinstance(entry.adapter, _Channel)
        ]

    def _channel(self, name: str) -> _Channel | None:
        for entry in self._systems:
            if entry.name == name:
                return entry.adapter if isinstance(entry.adapter, _Channel) else None
        return None

    def index(self, name: str) -> int:
        """Returns the position of the system `name` in step order."""
        for index, entry in enumerate(self._systems):
            if entry.name == name:
                return index
        raise ConfigError(f"target `{self.namespace}` has no system `{name}`")

    def to_config(self) -> dict[str, Any]:
        """Returns this target's entry in the deployment config.

        The entry lists the target's packs and its coordinator config. A pack
        with no library is built into the host, so it is not listed.
        """
        self._check_commands()
        coordinator: dict[str, Any] = {
            "clock": self._clock(),
            "ring_depth": self.ring_depth,
            "ring_bytes": self.ring_bytes,
            "namespace": self.namespace,
            "systems": self._steps(),
        }
        if self.seed:
            coordinator["seed"] = self.seed
        if self._resources:
            coordinator["resources"] = {
                name: self._resource(r) for name, r in self._resources.items()
            }
        return {
            "packs": [
                {**pack.to_json(), **self._wasm.get(pack.id, {})}
                for pack in self._packs.values()
                if pack.lib
            ],
            "coordinator": coordinator,
        }

    def _steps(self) -> list[dict[str, Any]]:
        """Returns every system's entry: the flight systems, `io_submit`, the
        models added with `sim`, then `io_complete`.

        The build wires each system that holds a `Cap` to the io systems.
        """
        flight = [self._system(self._systems, index) for index in range(len(self._systems))]
        sims = [self._system(self._sims, index) for index in range(len(self._sims))]
        if not self._holds_caps():
            return flight + sims
        submit = {"id": IO_SUBMIT, "ty": IO_TYPE}
        complete = {"id": IO_COMPLETE, "ty": IO_TYPE, "params": {"status": True}}
        return flight + [submit, *sims, complete]

    def _holds_caps(self) -> bool:
        """Returns whether any system holds a `Cap`, so the target needs io steps."""
        return any(type(entry.system)._caps for entry in self._systems + self._sims)

    def _resource(self, resource: ResourceHandle) -> dict[str, Any]:
        """Returns a resource's backing. A sim resource names the system and
        parameter holding each of its ends, and raises `ConfigError` if either
        end has no holder."""
        if not isinstance(resource.kind, SimKind):
            return resource.to_json()
        ends: dict[str, dict[str, str]] = {}
        for entry in self._systems + self._sims:
            for param, grant in entry.system._grants.items():
                if grant.name == resource.name:
                    ends[grant.end] = {"system": entry.name, "param": param}
        for end in ("host", "device"):
            if end not in ends:
                raise ConfigError(f"sim resource `{resource.name}` has no holder on its {end} end")
        sim = {"kind": resource.kind._key, "host": ends["host"], "device": ends["device"]}
        return {"sim": sim}

    def _check_commands(self) -> None:
        """Raises `ConfigError` if the target has channels but no command
        source, since those channels could never be commanded."""
        channels = self._channels()
        if channels and not self._commands:
            raise ConfigError(
                f"channel `{channels[0][0]}` has no command source; call `commands(..)`"
            )

    def _system(self, systems: list[_Entry], index: int) -> dict[str, Any]:
        name, system, adapter = systems[index].name, systems[index].system, systems[index].adapter
        entry: dict[str, Any] = {
            "id": name,
            "ty": f"{system._pack.id}.{system._ty}",
            "params": _json(system._params_for(self, index)),
        }
        if adapter is not None:
            entry.update(adapter.to_json())
        inputs = dict(system._inputs_for(self, index))
        if isinstance(adapter, _Channel) and self._commands:
            inputs[COMMANDS_PORT] = self._commands
        entry["inputs"] = [
            {
                "port": port,
                "from": [_resolve(self, name, port, source) for source in sources],
            }
            for port, sources in inputs.items()
        ]
        grants = {
            param: grant.name
            for param, grant in system._grants_for(self, index).items()
            if not isinstance(grant.kind, SimKind)
        }
        if grants:
            entry["grants"] = grants
        outputs = system._outputs_for(self, index)
        if outputs:
            entry["outputs"] = _json(outputs)
        if system._location is not None:
            entry["location"] = system._location
        return entry

    def _clock(self) -> dict[str, Any]:
        if self.sim_dt is not None:
            return simulated_clock(self.sim_dt, self.epoch)
        return wall_clock(self.cycle_rate)


def _adapter(
    thread: str | None, channel: str | None, start: str | None
) -> _Thread | _Channel | None:
    """Returns a system's adapter, if it has one."""
    if thread is not None and channel is not None:
        raise ConfigError("a system takes `thread` or `channel`, not both")
    if start is not None and channel is None:
        raise ConfigError("`start` sets a channel's state at boot, so it needs `channel`")
    if thread is not None:
        return _Thread(thread)
    if channel is None:
        return None
    return _Channel(channel, start or "running")


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

