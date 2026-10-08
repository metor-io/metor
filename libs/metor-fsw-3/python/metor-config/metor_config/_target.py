"""`Target`, which collects a target's systems and builds its config."""

from __future__ import annotations

from typing import Any, TypeVar, cast

from ._config import ConfigError, check_abi, simulated_clock, wall_clock
from ._io import Kind, ResourceHandle, ResourceStatus
from ._model import CHANNEL_PORTS, Loop, OutPort, Pack, Record, Source, System, _json, _sources

IO_SUBMIT = "io_submit"
IO_COMPLETE = "io_complete"

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

    def port(self, source: OutPort[T]) -> OutPort[T]:
        """Returns this system's copy of ``source``. See `Subscribe.port`."""
        return self._system._port(source, self._name, self._target)


class Target:
    """A target: a set of systems that run together, in a fixed order.

    Add systems with `add`. They run in the order they were added, once per
    cycle. With ``sim_dt`` (in seconds), the target runs on a simulated clock
    that advances that much each cycle. Without it, the target runs in real
    time at ``cycle_rate`` Hz. ``namespace`` is prefixed to the names the
    target announces to its peers.
    """

    def __init__(
        self,
        cycle_rate: float,
        sim_dt: float | None = None,
        ring_depth: int = 8,
        namespace: str | None = None,
    ) -> None:
        self.cycle_rate = float(cycle_rate)
        self.sim_dt = sim_dt
        self.ring_depth = ring_depth
        self.namespace = namespace
        self._systems: list[tuple[str, System, dict[str, Any] | None]] = []
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
        adapter = _adapter(thread, channel, start)
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
        self._systems.append((name, system, adapter))
        handle = SystemHandle(name, self, system)
        self._handles[name] = handle
        return cast(S, handle)

    @property
    def io_status(self) -> OutPort[ResourceStatus]:
        """`io_complete`'s output of each resource's op counts.

        Only a target with grants has io steps to write it.
        """
        if not self._granted():
            raise ConfigError("no system holds a resource, so there is no io_status")
        return OutPort(IO_COMPLETE, "io_status", ResourceStatus, self)

    def _check_grants(self, name: str, system: System) -> None:
        """Raises `ConfigError` unless every `Cap` of ``system`` is granted a
        resource of this target that provides its interface."""
        caps = type(system)._caps | type(system)._devices
        for param, grant in system._grants.items():
            if param not in caps:
                raise ConfigError(f"`{name}` has no `Cap` or `DeviceSide` parameter `{param}`")
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
        coordinator: dict[str, Any] = {
            "clock": self._clock(),
            "ring_depth": self.ring_depth,
            "namespace": self.namespace,
            "systems": [self._system(index) for index in range(len(self._systems))]
            + self._io_steps(),
        }
        if self._resources:
            coordinator["resources"] = [r.to_json() for r in self._resources.values()]
        return {
            "packs": [
                {**pack.to_json(), **self._wasm.get(pack.id, {})}
                for pack in self._packs.values()
                if pack.lib
            ],
            "coordinator": coordinator,
        }

    def _granted(self) -> list[str]:
        """Returns each system with a `Cap`, in step order."""
        return [name for name, system, _ in self._systems if type(system)._caps]

    def _io_steps(self) -> list[dict[str, Any]]:
        """Returns `io_submit` and `io_complete`, wired to every system with a
        `Cap`, or nothing when no system has one."""
        granted = self._granted()
        if not granted:
            return []
        submit = {
            "id": IO_SUBMIT,
            "ty": "fsw.io_submit",
            "inputs": [
                {"port": name, "from": [{"system": name, "port": "io_sq"}]} for name in granted
            ],
        }
        complete = {
            "id": IO_COMPLETE,
            "ty": "fsw.io_complete",
            "outputs": [{"port": name, "record": "io_cqe"} for name in granted],
        }
        return [submit, complete]

    def _check_commands(self) -> None:
        """Raises `ConfigError` if the target has channels but no command
        source, since those channels could never be commanded."""
        channels = self._channels()
        if channels and not self._commands:
            raise ConfigError(
                f"channel `{channels[0][0]}` has no command source; call `commands(..)`"
            )

    def _system(self, index: int) -> dict[str, Any]:
        name, system, adapter = self._systems[index]
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
            entry["grants"] = [
                {"param": param, "resource": grant.name} for param, grant in system._grants.items()
            ]
        if type(system)._caps:
            entry["inputs"].append(
                {"port": "io_cq", "from": [{"system": IO_COMPLETE, "port": name}]}
            )
        outputs = system._outputs_for(self, index)
        if outputs:
            entry["outputs"] = _json(outputs)
        return entry

    def _clock(self) -> dict[str, Any]:
        if self.sim_dt is not None:
            return simulated_clock(self.sim_dt)
        return wall_clock(self.cycle_rate)


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

