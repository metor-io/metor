# sysstats-fsw3

Host system statistics on metor-fsw-3. The `stats` pack samples CPU, memory,
network interfaces, processes, and block devices on Linux and macOS, and a db
records them for a panel. It is the first user of `FrameMap`: each per-core,
per-interface, per-process, and per-device set is a keyed map whose record
size tracks the entries written, and interfaces and disks nest a second map
of driver counters.

```
systems/stats/    sysstats-systems-3: the five samplers and pack()
sim.py            one target at 100 Hz with a publish link and a db
tests/            an in-process sampling test and a db test
```

## Systems

| System | Frame | Map key | Linux | macOS |
| --- | --- | --- | --- | --- |
| `cpu` | `Cpu`: totals and `cores` | core index | `/proc/stat` | `host_processor_info` |
| `mem` | `Mem`, fixed | | `/proc/meminfo` | `host_statistics64`, `hw.memsize`, `vm.swapusage` |
| `nics` | `Nics`: `ifaces`, each with `ethtool` | interface, counter | `/proc/net/dev`, `SIOCETHTOOL` | `NET_RT_IFLIST2` |
| `procs` | `Procs`: `pids` | PID | `/proc/<pid>/stat`, `io` | `proc_listallpids`, `proc_pidinfo`, `proc_pid_rusage` |
| `disks` | `Disks`: `devices`, each with `smart` | device, attribute | `/proc/diskstats`, NVMe admin ioctl | IOKit `IOBlockStorageDriver` statistics |

Components are named `<namespace>.<system>.<port>.<map>.<key>.<field>`, for
example `host.nics.nics.ifaces.eth0.ethtool.rx_queue_0_packets` or
`host.procs.procs.pids.4211.cpu_util`. A key's `.` becomes `_`.

The samplers are async systems. Each runs on the thread its config names
(`stats` in `sim.py`) and sets its own rate with `rate_hz`, so a slow read
never stretches a cycle. `sim.py` reads CPU, memory, and interfaces at 100 Hz,
processes at 10 Hz, and disks at 1 Hz.

- `cpu` reports cumulative ticks per state and a utilization averaged over
  `util_tau_s` (1 s), since a 10 ms window holds about one tick per core.
- `nics` writes at most `max_nics` interfaces and `max_ethtool` driver counters
  each. Counter names are read once per interface; an interface whose driver
  has no ethtool stats gets an empty map.
- `procs` writes the `max_procs` processes that used the most CPU since the
  previous sample, in PID order, and `total` counts every process read. Each
  entry carries its command name as a `FrameStr<16>`.
- `disks` reads the NVMe SMART / Health log page of each `nvme<N>n<M>`
  namespace when `/dev/<name>` opens, which needs root or `CAP_SYS_ADMIN`.
  Otherwise, and on every other device, `smart` stays empty.

## Platform gaps

- macOS has no ethtool, so every `ethtool` map is empty.
- macOS exposes SMART health only through private NVMe interfaces, so every
  `smart` map is empty. `in_flight` and `io_ns` read zero.
- Without root, macOS lets `proc_pidinfo` read only this user's processes.
  Linux withholds `/proc/<pid>/io` for other users' processes, whose IO
  counters read zero.
- `mem` fields a platform does not report read zero: `buffers` on macOS,
  `compressed` on Linux without zswap.

## Allocation

Buffers are sized at init and reused. A sample allocates only when a device
or process count outgrows its reservation, when a new interface or device
appears, and on macOS in IOKit and CoreFoundation, whose property reads copy
on every disk sample.

## Running

```
cargo run -p metor-fsw-3 --bin metor -- run examples/sysstats-fsw3/sim.py
```

`--cycles N` stops after N cycles. The target publishes on `[::]:2240` and its
db serves a panel on `0.0.0.0:2250`. `METOR_SYSSTATS_PORTS=pub,db` overrides
both.

## Tests

`cargo test -p sysstats-systems-3` runs the parser unit tests, which run on
any OS. `cargo test -p sysstats-fsw3` runs two integration tests on the
current host: `tests/sample.rs` runs the samplers in process for 300 cycles
and checks that each map fills, that nothing logged a fault, and that the
loop kept its rate; `tests/db.rs` runs `sim.py` under `metor run` and checks
that the db names a component for each kind of map entry.
