"""A target that raises at record time: its traceback is the error surface."""

from metor_config import Target, static_system

m = Target(cycle_rate=100.0)
m.add("a", static_system("Alarms"))
m.add("a", static_system("Alarms"))  # duplicate name -> ValueError
