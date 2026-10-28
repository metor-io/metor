# The stand's control code: a leak check and a heater controller.
#
# `metor code build stand.mc` compiles this file into the `stand` pack, a
# shared library with its Python module under `.metor/stand/`.

use frames from "stand_io"              # Chamber, ValveCmd, HeaterCmd

sequence leak_check(target = 150.0, hold = 5 min):
    "Pressurises the chamber to `target` psi, holds it, and fails if it leaked."
    in chamber: Chamber
    out valve: ValveCmd

    redline "chamber over": chamber.psi > 165.0
    on abort:
        valve.open = False

    valve = ValveCmd(open = True)
    wait until chamber.psi >= target timeout 30.0
    valve.open = False
    wait hold
    if chamber.psi < target - 2.0:
        fail "leak"

system heater(setpoint = 40.0, kp = 0.2, ki = 0.01):
    "Holds the chamber wall at `setpoint` degrees C."
    in chamber: Chamber
    out heater: HeaterCmd
    integ = 0.0
    alert overtemp critical: chamber.temp > 80.0 for 2.0
    loop:
        err = setpoint - chamber.temp
        integ = clamp(integ + err * dt, -10.0, 10.0)
        heater.duty = clamp(kp * err + ki * integ, 0.0, 1.0)
