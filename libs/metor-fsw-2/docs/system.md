# Systems

A system is one unit of target code. It owns state, reads typed input ports,
and writes typed output ports. A driver decides when the system runs.

A system gives one flight-software function a clear boundary. Sensor input,
state estimation, control, and device output can stay separate while they run
as one target. This makes each function easier to test, replace, and reuse.

## Start with a function system

For most cyclic work, define state and one execute function. The function
arguments declare the ports.

```rust
fn nav_init(params: NavParams) -> NavState {
    NavState::new(params)
}

fn nav_execute(
    state: &mut NavState,
    now: Timestamp,
    imu: &mut Input<Imu>,
    estimate: &mut Output<Estimate>,
    log: &mut LogPort,
) {
    let Ok(Some(imu)) = imu.latest() else {
        log.fault(LogLevel::Warn, "no_imu", "no imu sample", &[]);
        return;
    };

    estimate.publish(&state.update(now, &imu));
}

pub fn pack() -> Pack {
    Pack::new().system("navigation", system(nav_execute).init(nav_init))
}
```

The driver stores each port between calls. It reports publish drops and
flushes the log after each call; the coordinator times the step.

## Choose a run form

The crate has four system forms:

| Form | When it runs | Use it for |
| --- | --- | --- |
| Function system | Once per cycle | Most small cyclic systems |
| Struct cyclic system | Once per cycle | State and port sets that read better as named types |
| Cycle-polled pack task | One future poll per cycle | Sequences and other work that spans cycles |
| Free-running `AsyncSystem` | In its own task | Host I/O and timers that do not follow FSW cycles |

All four forms produce a `SystemDescriptor`. The descriptor lets the host
inspect ports before it builds the system. The run rule is the main reason to
choose one form over another.

## Struct cyclic system

A struct cyclic system implements `System` and `CyclicSystem`. The coordinator calls `execute` once per cycle.

```rust
impl System for Navigation {
    type Input = NavigationInput;
    type Output = Out<NavigationOutput>;
    const NAME: &'static str = "navigation";
}

impl CyclicSystem for Navigation {
    fn execute(
        &mut self,
        now: Timestamp,
        input: &mut Self::Input,
        output: &mut Self::Output,
    ) {
        let Ok(Some(imu)) = input.imu.latest() else {
            return;
        };

        let estimate = self.update(now, &imu);
        output.estimate.publish(&estimate);
    }
}
```

The input and output bundle derives list their port fields in source order. Bind must use that same order.

Use a cyclic system when the work belongs at one fixed point in each cycle.

## Free-running struct async system

A struct async system implements `System` and `AsyncSystem`. The coordinator spawns `run` once. The task sets its own pace.

`AsyncSystem` and its `AsyncContext` live in the host crate, not in
`metor-fsw-2-core`: the task is owned by the coordinator's runtime, so it is
registered statically and a pack cannot export one. Every other form works from
core alone.

```rust
impl AsyncSystem for LinkReader {
    async fn run(
        &mut self,
        context: &AsyncContext,
        input: &mut Self::Input,
        output: &mut Self::Output,
    ) {
        while let Some(next) = context.until_cancelled(input.frames.recv()).await {
            match next {
                Ok(frame) => self.send(&frame).await,
                Err(_) => output.log().fault(LogLevel::Error, "input_corrupt", "frame ring read corrupt", &[]),
            }
        }
    }
}
```

The task may wait on input or a timer. It should use `AsyncContext::until_cancelled` around long waits so shutdown can stop it.

Every async input and output uses a private ring. At the system's registered
cycle position, the coordinator imports its inputs and then exports work the
task produced before that boundary. The local task cannot run between those
operations, so newly imported data can first affect graph-visible output on the
next cycle. Registration order and `connect_delayed` therefore have the same
visibility meaning at async boundaries as at cyclic systems.

The coordinator does not flush the log or publish `system_status` for this form. The system calls `output.log().flush(now)` and `context.status().tick(elapsed_us)` when it chooses, and decides how to report its own publish drops.

Use this form for work driven by I/O or a timer rather than the FSW cycle.

## Cycle-polled pack task

