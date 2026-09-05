# Coordinator

The coordinator owns a ready system graph and runs it. Build fixes the graph, ring sizes, reader counts, and port bindings before the first system starts.

The coordinator turns a set of systems into one ordered target. It gives each
cycle one time, runs systems in a known order, starts and stops the target as
a unit, and reports failures that affect the whole run.

## A cycle at a glance

Cyclic systems run in wiring order. A system can publish a value for a later
system to use in the same cycle.

```text
cycle order: sensor -> filter -> control

sensor writes sample N
filter reads sample N and writes estimate N
control reads estimate N
```

Every cyclic system receives the same timestamp for that cycle. After the
cyclic steps, the coordinator copies new state to free-running async systems,
updates target status, and either waits for the next wall-clock cycle or
yields in simulated mode.

A backward state edge must be marked as delayed. The consumer then uses the
value from an earlier cycle. This makes feedback timing part of the target
design instead of an effect of system order.

## From target to run graph

Two front ends create the same `Wiring` IR:

- a Rust `WiringBuilder`
- an evaluated `target.py`

The shared resolver checks the IR and builds an `InitGraph`. The init graph stores systems, slots, edges, and run config as plain host data.

Build then runs these main passes:

1. Check config, names, systems, and edges.
2. Resolve each edge to one output and one input port.
3. Count readers and plan input fan-in.
4. Allocate output rings and host input rings.
5. Build the output registry.
6. Plan and allocate private async input rings.
7. Bind each system's typed ports in descriptor order.
8. Return a ready `Coordinator`.

No data flows before these passes finish.

## Node and port order

Node zero is always the coordinator. It owns `system_status`, log, status, sequence registry, and command outputs. A target loaded through the wiring front end also adds a wiring manifest output.

User system order has two roles:

- it is registration order
- it is cyclic step order

The build does not sort cyclic systems after registration.

Port order is also fixed. A descriptor lists input and output ports. Allocation records rings in that order. Generated bind code takes rings in that same order.

This rule lets the binder stay type erased. It also means a custom descriptor and custom bind implementation must use the same order.

## Edge checks

A frame output and input must use the same frame id and delivery rule. The input's component set may be a subset of the output's set. Shared components must have the same type and shape.

A message edge must use the same packet id and delivery rule.

Frame snapshot inputs take one producer. Message log inputs may take zero or more producers.

The build rejects missing required frame inputs, duplicate names, bad host ports, bad fan-in, and incompatible schemas.

## Delayed state and feedback

A snapshot edge that points backward would read an old value. Build rejects it unless the edge is marked delayed.

```text
cycle order: control -> plant

plant -> control is delayed
control reads the prior plant state
plant then writes the next state
```

Each feedback loop must contain a delayed edge. The run-time ring path stays the same. The delayed mark tells validation that the old value is intended.

Message log edges do not form same-cycle data needs, so they do not take part
in this check. Async import/export boundaries do: their registration position
defines when inputs are sampled and outputs become visible.

## Ring allocation

Each output port gets one ring and one writer. Its frame or message size bound and delivery rule set the ring depth.

A frame input reads the producer's ring. A message input gets one view for each producer ring.

Each ring has a fixed reader count. Build includes:

- one reader for each edge
- self-tap readers
- one reader for each `AllOutputs` grant
- spare readers from `CoordinatorConfig::reader_slack`

A later registry view claim fails if no reader slot remains.

## Async boundaries

A free-running async system binds only private rings. Its graph position holds
a boundary that imports all inputs and then exports outputs completed before
that point. Snapshot ports copy the newest changed record; log ports drain all
pending records. No copy waits for space.

The task is local and cooperative, so an import wake cannot run it before the
immediately following export. Input sampled on cycle N can therefore first
produce graph-visible output on cycle N+1.

## Start

`Coordinator::run_for` may run only once on a coordinator.

Start uses a barrier:

1. Spawn each free-running async system.
2. Run each async system's `init` in its task.
3. Wait until all async init calls finish.
4. Run cyclic init calls in registration order.
5. Release async tasks into their `run` methods.
6. Emit boot data such as the sequence registry and wiring manifest.

No system enters its main run method before all init calls finish.

## One cycle

Each cycle does this work:

```text
choose one timestamp
publish it as FSW time
handle registry reload requests
step cyclic slots and async import/export boundaries in order
update coordinator status if state changed
drain host tracing logs
sleep for wall-clock pace, or yield in simulated mode
```

Every cyclic system receives the same timestamp for that cycle.

Slots drain their own command inputs at the start of their step. A command can affect a slot in the cycle where the slot reads it.

## Clocks

Wall mode reads the current time at the start of each cycle. The coordinator sleeps for the rest of the cycle budget. If work uses the full budget, it logs a `cycle_overrun` fault and yields so async tasks still make progress.

Simulated mode starts from one epoch. Cycle `k` uses `epoch + k * dt`. It does not sleep, but it yields once per cycle so spawned tasks can run.

Systems should stamp cycle data with the supplied `now` value. Reading wall time inside a cyclic system breaks simulated time rules.

## Status, system status, and the coordinator log

The status frame (`coordinator_status`) lists hard-stopped cyclic systems and process worker state. The coordinator sends a new one when the stopped set or worker facts change, not every cycle.

Every registered system also gets a `system_status` frame the coordinator writes for it: after each `step` it publishes the slot's cycle count, how long that step took, and the slot's run-state code. See [System status](system.md#status). The coordinator's own record closes once per cycle after every slot has stepped, so its `cycles` equals the FSW cycle count and `last_execute_us` is the time the whole graph took to step (the sleep that pads out a wall-clock cycle is excluded). Shutdown closes one final record.

Host-observed faults land on the coordinator's log as `kind=` lines, one per affected cycle:

- `proc_step_timeout` and `proc_restart`
- `async_boundary_dropped`, `wasm_boundary_dropped`, and `boundary_corrupt`
- `system_stopped`
- `status_publish_failed` when a status reader backpressures a slot or the coordinator; the slot continues running
- `reload_input_corrupt`
- `cycle_overrun`
- `async_shutdown_timeout`
- `log_dropped` for a tracing forward queue that overflowed

## Shutdown

Shutdown first signals all free-running async systems to stop.

The coordinator gives them a short time to return from `run` and complete `shutdown`. It force-cancels a task that misses the limit and reports `async_shutdown_timeout`.

It then calls cyclic shutdown in reverse registration order. Last, it drains any tracing events made during shutdown.

Build a new coordinator for another run. The first run consumes async systems and their run-time links.
