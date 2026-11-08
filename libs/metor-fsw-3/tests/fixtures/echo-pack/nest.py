"""The fixture's nested inputs: `tag` producers at different rates feed one
`gather`, whose `tags` is an array and `pairs` an array of structs.

`METOR_NEST_SWAP` set wires `tags.0` and `tags.1` the wrong way round.
"""

import os

from echo_pack import Gather, Tag
from metor_config import Target

fsw = Target(cycle_rate=1000.0)


def tag(n: int, every: int) -> Tag:
    return fsw.add(f"tag_{n}", Tag(tag=n, every=every))


tags = [tag(n, every=n + 1) for n in range(3)]
if os.environ.get("METOR_NEST_SWAP"):
    tags[0], tags[1] = tags[1], tags[0]
pairs = [
    Gather.Pairs(left=tag(10 * i, every=i + 2).output, right=tag(10 * i + 1, every=3).output)
    for i in (1, 2)
]
fsw.add("gather", Gather(tags=[t.output for t in tags], pairs=pairs))
