"""Record target topology and emit the wiring IR."""

from __future__ import annotations

import atexit
import json
import os
import sys
from contextlib import contextmanager
from dataclasses import fields
from typing import Any, Iterator, overload
from ._version import __version__, IR_VERSION, PROGRAM_ARTIFACT, COORDINATOR
from ._model import (
    Artifact,
    Spec,
    StateHandle,
    SystemHandle,
    OutPort,
    InPort,
    F,
    H,
    static_system,
    _handle_name,
    _source_ref,
)
from ._program import ExprHandle, _program
from ._dashboard import Preset


_targets: list["Target"] = []


_INIT_STATES = {"loaded": "Loaded", "running": "Running"}


class Target:
    """A target under construction. Exactly one may exist at emission time.

    ``sim_dt`` (seconds) selects a free-running simulated clock; without it the
    loop paces a wall clock at ``cycle_rate``. ``default_depth`` is the in-flight
    record depth for a snapshot buffer. ``namespace`` is a bare dotted
    prefix (``"sat1"``) stamped onto every component name this target registers
    and announces, so several targets sharing one db keep disjoint id spaces;
    ``None`` leaves names and ids identical to an un-namespaced target.
    ``wasm_fuel_per_poll`` bounds one guest poll and
    ``wasm_memory_limit_bytes`` bounds guest memory during load and bind;
    omitted values select 100,000,000 fuel and 64 MiB.
    """

    def __init__(
        self,
        cycle_rate: float,
        sim_dt: float | None = None,
        default_depth: int | None = None,
        namespace: str | None = None,
        wasm_fuel_per_poll: int | None = None,
        wasm_memory_limit_bytes: int | None = None,
    ):
        self.cycle_rate = float(cycle_rate)
        self.sim_dt = sim_dt
        self.default_depth = default_depth
        self.namespace = namespace
        self.wasm_fuel_per_poll = wasm_fuel_per_poll
        self.wasm_memory_limit_bytes = wasm_memory_limit_bytes
        self.coordinator = SystemHandle(COORDINATOR)
        self._artifacts: list[dict[str, Any]] = []
        self._artifact_decls: dict[str, Artifact] = {}
        self._states: list[dict[str, Any]] = []
        self._systems: list[dict[str, Any]] = []
        self._slots: list[dict[str, Any]] = []
        self._edges: list[dict[str, Any]] = []
        self._scopes: list[dict[str, Any]] = []
        self._scope_stack: list[int] = []
        _targets.append(self)

    # -- scopes -------------------------------------------------------------

    @contextmanager
    def scope(self, name: str) -> Iterator[None]:
        """Prefix instance names with ``name.`` and record the scope in the IR,
        nesting through the enclosing scope."""
        parent = self._scope_stack[-1] if self._scope_stack else None
        prefix = self._scopes[parent]["path"] if parent is not None else None
        path = f"{prefix}.{name}" if prefix else name
        index = len(self._scopes)
        self._scopes.append({"path": path, "parent": parent, "src": _source_ref()})
        self._scope_stack.append(index)
        try:
            yield
        finally:
            self._scope_stack.pop()

    def _scoped(self, name: str) -> tuple[str, int | None]:
        if not self._scope_stack:
            return name, None
        index = self._scope_stack[-1]
        return f"{self._scopes[index]['path']}.{name}", index

    def _register_spec_artifact(self, spec: Spec) -> None:
        """Auto-register a generated :class:`System`'s declaring artifact."""
        decl = getattr(spec, "artifact_decl", None)
        if decl is not None:
            self._register_artifact(decl)

    def _register_artifact(self, decl: "Artifact") -> None:
        """Merge compatible declarations; reject conflicting identities.

        Optional provenance may be supplied by a later declaration. The host
        supplies ``METOR_FSW_ABI_VERSION`` during evaluation; standalone
        recording leaves ABI compatibility to the host's load-time check.
        """
        if decl.kind not in ("cdylib", "wasm"):
            raise ValueError(f"artifact `{decl.id}`: unknown kind `{decl.kind}`")
        if decl.kind == "cdylib" and (not decl.crate or not decl.lib):
            raise ValueError(
                f"artifact `{decl.id}`: a cdylib names a crate and a lib stem"
            )
        expected = os.environ.get("METOR_FSW_ABI_VERSION")
        if expected is not None and decl.abi_version is not None:
            if decl.abi_version != int(expected):
                raise ValueError(
                    f"artifact `{decl.id}`: FSW ABI {decl.abi_version} differs "
                    f"from host ABI {expected}; rebuild the pack"
                )
        previous = self._artifact_decls.get(decl.id)
        if previous is not None:
            merged: dict[str, Any] = {}
            for field in fields(Artifact):
                old, new = getattr(previous, field.name), getattr(decl, field.name)
                if old is not None and new is not None and old != new:
                    raise ValueError(
                        f"artifact `{decl.id}`: conflicting {field.name}: {old!r} and {new!r}"
                    )
                merged[field.name] = old if old is not None else new
            decl = Artifact(**merged)
        entry = {
            "id": decl.id,
            "path": None,
            "prebuilt_dir": decl.prebuilt,
            "dist": (
                {"name": decl.dist, "version": decl.dist_version}
                if decl.dist is not None and decl.dist_version is not None
                else None
            ),
            "manifest_hash": decl.manifest_hash,
            "src": _source_ref(),
        }
        # Mirror serde's skips: `kind` is omitted for the default cdylib, and
        # the crate/lib fields are omitted when empty (a wasm artifact).
        if decl.kind != "cdylib":
            entry["kind"] = decl.kind
        if decl.crate is not None:
            entry["crate_name"] = decl.crate
        if decl.lib is not None:
            entry["lib"] = decl.lib
        if previous is None:
            self._artifacts.append(entry)
        else:
            index = next(
                i for i, item in enumerate(self._artifacts) if item["id"] == decl.id
            )
            entry["src"] = self._artifacts[index]["src"]
            self._artifacts[index] = entry
        self._artifact_decls[decl.id] = decl

    # -- systems and slots --------------------------------------------------

    def state(self, name: str, spec: Spec) -> StateHandle:
        """Declare a pack-shared state instance (``link = m.state("link",
        TcpServer(addr="0.0.0.0:2240"))``): constructed once, before any
        system, from its own params. States live in their own namespace and
        take no edges. The returned handle is passed to a shared-state
        system's constructor (``Downlink(link)``) to attach it."""
        self._states.append(
            {
                "name": name,
                "ty": spec.ty,
                "params": spec._param_source(),
                "src": _source_ref(),
            }
        )
        return StateHandle(name)

    @overload
    def add(
        self,
        name: str,
        spec: ExprHandle,
        process: bool = False,
        node: tuple[float, float] | None = None,
    ) -> ExprHandle: ...

    @overload
    def add(
        self,
        name: str,
        spec: H,
        process: bool = False,
        node: tuple[float, float] | None = None,
    ) -> H: ...

    def add(
        self,
        name: str,
        spec: Any,
        process: bool = False,
        node: tuple[float, float] | None = None,
    ) -> Any:
        """Register ``spec`` under ``name`` (scope-prefixed) and return its handle.

        The return is typed as the spec's own class so a generated entry's port
        attributes (``plant.sensors``) are checkable; at runtime it is a
        :class:`SystemHandle` whose attribute access yields :class:`PortRef`s.
        A generated :class:`System` also auto-registers its artifact. ``node``
        pins the system's canvas card, the :func:`node` decorator's twin for a
        native system. A ``@system`` handle registers here too, at this call's
        step position, and comes back renamed to the instance."""
        full, scope = self._scoped(name)
        if isinstance(spec, ExprHandle):
            return self._add_expr(full, scope, spec, process, node)
        self._register_spec_artifact(spec)
        self._systems.append(
            {
                "name": full,
                "ty": spec.ty,
                "artifact": spec.artifact,
                "params": spec._param_source(),
                "process": bool(process),
                "src": _source_ref(),
                "scope": scope,
                "attach": spec.attach,
                "layout": [float(node[0]), float(node[1])] if node else None,
            }
        )
        return SystemHandle(full)

    def _add_expr(
        self,
        full: str,
        scope: int | None,
        handle: ExprHandle,
        process: bool,
        node: tuple[float, float] | None,
    ) -> ExprHandle:
        """The ``@system`` arm of :meth:`add`: an ordinary spec addressing the
        declaration's pack entry, at this call's position in the system list."""
        entry = handle._entry
        if process:
            raise ValueError(
                f"system `{full}`: `process=True` is redundant for a Python "
                "system (the interpreter already isolates it)"
            )
        if entry["added"] is not None:
            raise ValueError(
                f"`{entry['name']}` is already added as `{entry['added']}`; "
                "multi-instance binding of one function is future work"
            )
        entry["added"] = full
        handle.name = full
        self._systems.append(
            {
                "name": full,
                "ty": entry["name"],
                "artifact": PROGRAM_ARTIFACT,
                "params": "None",
                "process": False,
                "src": _source_ref(),
                "scope": scope,
                "attach": None,
                "layout": [float(node[0]), float(node[1])] if node else entry["layout"],
            }
        )
        return handle

    def slot(
        self,
        name: str,
        inputs: list[str],
        outputs: list[str],
        allow: list[Spec],
        initial: str | None = None,
        initial_state: str = "running",
        process: bool = False,
    ) -> SystemHandle:
        """Register a runtime-loadable slot. ``allow`` occupants use the same
        call convention as system specs; ``initial`` names one of them."""
        full, scope = self._scoped(name)
        for occupant in allow:
            self._register_spec_artifact(occupant)
        occupants = [
            {
                "occupant": s.ty,
                "artifact": s.artifact,
                "params": s._param_source(),
                "src": _source_ref(),
            }
            for s in allow
        ]
        initial_spec = None
        if initial is not None:
            state = _INIT_STATES.get(initial_state, initial_state.capitalize())
            initial_spec = {"occupant": initial, "state": state}
        self._slots.append(
            {
                "name": full,
                "inputs": list(inputs),
                "outputs": list(outputs),
                "allow": occupants,
                "initial": initial_spec,
                "process": bool(process),
                "src": _source_ref(),
                "scope": scope,
            }
        )
        return SystemHandle(full)

    # -- edges --------------------------------------------------------------

    def connect(self, src: OutPort[F], dst: InPort[F], delayed: bool = False) -> None:
        """Record a component-frame edge from ``src`` to ``dst``. The shared
        frame parameter ``F`` makes a cross-frame connection a pyright error;
        at runtime both ends are :class:`PortRef`s (same ``instance``/``port``
        fields the annotations declare)."""
        self._edge(src, dst, bool(delayed))

    def route(
        self, src: "SystemHandle | Spec", dst: "SystemHandle | Spec", msg: str
    ) -> None:
        """Record a message edge carrying ``msg`` from ``src`` to ``dst``. A
        message edge is log-delivery pub/sub, so it has no ``delayed`` form.
        Endpoints are handles (from :meth:`add`/:meth:`slot`, or
        :attr:`coordinator`); a generated entry's handle is typed as its class
        but is a :class:`SystemHandle` at runtime, so its ``name`` is read
        dynamically."""
        self._edges.append(
            {
                "from": _handle_name(src),
                "out": msg,
                "to": _handle_name(dst),
                "in_": msg,
                "delayed": False,
                "kind": "Msg",
                "src": _source_ref(),
            }
        )

    def _edge(self, src: OutPort[F], dst: InPort[F], delayed: bool) -> None:
        self._edges.append(
            {
                "from": src.instance,
                "out": src.port,
                "to": dst.instance,
                "in_": dst.port,
                "delayed": delayed,
                "kind": "Frame",
                "src": _source_ref(),
            }
        )

    # -- emission -----------------------------------------------------------

    def _clock(self) -> Any:
        if self.sim_dt is not None:
            return {"Simulated": {"dt_secs": float(self.sim_dt)}}
        return "Wall"

    def _program_ir(
        self,
    ) -> tuple[dict[str, Any] | None, list[dict[str, Any]]]:
        """The assembled program blob and the program-built wasm artifact its
        added ``@system`` specs address. The blob carries every captured
        ``Frame``/``State`` class plus the *added* system declarations, in
        definition order — compile order is source order; step order is add
        order; the two are independent. A ``@system`` never added is staged
        code: legal, but warned about, and left out of the program. Offsets
        are byte offsets into the assembled source, what the compiler's spans
        are mapped back through."""
        for entry in _program:
            if entry["system"] and entry["added"] is None:
                src = entry["src"]
                at = (
                    f"{src['file']}:{src['line']}"
                    if src["file"]
                    else f"line {src['line']}"
                )
                print(
                    f"warning: @system `{entry['name']}` ({at}) was never added and "
                    f'will not run; register it with target.add("{entry["name"]}", '
                    f"{entry['name']})",
                    file=sys.stderr,
                )
        included = [e for e in _program if not e["system"] or e["added"] is not None]
        if not any(e["system"] for e in included):
            return None, []
        decls: list[dict[str, Any]] = []
        parts: list[str] = []
        offset = 0
        for entry in included:
            text = entry["source"]
            if not text.endswith("\n"):
                text += "\n"
            text += "\n"
            decls.append({"name": entry["name"], "src": entry["src"], "offset": offset})
            parts.append(text)
            offset += len(text.encode())
        artifact = {
            "id": PROGRAM_ARTIFACT,
            "kind": "wasm",
            "path": None,
            "prebuilt_dir": None,
            "dist": None,
            "manifest_hash": None,
            "src": None,
        }
        return {"source": "".join(parts), "decls": decls}, [artifact]

    def to_ir(self) -> dict[str, Any]:
        """The serialized ``Wiring`` this target describes."""
        program, program_artifacts = self._program_ir()
        return {
            "ir_version": IR_VERSION,
            "metor_config_version": __version__,
            "coordinator": {
                "cycle_rate": self.cycle_rate,
                "default_depth": self.default_depth,
                "clock": self._clock(),
                "namespace": self.namespace,
                "wasm_fuel_per_poll": self.wasm_fuel_per_poll,
                "wasm_memory_limit_bytes": self.wasm_memory_limit_bytes,
            },
            "artifacts": self._artifacts + program_artifacts,
            "states": self._states,
            "systems": self._systems,
            "slots": self._slots,
            "edges": self._edges,
            "scopes": self._scopes,
            "program": program,
        }


