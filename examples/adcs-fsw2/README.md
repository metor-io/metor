# adcs-fsw2 — closed-loop ADCS on metor-fsw-2

A self-contained spacecraft attitude-determination-and-control mission built on the
`metor-fsw-2` framework: a plant/dynamics system, an MEKF navigation filter, and a
Yang-LQR controller in a closed feedback loop (reusing the `metor-fsw/adcs` math and
`nox` six-dof dynamics).

```text
  plant ──sensors──▶ nav ──attitude_estimate──▶ ctrl
    ▲                                             │
    └───────────────── torque_cmd ────────────────┘   (one-cycle-delayed feedback)
```

## Watch it live in metor-panel

`cargo run` paces the loop on a wall clock at 120 Hz and streams every output frame to a
running metor-panel over TCP, in metor-proto's wire format — the same format metor-db
ingests.

1. Start **metor-panel** (it boots a metor-db on `127.0.0.1:2240` and opens the UI):
   ```sh
   cargo run -p metor-panel
   ```
2. In another terminal, run the mission:
   ```sh
   cargo run -p adcs-fsw2
   ```
3. In the panel, the `plant` / `nav` / `ctrl` (and `coordinator`) instances appear in the
   component tree. Plot e.g. `nav.attitude_estimate.q_hat` against `plant.truth.q_true`
   and watch the estimate track truth as the controller drives the spacecraft to the
   target; `plant.sensors.gyro` and `ctrl.torque_cmd.torque` show the rate damping and the
   commanded torque.

The mission converges in ~30 s of real time; the terminal prints the attitude error each
second. Ctrl-C to stop. If the panel isn't running the downlink just fails to connect and
the control loop runs unaffected.

> Component names are prefixed by the **instance** name (`nav.attitude_estimate.q_hat`),
> so two instances of one system type never collide — the metor-fsw-2 output registry
> applies that prefix when announcing each frame's schema to the panel.

## Headless test (no panel needed)

```sh
cargo test -p adcs-fsw2     # asserts the closed loop converges, code-first and via KDL
```
