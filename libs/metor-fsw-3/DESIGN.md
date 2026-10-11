# Metor FSW

Metor FSW is a framework for building modular and reusable flight software and ground systems. We achieve this by breaking up software into a series of sub-programs we call "systems". Systems can broadly be thought of as function that has some inputs, some outputs, and shared state. Systems are joined together using a series of shared memory ring buffers.

## Rings, Frames, and Messages

A key concept in Metor FSW is the ring. A ring is a single producer multi consumer ring buffer. Readers create "views" into rings that allow them to read the contents. Systems take in "views" as inputs, and create rings as outputs. This allows you to compose systems into each other. For example you can do the following in pseudo code

```python
gps = gps_driver()
imu = imu_driver()
nav = nav_filter(gps = gps, accel = imu.accel)
```

nav_filter takes in a series of rings –- in this case gps and accell - and produces its own outputs also has rings. When nav_filter accepts data it is in the form of a "View"

### Rings

Ring buffers in metor are SPMC (single producer multi consumer) ring buffers, that can work cross process. Ring buffers have a fixed number of reader slots, than new readers can claim a slot. More details on the implementation can be found in ../metor-fsw-2/ring/lib.rs


### Frames

Frames are collections of data that can be decomposed into "components". A component is a multi-dimensional array (aka a tensor) of a fixed size. For example the following is a valid frame;

```rust
#[derive(Frame)]
#[repr(C)]
struct IMU {
    #[frame(timestamp)]
    timestamp: Timestamp,
    accel: [f64; 3],
    gyro: [f64; 3],
}
```

You will also notice that a frame has an optional timestamp field. This is particularly useful for data that might come from time-domains other than the main software run-loop.

### Dynamic Frames 

A feauture of the fsw ring implementation is that each write as an associated length. This allows you to encode variable length data in the ring buffer. One use case for this is dynamic frames. A dynamic frame is a frame that has a variable number of components.

This feature is similar in concept to how flatbuffers or rkyv handle dynamic data, with some minor differences. A dynamic container is really a pointer into extra space at the end of the frame.  For instance the memory layout would look something like this:

```
+----------------+
| frame header   |
+----------------+
| component 1    |
+----------------+
| len | offset   |
+----------------+
| dynamic data   |
+----------------+
```

In code you would define this as follows:

```rust
struct Top {
    #[frame(timestamp)]
    timestamp: Timestamp,
    processes: FrameList<Process>,
}
```
 
Reading and writing the frame is then done via a "Yoke" (inspired by the fantastic ). So you would have a `Yoke<Top, &[u8]>` that you could then use to read from the frame using generator accessors like `top.processes()`. Similarly this would work for writing as well with a `Yoke<Top, &mut [u8]>`; you could use this with `top.processes_mut()` to write to the dynamic data.

### Messages

Sometimes you need to send structured data that can not or should not be easily repersented as a series of components. In this case we also support "Messages" that are sent as variables length raw bytes, and not interpreted as a series of components.

In previous versions of metor-fsw, there were different delivery semantics for messages and frames by default. Now all messages and frames use the same delivery semantic. All writes will be read by all readers. Similarly messages used to be hard-coded for postcard, that should no longer be the case 

## Systems

As discussed above systems are conceptually functions that do some isolated task. Systems are called by metor-fsw in a fixed order, and are initialized with a series of rings. Broadly systems follow this series of traits:

```rust
type System {
    fn def() -> SystemDef;
    type State: SystemState;
    type Inputs: SystemInputs;
    type Outputs: SystemOutputs;
    fn execute(state: &mut State, inputs: &Inputs, outputs: &mut Outputs);
}

trait SystemState {
    fn from_config()
}

trait SystemInputs {
    fn from_views(views: &[View]) -> Self;
}
```

A `SystemDef` contains all of the type information about a system. Basically just what inputs and outputs it expects. This allows the fsw to feed views into the system, and to create new ring buffers for its outputs.

Systems have "state" which is a mutably accesible type that they can put whatever state they need into. A system's state is its own; no other system can reach it. Sharing is an opt-in extra, a parameter naming a shared instance, designed in `docs/plans/05-links.md` and held until a case needs it.


### Defining Systems

Although systems can be anything that implements the `System` trait, they are typically defined using a system builder. You can define a new system as follows:

```rust
struct Nav { ... }
impl Nav { 
    fn new() -> Self { ... }
}

fn nav(state: &mut Nav, gps: &mut Input<Gps>, imu: &mut Input<Imu>, est: &mut Output<Est>) { ... }

let system = system(nav).with_state(Nav::new);
```


