# Process systems and process slots

A process system runs a pack entry outside the coordinator process. It uses the
same pack ABI and the same ring data as an in-process loaded system.

If the worker crashes, panics, or misses a deadline, the coordinator can report
the fault and keep running other systems. Process mode adds startup cost and
worker cleanup, so it is a choice for systems that need this isolation.

A process slot applies the same isolation to a runtime-selected occupant. Each
load starts a new worker for the selected entry.

Use process mode when a system must not share the coordinator's address space.
The main reasons are fault isolation and a clear step deadline. Keep a system
in process when low start cost and simple cleanup matter more.

## Use process mode

Set `process=True` when adding a system from a pack:

```python
plant = m.add("plant", Plant(seed=4), process=True)
```

The system keeps the same params, ports, and place in the cycle. The host runs
its pack entry in a worker instead of loading it into the coordinator process.

The system must come from an artifact. A static registry system has no pack
library for a worker to open.

## Platform support

Process mode needs a wait primitive that works across processes. It is built on
Linux and macOS. macOS hosts need 14.4 or later.

Other targets keep `proc::worker_entry()` as a no-op. A graph with a process
system fails at build time.

## Worker entry point

The framework re-runs the host executable as its worker. Any binary that embeds
the framework and uses process mode must call this first:

```rust
fn main() {
    metor_fsw_2::proc::worker_entry();
    // normal host setup
}
```

When `METOR_FSW_WORKER` is not set, the call returns at once. When it is set,
the call reads the worker manifest, runs worker mode, and exits without running
the rest of `main`.

The `metor-fsw` CLI installs this guard.

## Two worker modes

A postcard `WorkerManifest` selects one of two modes.

Describe mode opens a pack, reads its manifest bytes, writes them to a file,
and exits. The coordinator uses this mode while it resolves a process system.
The coordinator process does not load that pack.

Run mode opens a pack entry and drives it until shutdown. Its manifest names
the artifact, entry, params, instance, control file, input ring files, and
output ring files.

## Shared files

One coordinator run owns a session directory. It uses `/dev/shm` when that
directory exists, and the OS temp directory otherwise.

The session contains:

- one mmap file for each shared ring
- one control file per worker
- one launch manifest per worker
- short-lived files used by describe workers

The coordinator removes the session on drop as a best effort.

## Control protocol

The control file holds a fixed header and atomic words. The header has a magic
value, layout version, and architecture tag.

The host and worker follow this order:

```text
host                                      worker
create control, spawn                 -> attach files, open pack, create
wait for Attached                     <- report Attached
request Init                           -> bind rings and init
wait for Ready                        <- report Ready
send sequence and timestamp           -> execute one step
wait for matching ack                 <- report status and ack
request Shutdown                      -> shutdown and destroy
wait for Done                         <- report Done and exit
```

The host sends one timestamp for each coordinator cycle. The worker serves the
newest sequence it sees and acks that sequence.

If a worker misses several doorbells, it skips old steps. A late ack from an
old step does not satisfy the next wait.

## Deadlines

The first worker attach may take up to 10 seconds. Init may also take up to 10
seconds. Clean shutdown gets a 1 second grace period before the host kills the
child.

Each steady-state step has its own deadline. The default is 100 ms. A live but
late worker adds a coordinator health error and the main loop continues.

The timeout does not cancel code already running in the child. The next
doorbell can replace the missed step.

## Fixed process system behavior

Resolve starts a describe worker, checks the returned descriptor, encodes
params, and adds a process node. The run worker starts when the graph builds.

The first spawn blocks until the worker reports `Attached`. Later restarts do
not block the cycle loop.

## Restart policy

A fixed process system can restart after its worker dies or reports a panic.
The defaults are:

- at most 3 restart attempts
- 500 ms before each attempt
- 100 ms for each step ack

`ResolveOptions` can change the worker executable, session root, step timeout,
restart count, and restart delay. These are host choices and do not enter the
portable IR.

A restart has these stages:

```text
stop -> reap -> reclaim ring roles -> delay -> spawn -> attach -> init -> run
```

Each attempt uses one unit of the restart limit. A failure during attach or init
can start another attempt while the limit remains.

The new worker's input views start at the current ring positions. It skips data
written during the outage.

## Process slots

A slot can put every allowed occupant in process mode:

```python
mode = m.slot(
    "mode",
    inputs=["estimate"],
    outputs=["mode_cmd"],
    allow=[commissioning(), safe_mode()],
    initial="commissioning",
    initial_state="running",
    process=True,
)
```

Resolve describes each allowed pack without loading it in the host. Every
occupant must match the slot's port contract and must be reloadable.

Each Load starts a new worker. Load has an attach stage and an init stage. The
slot reports `Loading` until both finish.

A process slot does not auto-restart a failed occupant. A sequence may perform
actions that must not run twice without an operator command.

Stop, Reset, Unload, and a new Load end the old worker first. The host kills and
reaps it, then reclaims its ring roles before another worker can bind.

## Step status

The worker sends the raw ABI status in its ack:

- `Running` keeps the entry active.
- `Done` ends a slot task without an error.
- `Panicked` marks the entry as stopped.

For a slot task, the worker latches `Done`. Extra doorbells return `Done`
without polling the finished future.

A panic causes the worker to destroy its foreign state. The host then applies
the fixed-system restart rule or the slot failure rule.

## Cleanup after failure

A worker can die while it owns a writer or reader role in a ring. The host
keeps handles for every ring that worker attached.

The host reaps the child, then reclaims roles owned by its process id. Reaping
comes first so the child cannot write to the ring during reclaim.

This cleanup prevents a dead reader from keeping a ring full and prevents a
dead writer claim from blocking the next worker.

## Dedicated worker executables

By default, the host re-runs itself. `ResolveOptions::worker_exe` can name a
small worker binary instead.

The chosen binary must use the same framework build and must call
`worker_entry()` first. ABI, control layout, and ring architecture checks reject
mixed or incompatible builds.
