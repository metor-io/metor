# The metor Python toolchain

One directory per distribution (`docs/design-packaging.md` §4):

- `metor-config/` — the mission-config recorder. A mission file builds a
  `Mission`, adds systems and slots, and connects their ports; at exit the
  recorder serializes the mission to the `Wiring` IR JSON the Rust host
  ingests (`$METOR_IR_OUT`). Stdlib only, CPython 3.10+. The `metor-fsw`
  binary embeds this package as the no-venv fallback (a venv-installed copy
  is preferred); `$METOR_CONFIG_PY` points the host at a live checkout of
  `metor-config/` instead.
- `metor-build/` — the PEP 517/660 build backend pack crates declare.
- `tests/` — the recorder test suite.

```python
from metor_config import Mission, Alarms, Alarm, Target, band, TcpUplink, TcpDownlink

m = Mission(cycle_rate=120.0, sim_dt=1 / 120)
adcs = m.artifact("adcs", crate="adcs-systems", lib="adcs_systems")
plant = m.add("plant", adcs.Plant(init_angle=0.5), process=True)
nav = m.add("nav", adcs.Nav(meas_sigma=0.02))
m.connect(plant.sensors, nav.sensors)
```

## Tests

```
cd libs/metor-fsw-2/python
python3 -m unittest discover -s tests
```

`tests/test_golden.py` asserts the recorder emits exactly
`libs/metor-fsw-2/tests/golden/mission.json` — the same fixture the Rust
`tests/ir_contract.rs` round-trip test consumes, so the JSON contract is checked
from both sides.