### Adapters

Often we will want systems to run in different contexts; for instance we might want a system to run in a different process, in a shared object library, in a different thread, or even in WASM. Adapters are the way that we bridge between native in process systems, and these different types of systems. metor-fsw natively does not know about any of these other contexts.


Broadly adapters pretend to be a normal system, and then call out to the "real" system in the background. For WASM that means executign the system in the WASM worker. For a process that means notifying the other process to execute, and using shared memory to bridge the rings. Rings are backed by mmaped memory slices to facilitate this shared memory behaivor.

The first adapter is the thread adapter. A system authored as one `async fn run(.., stop)` runs on a background thread; the adapter mirrors each of its rings and copies records across every cycle, so the loop never waits on IO and a slow consumer drops only its own copies. Async systems share one background thread by default and are placed on a named thread with `fsw.add(.., thread="gps")`. Adapters are ordinary table entries: one closure that takes the instance's id, thread, definition, params, and rings, and returns a step.


## Coordinator

The coordinator is responsible for calling out to systems. Fundementally it is a very simple structure that loops through a list of systems and executes them one after another. 

The coordinator takes in a low level config, that is not designed to be authored by end-users. It is shaped roughly as follows:

```
struct CoordinatorConfig {
  systems: Vec<SystemConfig>,
}

struct SystemConfig {
  id: String,
  ty: String,
  inputs: Vec<InputConfig>
}

struct InputConfig {
  system: String,
  port: String,
}
```

These configurations assume that the list of systems available to the coordinator is provided ahead of time.

The coordinator also injects its own port into each system that includes telemetry about how the system ran including:
```
exec_time_ns: u64,
exec_offset_ns: u64,
```

## Configuration

Most users will configure metor-fsw through Python that generates a valid config for the build system and coordinator.

For instance the following is an example of a simple metor-fsw configuration:

```python
from adcs import Nav, Control
from plant import Plant

fsw = Target(cycle_rate = 100.0, namespace = "cube_sat")
motor_cmd = fsw.loop()
plant = fsw.add("plant", Plant(
    altitude=400e3,  
    inclination=math.radians(97.03),  
    ltan_hours=10.5, 
    motor_cmd = motor_cmd,
))

nav = fsw.add("nav", Nav(imu = plant.imu))
control = fsw.add("control", Control(est = nav.est))

motor_cmd.connect(control)
```

There are a number of notable things going on here. For starts, we are importing type-safe Python bindings for each "pack", a pack is a set of systems built together.

For another, we are declaring a "loop" that is a place where the outputs of a system feed into the input of a system that runs before it. You have to manually declare these, because they invoke a 1 cycle phase delay.

### Build System

So far we have side-stepped how we actually get systems to be built or included. Like what is a system actually? Some systems are built into metor-fsw, but most useful systems will be distributed external to metor-fsw.

Systems are bundled into "packs". To define a pack you create a pyproject.toml file:

```toml
[project]
name = "adcs"
version = "0.1.0"
description = "The plant, nav, and ctrl systems of the cube-sat example"
requires-python = ">=3.11"

[build-system]
requires = ["metor-build"]
build-backend = "metor_build"

[tool.metor.pack]
id = "adcs"

[tool.uv.sources]
metor-build = { path = "../../../../libs/metor-fsw-2/python/metor-build" }
```

Similarly to define a metor-fsw deployment you create a pyproject:

```toml
[project]
name = "cube-sat"
version = "0.1.0"
description = "cube-sat target config"
requires-python = ">=3.11"
dependencies = ["adcs-pack", "adcs-seqs", "metor-config"]

[tool.uv.sources]
adcs-pack = { path = "systems/adcs", editable = true }
adcs-seqs = { path = "systems/plant", editable = true }
metor-config = { path = "../../libs/metor-fsw-2/python/metor-config", editable = true }
```

## Types of systems

Thus far we've been dancing around what a system actually is. Broadly there are four types of system artifacts:
- Shared object libraries - these are compiled libraries that are dlopened into the final executable
- Executable - these are standalone executables that are run by metor-fsw
- WASM - these are WASM libraries that are run by metor-fsw
- Built-in - these are systems that are included with metor-fsw

Each of these types of systems has a different adapter that allows it to be run by metor-fsw. Each of these adapters features a stable ABI. Broadly each of these ABIs are very similar, though may be subtetly differnet to account for the specifics of the binary type

### Slots

