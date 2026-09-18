"""Record a target's systems and emit its coordinator config."""

from __future__ import annotations

import atexit
import json
import os
import sys
from typing import Any, TypeVar, cast

from ._config import OUT_ENV, CONFIG_VERSION, ConfigError, check_abi, simulated_clock, wall_clock
from ._model import Loop, OutPort, Pack, Record, Source, System

S = TypeVar("S", bound=System)
T = TypeVar("T", bound=Record)

_targets: list["Target"] = []
_emitted = False

ALWAYS_OUTPUTS = ("log", "status")


class SystemHandle:
    """A registered system's ports. Attribute access yields the `OutPort` of a
    declared output; `Target.add` types it as the `System` subclass it was
    handed, so a checker resolves each port's record."""

    def __init__(self, name: str, outputs: tuple[str, ...]) -> None:
        self._name = name
        self._outputs = tuple(outputs) + ALWAYS_OUTPUTS

    def __getattr__(self, port: str) -> Any:
        if port not in self._outputs:
            raise AttributeError(f"system `{self._name}` has no output port `{port}`")
        return OutPort(self._name, port)


class Target:
    """A target under construction: its clock, its rings' depth, and its systems
    in step order.

    ``sim_dt`` (seconds) selects the simulated clock; without it the loop paces a
    wall clock at ``cycle_rate``. ``namespace`` is recorded for the later slices
    that prefix announced names with it.
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
        self._systems: list[tuple[str, System, str | None]] = []
        self._handles: dict[str, SystemHandle] = {}
        self._packs: dict[str, Pack] = {}
        _targets.append(self)

    def loop(self, record: type[T]) -> Loop[T]:
        """A one-cycle delayed edge of ``record``, read by a system added before
        its producer. Connect it once the producer exists."""
        return Loop(record)

    def add(self, name: str, system: S, thread: str | None = None) -> S:
        """Register ``system`` as ``name``; the returned handle carries its output
        ports. Add order is step order, so a system only names ports of systems
        already added. ``thread`` places an async system on a thread of that name.
        """
        if name in self._handles:
            raise ConfigError(f"system `{name}` is already added")
        pack = system._pack
        check_abi(pack.id, pack.abi_version)
        self._packs.setdefault(pack.id, pack)
        self._systems.append((name, system, thread))
        handle = SystemHandle(name, system._outputs)
        self._handles[name] = handle
        return cast(S, handle)

    def to_config(self) -> dict[str, Any]:
        """The config file as a dict: this target's packs and its coordinator.

        A pack with no library is built into the host, so it is not listed.
        """
        self._finalize()
        return {
            "config_version": CONFIG_VERSION,
            "packs": [pack.to_json() for pack in self._packs.values() if pack.lib],
            "coordinator": {
                "clock": self._clock(),
                "ring_depth": self.ring_depth,
                "systems": [
                    _system(name, system, thread) for name, system, thread in self._systems
                ],
            },
        }

    def _finalize(self) -> None:
        """Hands every system that asked for it this target, before emission."""
        for index, (name, system, _) in enumerate(self._systems):
            if not system._wants_target:
                continue
            system._params["namespace"] = self.namespace
            system._params["link"] = name
            system._finalize(self, index)

    def _clock(self) -> dict[str, Any]:
        if self.sim_dt is not None:
            return simulated_clock(self.sim_dt)
        return wall_clock(self.cycle_rate)


def _system(name: str, system: System, thread: str | None) -> dict[str, Any]:
    entry: dict[str, Any] = {
        "id": name,
        "ty": f"{system._pack.id}.{system._ty}",
        "params": system._params or None,
    }
    if thread is not None:
        entry["thread"] = thread
    entry["inputs"] = [
        {"port": port, "from": [_resolve(name, port, source) for source in sources]}
        for port, sources in system._inputs.items()
    ]
    if system._dyn_outputs:
        entry["outputs"] = system._dyn_outputs
    return entry


def _resolve(name: str, port: str, source: Source[Any]) -> dict[str, str]:
    if isinstance(source, Loop):
        if source.producer is None:
            raise ConfigError(
                f"the loop into `{name}.{port}` has no producer; call `connect` on it"
            )
        return source.producer.ref.to_json()
    return source.ref.to_json()


def emit(path: str | None = None) -> None:
    """Write this file's target config to ``path``, else ``$METOR_CONFIG_OUT``,
    else stdout."""
    global _emitted
    text = json.dumps(_the_target().to_config(), indent=2, sort_keys=False) + "\n"
    out = path or os.environ.get(OUT_ENV)
    if out:
        with open(out, "w", encoding="utf-8") as file:
            file.write(text)
    else:
        sys.stdout.write(text)
    _emitted = True


def _the_target() -> Target:
    if len(_targets) != 1:
        raise ConfigError(f"exactly one Target must exist at emission, found {len(_targets)}")
    return _targets[0]


def _emit_at_exit() -> None:
    # A target file is a script that ends; a file that raised has already
    # printed its traceback.
    if not _emitted and len(_targets) == 1:
        emit()


atexit.register(_emit_at_exit)
