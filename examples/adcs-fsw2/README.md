# adcs-fsw2 — closed-loop ADCS on metor-fsw-2, with `dlopen`'d systems

A self-contained spacecraft attitude-determination-and-control mission built on the
`metor-fsw-2` framework: a plant/dynamics system, an MEKF navigation filter, and a
Yang-LQR controller in a closed feedback loop (reusing the `metor-fsw/adcs` math and
`nox` six-dof dynamics).

```text
  plant ──sensors──▶ nav ──attitude_estimate──▶ ctrl
    ▲                                             │
    └───────────────── torque_cmd ────────────────┘   (one-cycle-delayed feedback)
```

The three systems are **not** statically linked into the mission binary — each is its own
`dlopen`-loadable `cdylib` (WP8 dl-open, `docs/dl-open.md` §8). The host describes the
mission as a `Wiring`, builds the three `.so`s with the build driver, and `dlopen`s them
into a coordinator. Edit a control law, `cargo build -p adcs-ctrl` (a small crate), and
re-run — without recompiling the host or the telemetry stack.

## Crate layout

| Crate | Path | Role |
|---|---|---|
| `adcs-contracts` | `contracts/` | The shared compile-time contract: the four frame structs + a `Params` struct per system. Linked by the cdylibs (and the test), **not** by the host. |
| `adcs-plant` | `systems/plant/` | The rigid-body plant, a `cdylib` ending in `export_system!(PlantSystem)`. |
| `adcs-nav` | `systems/nav/` | The MEKF filter cdylib. |
| `adcs-ctrl` | `systems/ctrl/` | The Yang-LQR controller cdylib. |
| `adcs-fsw2` | (this crate) | The mission **host**: builds + `dlopen`s the three cdylibs and runs the coordinator. Links only `metor-fsw-2` — it is fully schema-agnostic (frames validated from serialized VTables, params encoded from each `.so`'s exported schema). |

Each system crate is `crate-type = ["cdylib", "rlib"]`: the cdylib is what the host loads;
the rlib lets the convergence test also link the systems statically for the parity check.
The `export_system!` C-ABI symbols ride an `export` feature (on by default for the cdylib,
off when the test links the rlib) so three system rlibs link into one test binary without a
duplicate `fsw_*` symbol clash.

## Watch it live in metor-panel

`cargo run` first builds the three system cdylibs (incremental — only changed crates
recompile), `dlopen`s them, then paces the loop on a wall clock at 120 Hz and streams every
output frame to a running metor-panel over TCP, in metor-proto's wire format — the same
format metor-db ingests.

1. Start **metor-panel** (it boots a metor-db on `127.0.0.1:2240` and opens the UI):
   ```sh
   cargo run -p metor-panel
   ```
2. In another terminal, run the mission (this builds the cdylibs, then loads them):
   ```sh
   cargo run -p adcs-fsw2
   ```
3. In the panel, the `plant` / `nav` / `ctrl` (and `coordinator`) instances appear in the
   component tree. Plot e.g. `nav.attitude_estimate.q_hat` against `plant.truth.q_true`
   and watch the estimate track truth as the controller drives the spacecraft to the
   target; `plant.sensors.gyro` and `ctrl.torque_cmd.torque` show the rate damping and the
   commanded torque.

The mission converges in ~30 s of real time. The terminal prints only a heartbeat — the
host stays schema-agnostic (it doesn't decode the `truth` frame), so convergence is watched
in the panel (or asserted headlessly by `cargo test`). Ctrl-C to stop. If the panel isn't
running the downlink just fails to connect and the control loop runs unaffected.

> Component names are prefixed by the **instance** name (`nav.attitude_estimate.q_hat`),
> so two instances of one system type never collide — the metor-fsw-2 output registry
> applies that prefix when announcing each frame's schema to the panel.

## Headless test (no panel needed)

```sh
cargo test -p adcs-fsw2     # builds the cdylibs, then asserts convergence
```

The test runs the loop **two** ways — the systems statically linked (rlibs) and the same
systems `dlopen`'d from their cdylibs — and asserts both converge to the **same** attitude-
error / body-rate envelope (bit-identical: same systems, same params, same seed, just loaded
vs linked). It builds the cdylibs in-process via the build driver, so it is slower than a
plain unit test.
