"""A mission that raises at record time: its traceback is the error surface."""

from metor_config import Mission, static_system

m = Mission(cycle_rate=100.0)
m.add("a", static_system("Alarms"))
m.add("a", static_system("Alarms"))  # duplicate name -> ValueError
