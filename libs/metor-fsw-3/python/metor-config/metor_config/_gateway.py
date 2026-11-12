"""`gateway`, a composite that records other targets' telemetry."""

from __future__ import annotations

from typing import NamedTuple, Sequence

from ._builtins import Db, Peer, Subscribe, _namespace
from ._composite import Scope, composite
from ._target import SystemHandle


class Gateway(NamedTuple):
    """What `gateway` returns."""

    db: Db


@composite
def gateway(
    s: Scope,
    *,
    peers: Sequence[SystemHandle],
    listen: str,
    path: str | None = None,
    keep: bool | None = None,
) -> Gateway:
    """Records telemetry from other targets.

    Subscribes to each link in ``peers``, named for its target's namespace,
    and adds a `Db` on the thread ``db``, which clients reach on ``listen``.
    The `Db` records every system added before it on the target, not only
    the subscribers. ``keep`` says whether the db survives the run
    stopping; by default it is deleted unless ``path`` is given.
    """
    for peer in peers:
        s.add(_namespace(peer), Subscribe(transport=Peer(peer)))
    db = s.add("db", Db(listen=listen, path=path, all=True, keep=keep), thread="db")
    return Gateway(db)
