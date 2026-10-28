# metor-code-stand

A test stand whose control code is metor-code. `stand.mc` holds the leak
check from the metor-code design (pressurise to 150 psi, hold five minutes,
abort above 165 psi) and a heater controller. The Rust `stand_io` pack holds
a simulated chamber and a monitor that prints what happens; `stand.mc`
imports its frames with `use frames from "stand_io"`.

```
stand.mc              the leak_check sequence and the heater system
systems/stand-io/     stand_io: chamber_sim, monitor, and the frames
target.py             one target wiring them together
```

## Running

From the repo root:

```
cargo build -p metor-fsw-3 --bin metor -p metor-code --bin metor-code
target/debug/metor code build examples/metor-code-stand/stand.mc
target/debug/metor run examples/metor-code-stand/target.py --sim-dt 0.01 --cycles 32000
```

`metor code build` builds `stand_io` with `metor pack dev`, compiles
`stand.mc` with Cranelift, links it with the mc-rt runtime into
`.metor/stand/_libs/<triple>/libstand.dylib` (or `.so`), and writes the
`stand` Python module beside it. `metor run` then loads both packs like any
others. The run steps 320 s of simulated time in a second or two:

```
[    0.00 s] leak_check running (chamber 14.7 psi)
[    0.00 s] valve opened at 14.7 psi
[   13.54 s] valve closed at 150.1 psi
[  313.54 s] leak_check completed (chamber 150.1 psi)
```

`STAND_SCENARIO` changes the plant:

- `leak`: the chamber leaks, so the check ends `failed` after `on abort:`
  closes the valve.
- `surge`: the chamber jumps past 165 psi in one cycle, so the redline trips
  while the valve is open and `on abort:` closes it.
- `stuck`: the valve sticks open after the check closes it, and the redline
  trips during the hold.

`STAND_REPORT` sets the seconds between chamber reports. With
`STAND_PANEL=1` the target also runs a db on `127.0.0.1:2250`, so a panel's
sequence control can start, abort and reset `leak_check`; `cmds` takes
`SequenceCommand`s on `127.0.0.1:2241`. Run it on the wall clock for that
(drop `--sim-dt`).
