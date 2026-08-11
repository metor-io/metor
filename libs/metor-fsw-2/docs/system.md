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
    health: &mut HealthPort,
) {
    let Ok(Some(imu)) = imu.latest() else {
        health.error("no_imu");
        return;
    };

    estimate.publish(&state.update(now, &imu));
}

pub fn pack() -> Pack {
    Pack::new().system("navigation", system(nav_execute).init(nav_init))
}
```

The driver stores each port between calls. It times the function, reports
publish drops, and closes one health cycle after each call.

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
                Err(_) => output.health().error("input_corrupt"),
            }
        }
    }
}
```

The task may wait on input or a timer. It should use `AsyncContext::until_cancelled` around long waits so shutdown can stop it.

Snapshot inputs use private copy-in rings. Message inputs read producer rings without that copy.

The coordinator does not call `HealthPort::end_cycle` for this form. The system must choose when to publish health and flush queued logs. It must also choose how to report output publish drops.

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

The task driver times each poll, reports publish drops, and closes one health cycle per poll.

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

## Health and logs

Each system descriptor adds a `health` frame output and a `log` message output.

`HealthPort::error("kind")` adds to the total error count and the named count. A kind must not be empty or contain `.`.

`HealthPort::log(level, text)` queues a `LogEvent`. `end_cycle` sends queued events and one `SystemHealth` frame.

`SystemHealth` contains:

- cycle count
- total error count
- last execute or poll time in microseconds
- a bounded map of named error counts

Logs are postcard messages. They are not fields in the health frame.

Cyclic struct drivers, function drivers, and task drivers call `end_cycle` for you. Free-running struct async systems must set their own health update points.

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

Each attached system gets a scoped `&mut` borrow during its call. Attached systems stay separate. They keep their own instance names, ports, and health data.

Shared state works only for cyclic entries. A borrow must not cross an await.

`SharedLifecycle::start` runs before the first attached system init. `SharedLifecycle::shutdown` runs after the last attached system shuts down.

If shared state needs a background task, start it from `SharedLifecycle::start`. Keep async borrows inside that task separate from the scoped state borrow.