Occasionally we might want to swap what system is running at runtime. That is what slots are for. Slots allow you to swap what system is running at runtime. This is primarily used for "sequence" style systems.


Slots can also be aborted and stopped at runtime. An abort is a form of graceful shutdown where the system is notified that it needs to shutdown, and it has a chance to gracefully exit. Stop simply no longer executes the system.

### Sequences

Sequences are a special type of system that has re-entrant execution. A normal system executes the same function every cycle, starting from the top of a function. But in flight software we often want to execute a sequence of steps, that might take multiple cycles. In psuedo-code we want to write something like this:

```
motor0.power_en = true
motor1.power_en = true
motor2.power_en = true
wait_for(|| motor0.healthy == true && motor1.healthy == true && motor2.healthy == true, timeout = 1.0)
motor0.arm = true
motor1.arm = true
motor2.arm = true
sleep(0.5s)
control.arm = true
```

One way to think of this sequence is a state machine that moves through the, power, motor arm, and control arm, states. But it is nice to write this out sequentially because that is the "true" form of the sequence. Thankfully Rust has a nice convinent way of expressing a state machine. Async functions

```rust
#[sequence]
async fn arm_controls(
    motor_power_en: &[Output<MotorEn>],
    motor_arm: &[Output<MotorArm>],
    motor_health: &[Input<MotorHealth>]
    control_arm: Output<ControlArm>) {
    for motor in motors_arm {
        motor.publish(true);
    }
    wait_for(|| motor_health.all(|motor| motor.latest() == true), 1.0).await;
    for motor in motors_arm {
        motor.publish(true);
    }
    sleep(Duration::from_millis(500)).await;
    control.publish(true);
}
```

In the above example the Future returned by the async function will be polled each cycle. When it reaches a sleep it won't continue until the elapsed time exceeds the specified duration. A wait_for will re-call the closure each cycle until it returns true


### ABI

metor-fsw uses a minimal ABI format to load packs from shared object libraries, and to execute systems from them. There will be a single public symbol `metor_fsw_pack_def` which will send a serialized `PackDef` struct to the caller. PackDef will rougly be defined as follows:
```rust
struct PackDef {
    systems: Vec<AbiSystemDef>,
}
struct AbiSystemDef {
    /// Includes name, inputs, and outputs
    system_def: SystemDef, 
    /// fn pointer to the system's state constructor
    state_ctor: Option<usize>,
    /// fn pointer to the system's execution fn
    execute_fn: usize, 
}
```


## Deployments

So far we have discussed how a single target, or an instance of metor-fsw, functions, but in reality you often want to chain together multiple targets to form a large "deployment". A deployment is really just a collection of targets defined in a single config file. You can define one as follows:

```python
deployment = Deployment(targets = [plant, fsw, gw])
```

#### Running

Deployments can be run a few ways. For local development the primrary path is to run it using the `metor run` command. In that state the entire deployment will be run at once. Deployments can also be run individually using the `metor run --target` command. 

For production deployments you likely want to distribute the work across multiple machines. metor-fsw supports different deployment backends to facilitate this. For example you can render the set of deployments out into a NixOS configuration, and then dploy it using whatever tools you want. 


### Cross target communication

What good is multiple targets if they can't talk to each other? To facilate that metor-fsw supports cross-targets coms. A `Publish` system serves the records you pass it as inputs over metor-proto, and a `Subscribe` system receives records and emits them as outputs. Each owns its own socket: a transport of `listen=` or `connect=`, so a target can serve the panel, push into a db, or dial another target with the same two systems.

```python
imu = a.add("imu", Imu())
publish = a.add("publish", Publish([imu], listen="0.0.0.0:2240"))
cmds = a.add("cmds", Subscribe([Arm], listen="0.0.0.0:2241"))
sub = b.add("subscribe", Subscribe([publish.imu.accel], connect=a_addr))
```

You reference the typed output of the Publish system, which allows you to subscrib to fields from a specific target. The last line, subscribing to a peer's frames with the address resolved by the deployment, is slice 7.


### Gateways

If you have multiple targets downlinking telemetry becomes challenging. Who do you ask for data? The answer is a single target called the gateway, that embeddeds a metor-db instance. The gateway subscribes to all telemetry from all targets, and then pushes it into metor-db. Then panel, and other downstream subscribes, can interact with the DB.

This has a few advantages. One is that slow consumers can interact with the DB without blocking the main run-loop, collecting passed or missed data easily. Another is that you can sync this gateway DB insance with other instances of the DB for replication.