`Pack::task` accepts an async fn whose ports move into its future. This is not a free-running `AsyncSystem`.

The coordinator polls the future once per cycle with a no-op waker. `cycle().await` and `wait().await` use the FSW cycle clock.

```rust
async fn warmup(mut output: Output<Ready>) -> Outcome {
    for step in 0..10 {
        let now = cycle().await;
        output.publish(&Ready::new(now, step));
    }
    Outcome::Completed
}

pub fn pack() -> Pack {
    Pack::new().task("warmup", warmup)
}
```

A task that returns `Outcome` can run as a sequence or a slot occupant. A task that returns `()` maps to `Outcome::Completed`.

The task driver reports publish drops and flushes the log after each poll; the coordinator times the poll.

Use this form for work that must make progress on cycle boundaries, such as a timed sequence.

## Params and build

Struct systems implement `BuildSystem` to build from typed params. `BuildSystem::configure` may resolve host data such as message name tokens for a statically built system.

A loaded system cannot use host tables during construction. Its params must contain all data needed inside the shared library.

A function system may use `.init(fn)` for typed params. It may use `.defaults(value)` to store encoded defaults.

A pack task receives params by value through `Params<P>`.

## Lifecycle

The common `System` trait has `init` and `shutdown` hooks. The two struct forms use them.

For a normal run:

1. Each free-running async system starts its task and runs `init`.
2. The coordinator waits for all async init calls.
3. Cyclic systems run `init` in registration order.
4. Async tasks enter `run`.
5. Cyclic systems step in registration order each cycle.
6. Shutdown cancels and joins async tasks.
7. Cyclic systems run `shutdown` in reverse order.

Function systems and pack tasks use the pack `Driver` contract. Their current drivers have no user init or shutdown hook after state construction.

## Status

The coordinator appends a `system_status` frame output to every system it registers and publishes it itself, once per cycle, right after stepping the system. No system declares or writes this port; a guest's positional ring arrays never include it.

`SystemStatus` contains:

- `cycles`: steps the coordinator has issued to the slot
- `last_execute_us`: how long the last step took on the host's clock. For a process system this is the doorbell-to-ack round trip, scheduling included.
- `state`: the slot's run state as a `SlotState` code (`Running` for a plain system; `Loaded`, `Done`, `Stopped`, and so on for a runtime slot)

The coordinator's own record (`coordinator.system_status`) counts FSW cycles and times the whole graph step.

A free-running async system is the exception: the coordinator never steps it, so it publishes its own record with `context.status().tick(elapsed_us)`.

## Logs

Each system descriptor adds a `log` message output. `output.log()` is the handle:

- `log(level, text)` queues a `LogEvent`.
- `fault(level, kind, text, fields)` queues a line whose first field is `kind=<kind>`. This is how a system reports a dropped publish, a corrupt input, or a missing sensor: the ground counts lines by kind. A fault that recurs every cycle costs one line per cycle.
- `flush(now)` stamps and sends the queued lines. Cyclic struct drivers, function drivers, and task drivers call it for you; a free-running async system calls it itself.

Lines are postcard messages, so a long line is never truncated. A line the ring rejects, or one queued past the per-cycle cap, is counted and reported as one `log_dropped` line once the ring has room.

## Shared pack state

`Pack::shared_state` creates one state value for several function systems in the same pack.

```rust
pub fn pack() -> Pack {
    let mut pack = Pack::new();
    let link = pack.shared_state("TcpServer", |params: LinkParams| {
        LinkState::bind(params.addr).map(|state| state.with_name(params.name))
    });

    pack.system("downlink", system(send).shared(&link))
        .system("uplink", system(receive).shared(&link))
}
```

Each attached system gets a scoped `&mut` borrow during its call. Attached systems stay separate. They keep their own instance names, ports, and log.

Shared state works only for cyclic entries. A borrow must not cross an await.

`SharedLifecycle::start` runs before the first attached system init. `SharedLifecycle::shutdown` runs after the last attached system shuts down.

If shared state needs a background task, start it from `SharedLifecycle::start`. Keep async borrows inside that task separate from the scoped state borrow.
