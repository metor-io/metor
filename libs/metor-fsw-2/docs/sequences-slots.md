# Sequences, tasks, and slots

In FSW, we often want to repersent tasks that span across multiple cycles. For instance, you might want to write a system that powers on a system, by turning on one sub-system at a time. It is awkward to write these with purely cyclic systems. To make this easier, we added tasks and sequences.

A task is an future that we advance once per cycle.  Futures end up being a very good way of repersenting this type of work. You can easily wait for a value to change, sleep, or wait for the next cycle. They let you write what would other wise be a complex state-machine, in simple linear code. A sequence is a type of task that can finish, rather than running forever

A task can run as a normal wired system. It can also run as the occupant of a
runtime slot. The same task body works in both cases, but a slot adds commands,
progress, and status.

A slot lets the target choose or replace one allowed behavior at run time. The goal is to let operators choose between different modes of operation at runtime. For example you might want to first power on the device and then you might want to run a built-in-test. You can equate this to scripts in other fsw systems.

A slot can also hold a cyclic pack entry. Async tasks get cooperative cancel. Cyclic occupants stop on cancel without running async cleanup.

## Write a task

A task takes ports by value and returns `Outcome`:

```rust
use core::time::Duration;
use metor_fsw_2::sequence::{now, progress, wait};
use metor_fsw_2::{Input, Outcome, Output, Pack};

async fn deploy(
    mut sensor: Input<SensorState>,
    mut command: Output<DeployCommand>,
) -> Outcome {
    progress("checking sensor");

    if wait(Duration::from_secs(2)).await.aborted() {
        progress("cancelled");
        return Outcome::Aborted;
    }

    command.publish(&DeployCommand::new(now()));
    Outcome::Completed
}

pub fn pack() -> Pack {
    Pack::new().task("deploy", deploy)
}
```

The future owns its ports and local variables. No task runs on a timer thread. The host polls it once for each FSW cycle with a no-op waker.

The task must return one of these results:

- `Outcome::Completed`
- `Outcome::Aborted`
- `Outcome::Failed`

After a task returns, its driver does not poll it again.

## FSW time

Task helpers read the current poll's `CycleClock`:

