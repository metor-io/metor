# The metor Python toolchain

One directory per distribution (see `../docs/packaging.md`):

- `metor-config/` — the target-config recorder. A target file builds a
  `Target`, adds systems and slots, and connects their ports; at exit the
  recorder serializes the target to the `Wiring` IR JSON the Rust host
  ingests (`$METOR_IR_OUT`). Stdlib only, CPython 3.10+. The `metor-fsw`
  binary embeds this package as the no-venv fallback (a venv-installed copy
  is preferred); `$METOR_CONFIG_PY` points the host at a live checkout of
  `metor-config/` instead.
- `metor-build/` — the PEP 517/660 build backend pack crates declare.
- `metor-fsw/` — the `metor-fsw` binary as a wheel, with a locator and a
  console script, so a target's venv carries its own host.
- `metor-fsw-abi/` — the pack ABI version marker. Pack wheels pin it so an
  environment cannot combine a pack with a host of another ABI.
- `tests/` — the recorder test suite.

```python
from metor_config import Target
from adcs_pack import Nav, Plant

m = Target(cycle_rate=120.0, sim_dt=1 / 120)
plant = m.add("plant", Plant(init_angle=0.5), process=True)
nav = m.add("nav", Nav(meas_sigma=0.02))
m.connect(plant.sensors, nav.sensors)
```

## Tests

```
cd libs/metor-fsw-2/python
python3 -m unittest discover -s tests
```

`tests/test_golden.py` asserts the recorder emits exactly
`libs/metor-fsw-2/tests/golden/target.json` — the same fixture the Rust
`tests/ir_contract.rs` round-trip test consumes, so the JSON contract is checked
from both sides.