def _the_target() -> Target:
    if len(_targets) != 1:
        raise RuntimeError(
            f"exactly one Target must exist at emission, found {len(_targets)}"
        )
    return _targets[0]


def emit(target: Target | None = None) -> None:
    """Write the target IR to ``$METOR_IR_OUT`` (stdout if unset)."""
    ir = (target or _the_target()).to_ir()
    text = json.dumps(ir, indent=2)
    out = os.environ.get("METOR_IR_OUT")
    if out:
        with open(out, "w", encoding="utf-8") as f:
            f.write(text)
    else:
        sys.stdout.write(text)


def _emit_at_exit() -> None:
    # Emit only for a well-formed single-target module; a broken module has
    # already raised, and its traceback is the error surface.
    if len(_targets) == 1:
        emit(_targets[0])


def Presets(presets: list[Preset]) -> Spec:  # noqa: N802 - a system-type wrapper
    """The built-in preset broadcaster, its ``PresetsParams`` carrying one
    entry per preset under the ``preset`` field the Rust struct declares.

    Component references are namespace-relative, like alarm targets: recording
    qualifies them with the target's namespace, so the ids match what the
    target registers. The ``Target`` must therefore exist first — the usual
    ``m.add("presets", Presets([...]))`` order."""
    namespace = _the_target().namespace
    return static_system("Presets", preset=[p.to_json(namespace) for p in presets])


atexit.register(_emit_at_exit)
