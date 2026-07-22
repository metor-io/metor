# Coordinator

The coordinator owns a ready system graph and runs it. Build fixes the graph, ring sizes, reader counts, and port bindings before the first system starts.

The coordinator turns a set of systems into one ordered mission. It gives each
cycle one time, runs systems in a known order, starts and stops the mission as
a unit, and reports failures that affect the whole run.

## A cycle at a glance

Cyclic systems run in mission order. A system can publish a value for a later
system to use in the same cycle.

```text
cycle order: sensor -> filter -> control

sensor writes sample N
filter reads sample N and writes estimate N
control reads estimate N
```

Every cyclic system receives the same timestamp for that cycle. After the
cyclic steps, the coordinator copies new state to free-running async systems,
updates mission status, and either waits for the next wall-clock cycle or
yields in simulated mode.

A backward state edge must be marked as delayed. The consumer then uses the
value from an earlier cycle. This makes feedback timing part of the mission
design instead of an effect of system order.

## From mission to run graph

Two front ends create the same `Wiring` IR:

- a Rust `WiringBuilder`
- an evaluated `mission.py`

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

Node zero is always the coordinator. It owns health, log, status, sequence registry, and command outputs. A mission loaded through the wiring front end also adds a wiring manifest output.

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

Message log edges do not form same-cycle data needs, so they do not take part in this check. Edges with an async endpoint are also outside the cyclic order check.

## Ring allocation

Each output port gets one ring and one writer. Its frame or message size bound and delivery rule set the ring depth.

A frame input reads the producer's ring. A message input gets one view for each producer ring.

Each ring has a fixed reader count. Build includes:

- one reader for each edge
- self-tap readers
- one reader for each `AllOutputs` grant
- spare readers from `CoordinatorConfig::reader_slack`

A later registry view claim fails if no reader slot remains.

## Async copy-in

A free-running async system receives each snapshot input through a private ring. Build creates a matched wake endpoint for that ring.

After cyclic systems step, the coordinator checks the source ring. It copies the newest record only when the source has a new commit.

If the private ring is full, the coordinator skips the copy. It never waits inside the cycle. A later cycle tries the latest source record.

Message inputs do not need a private ring. The async task drains their producer rings.

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
publish it as mission time
handle registry reload requests
step cyclic slots in order
copy new snapshots to async inputs
update coordinator status if state changed
drain host tracing logs
sleep for wall-clock pace, or yield in simulated mode
```

Every cyclic system receives the same timestamp for that cycle.

Slots drain their own command inputs at the start of their step. A command can affect a slot in the cycle where the slot reads it.

## Clocks

Wall mode reads the current time at the start of each cycle. The coordinator sleeps for the rest of the cycle budget. If work uses the full budget, it reports `cycle_overrun` health data.

Simulated mode starts from one epoch. Cycle `k` uses `epoch + k * dt`. It does not sleep, but it yields once per cycle so spawned tasks can run.

Systems should stamp cycle data with the supplied `now` value. Reading wall time inside a cyclic system breaks simulated time rules.

## Status and coordinator health

The status frame lists hard-stopped cyclic systems and process worker state.

The coordinator sends a new status frame when the stopped set or worker facts change. It does not send the same status on every cycle.

Coordinator health counts host faults such as:

- process step timeouts
- process restarts
- stopped systems
- status write failures
- reload input corruption
- cycle overruns
- async shutdown timeouts

The current run loop closes a coordinator health cycle after a worker event, a stopped-set change, a host log queue drop, or a cycle overrun. Other error calls stay in the health state until a later close. An error near the end of shutdown may not reach a health record.

The coordinator health cycle count does not equal the mission cycle count.

## Shutdown

Shutdown first signals all free-running async systems to stop.

The coordinator gives them a short time to return from `run` and complete `shutdown`. It force-cancels a task that misses the limit and reports `async_shutdown_timeout`.

It then calls cyclic shutdown in reverse registration order. Last, it drains any tracing events made during shutdown.

Build a new coordinator for another run. The first run consumes async systems and their run-time links.
