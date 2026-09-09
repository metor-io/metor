"""Group the targets one file configures, builds, and runs together."""

from __future__ import annotations

from typing import Any
from ._version import __version__, IR_VERSION
from ._program import _warn_unadded
from ._target import Target


_deployments: list["Deployment"] = []


class Deployment:
    """The targets one ``target.py`` declares. One may exist per file; a file
    that constructs a bare :class:`Target` and no deployment emits a
    deployment of one.

    Members are listed in the order given — the order the IR carries them and
    the order ``metor-fsw build`` provisions them. A member's identity is its
    ``namespace``, so a deployment of more than one target requires every
    member to carry one (checked on the Rust side, with the rest of the
    namespace rules).
    """

    def __init__(self, targets: list[Target]):
        if _deployments:
            raise RuntimeError("exactly one Deployment may exist in a target file")
        for target in targets:
            if not isinstance(target, Target):
                raise TypeError(
                    f"a Deployment member must be a Target, found "
                    f"`{type(target).__name__}`"
                )
        if len({id(t) for t in targets}) != len(targets):
            raise ValueError("a Target may belong to one deployment, and once")
        self.targets = list(targets)
        _deployments.append(self)

    def to_ir(self) -> dict[str, Any]:
        """The serialized deployment: the emitter's version and one member
        ``Wiring`` per target, in the order given."""
        _warn_unadded()
        return {
            "ir_version": IR_VERSION,
            "metor_config_version": __version__,
            "targets": [t.to_ir() for t in self.targets],
        }
