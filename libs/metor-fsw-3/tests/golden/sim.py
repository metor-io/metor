"""A GPS driver on a simulated serial port, served by a scripted model."""

from echo_pack import Gps, UartScript
from metor_config import SimUart, Target

GGA = "$GPGGA,123519,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,*47\r\n"

fsw = Target(cycle_rate=100.0, sim_dt=0.01)
gps1 = fsw.resource("gps1", SimUart())
fsw.add("gps", Gps(uart=gps1))
fsw.sim("receiver", UartScript(uart=gps1, script=[GGA, GGA, GGA]))