- `now()` returns the timestamp for this FSW cycle.
- `wait(duration)` waits for FSW time, not wall time.
- `cycle()` waits until a later poll has a greater timestamp.
- `check(pred, hold, timeout)` waits for a condition (see [Conditions](#conditions)).
- `progress(text)` adds a status line for this cycle.
- `aborted()` reads the latched cancel flag.

These functions require a task poll. They panic if code calls them outside that context.

`wait` fixes its deadline when called. It returns `Step::Elapsed` when `now` reaches the deadline. It returns `Step::Aborted` when slot cancel has arrived.

`cycle()` does not check cancel. A loop that needs cancel should check after the await:

```rust
loop {
    let timestamp = cycle().await;
    if aborted() {
        return Outcome::Aborted;
    }

    publish_for_cycle(timestamp);
}
```

A repeated timestamp does not complete `cycle()`. Normal simulated clocks use a nonzero step.

## Conditions

Most task phases have the same shape: hold a condition for a dwell, and give up after a budget. `check` is that shape as one suspension point.

```rust
use metor_fsw_2::sequence::{check, Check};

let outcome = check(
    || sensor.latest().is_ok_and(|s| s.is_some_and(|s| s.temp_c > 60.0)),
    Duration::from_secs(2),   // hold: the condition must stay true this long
    Duration::from_secs(30),  // timeout: the budget for the whole phase
).await;
```

The predicate runs once per cycle, starting with the cycle `check` is called on. A condition that is already true resolves without suspending when the dwell is `Duration::ZERO`. A cycle where the predicate goes false restarts the dwell. The budget runs from the call, not from when the condition first went true, so a dwell that cannot finish inside the budget times out.

`check` returns one of:

- `Check::Held` — the condition held for the dwell
- `Check::TimedOut` — the budget ran out first
- `Check::Aborted` — slot cancel arrived, which wins over both

Spell "no timeout" as a very large `Duration`. The deadline saturates rather than overflowing.

### One safing site

`Check::or_fail` maps those three onto `?`. Write the phases in a helper that returns `Result<(), Outcome>` and the task body keeps a single place to safe the vehicle:

```rust
async fn deploy(mut sensor: Input<SensorState>, mut command: Output<DeployCommand>) -> Outcome {
    match phases(&mut sensor, &mut command).await {
        Ok(()) => Outcome::Completed,
        Err(outcome) => {
            command.publish(&DeployCommand::safe(now()));
            outcome
        }
    }
}

async fn phases(
    sensor: &mut Input<SensorState>,
    command: &mut Output<DeployCommand>,
) -> Result<(), Outcome> {
    progress("warming");
    check(|| warm(sensor), Duration::from_secs(2), Duration::from_secs(30))
        .await
        .or_fail("warm-up")?;

    command.publish(&DeployCommand::release(now()));
    check(|| released(sensor), Duration::ZERO, Duration::from_secs(5))
        .await
        .or_fail("release")?;
    Ok(())
}
```

A timeout adds a `timeout in <phase>` progress line and fails the task. A cancel returns `Outcome::Aborted`. Either way the safing publish happens once, in the `Err` arm, instead of at every suspension point.

## Wired tasks

A wired task is a normal cyclic entry. Its declared ports connect through target edges. It also has the usual health and log outputs.

It has no slot command input and no `SequenceStatus` output. Calls to `progress` become `Info` log messages after each poll.

No slot can abort a wired task. It may still return `Aborted` by its own rules.

This form suits long-lived async state code that needs one poll per cycle but does not need runtime replacement.

## Slot mounts

A slot is one fixed place in the cyclic schedule. The coordinator owns its rings for the whole run. Loading an occupant attaches fresh port handles to those rings.

Every allowed occupant must have the same user-port contract. The config lists that contract by frame name. Build checks each candidate before the target starts.

Every entry already has health and log outputs after its user outputs. A slot mount then adds two more ports:

- `SlotControlIn` after the user inputs, written by the slot host
- `SequenceStatus` after the health and log outputs, read by the slot host

The slot itself also has a `SequenceCommand` fan-in, a `slot_status` frame, and a `sequences` event output.

An async occupant publishes one `SequenceStatus` after each poll. It contains the terminal run-state code and up to 16 progress lines. Each line holds at most 64 bytes.

The slot turns those lines into ordered `SequenceChannelEvent::Progress` messages. It sends a final completed, aborted, or failed event after the last progress lines.

## Configure a slot

This Python example allows two tasks with the same ports:

```python
mode = m.slot(
    "mode",
    inputs=["attitude_estimate", "gps"],
    outputs=["mode_cmd"],
    allow=[
        commissioning(timeout_s=30.0),
        safe_mode(),
    ],
    initial="commissioning",
    initial_state="running",
)

m.connect(nav.attitude_estimate, mode.attitude_estimate)
m.connect(plant.gps, mode.gps)
m.connect(mode.mode_cmd, controller.mode_cmd, delayed=True)
```

`allow` must not be empty. `initial` must name an allowed entry. Initial state can be `empty`, `loaded`, or `running`. The `empty` state starts without loading the named entry.

All candidates need the same input and output descriptors in the same order. An occupant must still declare a contract port even if its own code does not use that value.

The slot name is its command address and telemetry prefix. Names may hold at most 48 bytes.

## Command route

The uplink must list `SequenceCommand`, and an edge must route it to the slot:

```python
uplink = m.add("uplink", Uplink(link, msgs=["SequenceCommand"]))
m.route(uplink, mode, msg="SequenceCommand")
```

(`link` is the handle from `m.state("link", TcpServer(...))`.)

Code in the host can also use the coordinator command output:

```python
m.route(m.coordinator, mode, msg="SequenceCommand")
```

Each command contains the target slot name. A slot drains all command inputs at the start of its step and ignores commands for other names.

The command set is:

- `Load { name }`
- `Start`
- `Stop`
- `Abort`
- `Reset`

There is no unload command in the current wire type. A later `Load` can replace a non-running occupant.

## Slot states

The slot publishes one `slot_status` frame each cycle.

| Code | State | Meaning |
|---:|---|---|
| 0 | Empty | No occupant has been selected. |
| 1 | Loaded | An occupant is ready, or `Stop` dropped it and `Reset` must rebuild it. |
| 2 | Loading | A process worker is starting and binding. |
| 3 | Running | The slot polls the occupant each cycle. |
| 4 | Done | The occupant returned. |
| 5 | Stopped | A panic or worker death ended the occupant. |

The frame also names the selected occupant.

The slot sends an event for each accepted transition. It sends `Refused` when the current state does not allow a command. It sends `Failed` when a load names an entry outside the allowed set or an occupant fails.

Two commands can arrive in one cycle. Events keep both results in order. The status frame only shows the final state for that cycle.

## Command rules

`Load` selects an allowed occupant and builds fresh state. It can replace an occupant in Loaded, Done, or Stopped state. It is refused during Running or Loading.

`Start` changes a live Loaded occupant to Running.

`Stop` drops a Running occupant at once. It does not run async cleanup. The state returns to Loaded, but no live future remains. `Start` is then refused until `Reset` rebuilds the occupant.

`Abort` writes cancel to a Running occupant. An async task sees it at the next poll. `wait` returns early, which lets the task send safe commands before it returns `Outcome::Aborted`.

A cyclic occupant cannot cooperate through `wait`. Its slot wrapper ends it as aborted when cancel arrives.

`Reset` builds the selected occupant from the start. It works after Done, Stopped, or a post-Stop Loaded state. It is refused during Running or Loading.

An async return changes Running to Done. A panic changes it to Stopped. The slot does not poll either terminal state again.

## Process slots

Set `process=True` on the slot to run all its occupants in worker processes:

```python
mode = m.slot(
    "mode",
    inputs=["attitude_estimate"],
    outputs=["mode_cmd"],
    allow=[commissioning(), safe_mode()],
    process=True,
)
```

Process mode belongs to the slot, not to one allowed entry. A slot cannot mix in-process and process occupants.

During resolve, a short-lived worker describes each candidate. The host does not load the candidate library into its own process. The coordinator stores the artifact path and params for later loads.

The coordinator stores each ring that crosses the worker boundary in the run's shared-memory session. The slot's command, event, status, and progress handling stays in the host.

Each runtime `Load` starts a new worker. The slot enters Loading while the worker starts, attaches rings, creates the entry, and runs init. The coordinator checks this work once per cycle. It does not block a cycle on the full load.

An initial process occupant completes this work during the target init barrier. A configured Running occupant can then start on the first cycle.

When loading finishes, the slot enters Loaded. `Start` then begins one worker step per cycle.

The coordinator waits up to `proc_step_timeout` for each step reply. A late worker adds `proc_step_timeout` to coordinator health. If the worker still lives, the slot stays Running and a later step can recover. If the worker died, the slot enters Stopped with `ProcessDied`.

A runtime process slot does not restart a dead worker on its own. Use `Reset` or `Load` to create a new worker.

`Stop` kills the current worker and reclaims its ring roles. `Reset` and a new `Load` also tear down the old worker first. Target shutdown asks the worker to stop, waits for a short grace period, then kills it if needed.

Process slots need a shared futex. The framework supports them on Linux and macOS 14.4 or later. Resolve rejects them on other targets.
