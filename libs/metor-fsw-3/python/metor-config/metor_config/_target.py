"""Record a target's systems and build its config."""

from __future__ import annotations

from typing import Any, TypeVar, cast

from ._config import ConfigError, check_abi, simulated_clock, wall_clock
from ._model import CHANNEL_PORTS, Loop, OutPort, Pack, Record, Source, System, _json, _sources

S = TypeVar("S", bound=System)
T = TypeVar("T", bound=Record)

_targets: list["Target"] = []


class SystemHandle:
    """A registered system's ports. Attribute access yields the `OutPort` of a
    declared output; `Target.add` types it as the `System` subclass it was
    handed, so a checker resolves each port's record."""

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
        """Every output this system has, with a channel's own when it has one."""
        ports = self._system._port_map()
        if self._target._channel(self._name) is None:
            return ports
        return {**ports, **CHANNEL_PORTS}

    def port(self, source: OutPort[T]) -> OutPort[T]:
        """This system's copy of ``source``; see `Subscribe.port`."""
        return self._system._port(source, self._name, self._target)


class Target:
    """A target under construction: its clock, its rings' depth, and its systems
    in step order.

    ``sim_dt`` (seconds) selects the simulated clock; without it the loop paces a
    wall clock at ``cycle_rate``. ``namespace`` prefixes announced names.
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
        _targets.append(self)

    def loop(self, record: type[T]) -> Loop[T]:
        """A one-cycle delayed edge of ``record``, read by a system added before
        its producer. Connect it once the producer exists."""
        return Loop(record)

    def add(
        self,
        name: str,
        system: S,
        thread: str | None = None,
        channel: str | None = None,
        start: str | None = None,
    ) -> S:
        """Register ``system`` as ``name``; the returned handle carries its output
        ports. Add order is step order, so a system only names ports of systems
        already added. ``thread`` places an async system on a thread of that name.

        ``channel`` makes a cyclic system commandable under that name, adding
        its ``events`` and ``channel_status`` outputs; ``start`` is
        ``"running"`` or ``"stopped"``, its state at boot.
        """
        adapter = _adapter(thread, channel, start)
        if name in self._handles:
            raise ConfigError(f"system `{name}` is already added")
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

    def wasm(self, pack: Pack, *, fuel: int | None = None, memory: int | None = None) -> None:
        """Load ``pack`` as a WebAssembly module on this target, each system in
        a guest of its own. ``fuel`` is each step's grant and ``memory`` each
        guest's ceiling in bytes; unset, the host's defaults apply."""
        if not pack.wasm:
            raise ConfigError(
                f"pack `{pack.id}` has no module; set `wasm = true` under "
                "`[tool.metor.pack]` and rebuild it"
            )
        choice = {"kind": "wasm", "fuel": fuel, "memory": memory}
        self._wasm[pack.id] = {key: value for key, value in choice.items() if value is not None}

    def commands(self, *sources: Source[Any]) -> None:
        """Feed every channel, added before or after this call, the
        `SequenceCommand`s these ports carry."""
        self._commands.extend(_sources("commands", sources))

    def _channels(self) -> list[tuple[str, System]]:
        """Each channel's name and its system, in step order."""
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
        """Where the system `name` sits in step order."""
        for index, (added, _, _) in enumerate(self._systems):
            if added == name:
                return index
        raise ConfigError(f"target `{self.namespace}` has no system `{name}`")

    def to_config(self) -> dict[str, Any]:
        """This target's packs and its coordinator, as one member of the envelope.

        A pack with no library is built into the host, so it is not listed.
        """
        self._check_commands()
        return {
            "packs": [
                {**pack.to_json(), **self._wasm.get(pack.id, {})}
                for pack in self._packs.values()
                if pack.lib
            ],
            "coordinator": {
                "clock": self._clock(),
                "ring_depth": self.ring_depth,
                "namespace": self.namespace,
                "systems": [
                    self._system(index) for index in range(len(self._systems))
                ],
            },
        }

    def _check_commands(self) -> None:
        """A channel no command reaches cannot be commanded, so it is refused."""
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
    """The adapter key a system's config entry carries, if any."""
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
    """``source``'s reference, which only names a port of ``target`` itself."""
    if source.target is not None and source.target is not target:
        raise ConfigError(
            f"`{name}.{port}` reads `{source.name}` on another target; subscribe to "
            "that link and pass the subscriber's `port()` instead"
        )
    return source.ref.to_json()

