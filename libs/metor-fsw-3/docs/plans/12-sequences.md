# Sequences and slots

The fifth slice of metor-fsw-3: systems an operator can command at run
time, and sequences, async fns polled once per cycle. Both rest on a
channel: a name that addresses one system from outside the target, the
name `SequenceCommand` carries.

In scope:

- The `Adapter` enum on `SystemConfig`, holding today's `thread` and the
  new `channel`.
- The channel adapter: a step that wraps a cyclic system and applies
  `Start`, `Stop`, `Abort`, `Reset`, and `Load` to it.
- The `abort` port and the `Abort` parameter.
- `#[sequence]` fns, the `Seq` handle, and the sequence step.
- `DlSlot`, a built-in system that loads pack systems on `Load`.
- `ChannelRegistry`, a built-in system that publishes `SequenceRegistry`.
- Pack ABI 9: end-of-run status words, the adapter in `Instance`, and
  params in the definition context.

- A content-addressed pack store, so a candidate's path names its
  bytes.

Out of scope: channels on async systems, process and wasm occupants
(slice 6), an `Unload` command (the wire type has none), `Progress`
events (a sequence logs progress on its `log`), a sequencing DSL,
loading a pack the config did not list, and the panel.

## Why this shape

fsw-2 made the slot the unit. A slot was a special entry with its own
command input, status frame, event output, occupant contract, and a
six-state machine, and only slot occupants could be stopped or aborted.

This slice puts commanding in an adapter. The thread adapter wraps an
async system in a step that copies its rings; the channel adapter wraps
a cyclic system in a step that reads its commands. The coordinator
loop does not change: a channel is one more step in the list, with a few
more ports.

Loading is the one command that needs more than the wrapper, because
only a pack can supply a system the binary does not contain. `DlSlot` is
an ordinary cyclic system that accepts `Load` through one `Step` method.
The channel adapter hands it the command.

Abort is a record on a ring, not a call. The channel adapter owns the
ring, so abort crosses the dl boundary with no new export, and whether
a system can be aborted is decided at build from its ports.

Channels are cyclic only. Resetting an async system means ending its
task on another thread before placing a new one, and pausing it leaves
the task running with its mirrors frozen. Neither is needed for
sequences, which are cyclic.

## Adapter

```rust
pub struct SystemConfig {
    // ..
    #[serde(flatten, default, skip_serializing_if = "Option::is_none")]
    pub adapter: Option<Adapter>,
}

#[serde(rename_all = "snake_case")]
pub enum Adapter {
    Thread(String),
    Channel(ChannelConfig),
}

pub struct ChannelConfig {
    pub name: String,
    #[serde(default)]
    pub start: RunState,
}

#[derive(Default)]
pub enum RunState {
    #[default]
    Running,
    Stopped,
}
```

In JSON the adapter is `"thread": "gps"` or `"channel": {..}` beside
`id` and `ty`, so today's configs parse unchanged, and a config cannot
name both. A test pins that a config with both keys is refused.

`MakeCx::thread: &str` becomes `adapter: Option<&Adapter>`. Each make
function checks the adapter it is given:

| Make | Accepts | Refuses |
|---|---|---|
| cyclic (`table::entry`) | none, `Channel` | `Thread` → `ThreadOnCyclic` |
| async (`thread::place`) | none, `Thread` | `Channel` → `ChannelOnAsync` |
| `DlSlot` | `Channel` | none → `SlotWithoutChannel` |

No system records whether it is cyclic or async; the make function that
knows refuses. `pack::def::Instance::thread` becomes `adapter` for the
same reason, so a pack's own `place` refuses a channel too.

Build checks channel names against `SEQUENCE_CHANNEL_NAME_CAP` and
checks that they are unique.

## The channel adapter

### Ports

With a channel, build asks `channel::def(inner)` for the definition
rather than the type's own:

- The inner inputs, less any `abort` input, then `commands:
  Input<SequenceCommand>`.
- The inner outputs, then `events: Output<SequenceChannelEvent>` and
  `channel_status: Output<ChannelStatus>`.

`commands`, `events`, and `channel_status` are reserved on a channel
entry, as `status` is on every entry. Config edges and `Publish` see
only the outer ports.

```rust
/// One channel's state, written every cycle.
#[derive(Frame)]
#[repr(C)]
pub struct ChannelStatus {
    #[frame(timestamp)]
    pub timestamp: Timestamp,
    /// Running, stopped, completed, aborted, failed, panicked, or unloaded.
    pub state: u32,
    /// The occupant's index in the registry's `available`, or `u32::MAX`.
    pub loaded: u32,
    /// The occupant's library file name; empty when there is none.
    pub artifact: FrameStr<64>,
    /// The blake3 of the occupant's library; zeros when there is none.
    pub hash: [u8; 32],
}
```

Events carry the transitions; the frame carries the state, so a plot
shows it and a late joiner reads it without replaying events. The
index names the occupant against the registry. The file name and hash
name the exact bytes running, which the index alone cannot once a
candidate's build changes. All three are fixed-size, so the frame stays
a frame.

The adapter fills the last three from the inner step's `occupant()`,
which a step that is not a slot leaves `None`. `metor build` refuses a
candidate whose file name exceeds `FrameStr::<64>::CAPACITY`, so the
name is never truncated.

### `FrameStr`

```rust
/// Text in a frame: `N` bytes, NUL-terminated, shown by the panel as a string.
#[derive(Clone, Copy, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(transparent)]
pub struct FrameStr<const N: usize>([u8; N]);

impl<const N: usize> FrameStr<N> {
    /// Text bytes it holds; one byte is always the terminator.
    pub const CAPACITY: usize = N - 1;
    pub const EMPTY: Self = Self([0; N]);

    /// Refuses text longer than `CAPACITY` or holding a NUL.
    pub fn new(text: &str) -> Result<Self, FrameStrError>;
    /// The text up to the first NUL.
    pub fn to_str(&self) -> Result<&str, core::str::Utf8Error>;
}
```

It is one component, a `U8` tensor of shape `[N]`, whose metadata is
`ComponentMetadata::with_string()`. The panel reads `is_string` and
renders the bytes up to the first NUL as text. `new` refuses rather
than truncating. `to_str` is fallible, because a `FrameStr` decoded
from a ring can hold any bytes.

It lives in `metor-component` beside the scalar leaf impls, and
`metor-fsw-3` re-exports it. The field impls are the ones a leaf needs:
`AsVTable` (one field, shaped like `nox::Tensor`'s), `Metadatatize`
(`with_string`), and `AsComponentView` and `FromComponentView` (a `U8`
view of shape `[N]`).

### Step

```rust
pub struct Channel {
    commands: Input<SequenceCommand>,
    core: Core,
}

/// Everything but the command input, so a drained command can borrow
/// `commands` while it changes `core`.
struct Core {
    name: String,
    run: RunState,
    inner: Option<Box<dyn Step>>,
    /// An end the adapter set: a failed make, or an unwound step.
    end: Option<End>,
    /// The end last reported on `events`.
    reported: Option<End>,
    /// The last name `Load` accepted, reloaded by a reset.
    loaded: Option<String>,
    recipe: Recipe,
    threads: Threads,
    abort: Option<Output<AbortRequest>>,
    events: Output<SequenceChannelEvent>,
    status: Output<ChannelStatus>,
}

struct Recipe {
    make: Arc<MakeFn>,
    def: SystemDef,
    params: serde_json::Value,
    inputs: Vec<Vec<RingBuffer>>,
    outputs: Vec<RingBuffer>,
}
```

`channel::make` allocates a private abort ring when the inner
definition has an `abort` input. It binds that input to the ring and
keeps the writer. It clones the ring handles into the recipe and calls
the inner make. `TableEntry::make` becomes an `Arc` so the recipe can
keep it. A cyclic make never reaches `threads`, so the adapter passes
an empty `Threads` of its own when it re-makes.

Each `execute`:

1. Drain `commands`, apply the ones for this channel in arrival order,
   and emit one event per command.
2. If running and not ended, run the inner step through `catch_step`.
   The adapter catches an inner panic, and survives to take a `Reset`.
3. When the inner step's end differs from the one last reported, emit
   `Completed`, `Aborted`, `Failed`, `Failed { "panicked" }` for a
   panic, or `Unloaded`.
4. Write `channel_status`.

The coordinator steps the adapter every cycle, whatever the channel's
state, so a stopped channel still reads its commands. Its status
measures the adapter, which is near zero while the inner step is
paused.

### Commands

The rules are one pure function, `apply(state, command) -> Result<(),
Refusal>`, which decides only whether a command is accepted. It never
sets `run`; `Core::transition` does, and tests cover it. The Kani
harness proves that an accepted command is `Stop` or `Abort` exactly
when the channel is live (running and not ended).

| Command | Accepted from | Effect |
|---|---|---|
| `Start` | stopped, not ended | `run = Running` |
| `Stop` | running | `run = Stopped`; the inner step is kept, not executed |
| `Abort` | running, with an abort ring | one `AbortRequest` on it |
| `Reset` | stopped, or ended | drop the inner step, make it again, re-apply `loaded`, `run = Stopped` |
| `Load` | stopped, or ended | `inner.load(name)`; on success `loaded = name`, `run = Stopped` |

Anything else is refused with a `Refused { reason }` event and changes
nothing. Commands for other channels are ignored without an event,
because several targets can share one command stream.

- `Stop` pauses and `Start` resumes. A stopped sequence keeps its
  future, so `Start` continues from the await it stopped at.
- `Reset` drops the old step before making the new one, so the new step
  claims the views and the writer the old one released. A fresh view
  starts at the ring's published position, and `latest()` on it is
  empty until the producer writes again.
- Re-applying `loaded` makes `Reset` restart what was last loaded, not
  what the slot was built with.
- A failed re-make leaves no inner step and ends the channel as
  `Failed` with the make error. `Reset` can be tried again.

### Step changes

```rust
pub trait Step {
    fn execute(&mut self, now: Timestamp);

    fn fault(&mut self, _now: Timestamp, _message: &str) {}

    /// How the step ended, once it has.
    fn ended(&self) -> Option<&End> {
        None
    }

    /// Replaces what this step runs with the system `name` names.
    fn load(&mut self, _name: &str) -> Result<(), LoadError> {
        Err(LoadError::NotLoadable)
    }

    /// What this step has loaded, if it loads anything.
    fn occupant(&self) -> Option<&Occupant> {
        None
    }
}

/// One loaded system, as `channel_status` reports it.
pub struct Occupant {
    pub index: u32,
    pub artifact: FrameStr<64>,
    pub hash: [u8; 32],
}

pub enum End {
    Completed,
    Aborted,
    Failed(Cow<'static, str>),
    Panicked,
    /// A slot with no occupant.
    Unloaded,
}
```

`latched()` becomes `ended() == Some(&End::Panicked)`. The coordinator
still drops a step that unwinds out of `execute`. With a channel that
step is the adapter, which catches its inner step's panics first.

## Abort

```rust
/// Asks a system to finish what it is doing and stop.
#[derive(Record, Serialize, Deserialize)]
#[record(codec = "postcard")]
pub struct AbortRequest {}
```

The port is always named `abort`. Two things declare it:

- An `Abort` parameter on a cyclic `#[system]` fn. `Abort::requested()`
  latches true after the first record arrives. What an aborted cyclic
  system does is up to that system, because a cyclic system cannot end
  itself.
- Every sequence, through its step, as below.

Without a channel, an `abort` input has no producer and reads nothing.
`Abort` on a channel whose inner system has no `abort` input is refused.

## Sequences

### Authoring

```rust
/// Powers the motors, waits for them to report healthy, then arms control.
#[sequence]
async fn arm_controls(
    seq: &Seq,
    motor_en: &mut Output<MotorEn>,
    motor_health: &mut Input<MotorHealth>,
    control_arm: &mut Output<ControlArm>,
) -> Result<(), SeqError> {
    let _ = motor_en.write(&MotorEn { timestamp: seq.now(), on: true });
    seq.wait_for(Duration::from_secs(1), || healthy(motor_health)).await?;
    seq.sleep(Duration::from_millis(500)).await?;
    let _ = control_arm.write(&ControlArm { timestamp: seq.now(), armed: true });
    Ok(())
}

table.register_sequence("arm_controls", arm_controls(), Default::default)?;
```

`#[sequence]` expands like `#[system]` (`11-fn-systems.md`) to
`fn arm_controls() -> impl SequenceFn<State = ..>`, reusing `Ports`.
`#[state]`, port parameters, `&mut Log`, and `Abort` mean what they mean
on a system. A `now: Timestamp` parameter is rejected, because its value
would be stale after the first await; read `seq.now()` instead. Ports
are `NoWake`: a sequence never parks on a notifier. The coordinator
polls it every cycle.

A sequence is cyclic, so it takes a channel and refuses a thread.

### The handle

```rust
pub struct Seq { /* now, aborted: Cell */ }

impl Seq {
    pub fn now(&self) -> Timestamp;
    pub fn aborted(&self) -> bool;
    /// Resolves once `dur` of cycle time has passed since its first poll.
    pub fn sleep(&self, dur: Duration) -> Sleep<'_>;
    /// Resolves once `pred` holds, checked on every poll from the first.
    pub fn wait_for<F: FnMut() -> bool>(&self, timeout: Duration, pred: F) -> WaitFor<'_, F>;
    /// Resolves on the next poll.
    pub fn next_cycle(&self) -> NextCycle<'_>;
}

pub enum SeqError {
    Aborted,
    TimedOut,
    Failed(Cow<'static, str>),
}
```

The handle is a parameter, not ambient state. fsw-2's free `wait` and
`now` read a thread-local and panicked when called outside a poll; a
borrowed handle can only exist inside one. Deadlines are fixed on the
first poll, so a `Sleep` built before an await still measures from when
it is awaited. Abort wins: `sleep` and `wait_for` return
`Err(SeqError::Aborted)` on the first poll after an `AbortRequest`, even
when the deadline or the predicate would also resolve. `next_cycle` does
not check abort.

A sequence that must make things safe on the way out does it in one
place:

```rust
match phases(seq, &mut ports).await {
    Ok(()) => Ok(()),
    Err(error) => {
        safe(&mut ports);
        Err(error)
    }
}
```

Time is cycle time, from the coordinator's clock. A sleep that spans a
`Stop` counts the stopped time, so a sequence resumed after its
deadline wakes on the first poll.

### The step

`SequenceStep` owns one `Pin<Box<dyn Future<Output = Result<(), SeqError>>>>`.
The future is an `async move` block that owns the state, the bound
bundles, and the handle, and borrows them for the user's fn. This is
the thread adapter's `Launch` shape. The box is allocated when the step
is made: at build, on `Reset`, and on a `Load`. None of those is steady
state.

Each `execute`:

1. Set the handle's `now`. Drain the `abort` input, latching `aborted`.
2. Poll once with `Waker::noop()`, inside the system's log scope.
3. On `Ready`, drop the future and keep the `End`: `Ok` is `Completed`,
   `Aborted` is `Aborted`, and the others are `Failed` with the error's
   text.

A sequence with no channel starts at boot, runs once, and stays ended.

## DlSlot

A `DlSlot` is one place in the schedule that a pack system can be loaded
into. It is a built-in cyclic type, as `Publish` is async, and it is the
only code that opens a pack after build. It refuses to build without a
channel, since nothing else could load it.

### Config

```json
{
  "id": "mode",
  "ty": "dl_slot",
  "channel": { "name": "mode", "start": "stopped" },
  "params": {
    "allow": [
      { "name": "commissioning", "pack": { "id": "adcs_seqs", "path": ".." },
        "ty": "commissioning", "params": { "timeout_s": 30.0 }, "def": { .. } },
      { "name": "safe_mode", "pack": { "id": "adcs_seqs", "path": ".." },
        "ty": "safe_mode", "params": null, "def": { .. } }
    ],
    "initial": "commissioning"
  },
  "inputs": [ { "port": "attitude", "from": [ .. ] } ]
}
```

Candidate packs are not in the target's `packs`, so `metor run` does
not open them at start.

### Ports

The slot's definition is its first candidate's `def`. `metor build`
computes each candidate's definition from its pack under the slot's
context and bakes it into the params, as it bakes a peer's port
definitions today. Build fails when two candidates disagree: the same
names in the same order, and `same_record` on each.

Reading a definition from params means `DefCx` gains `params: &Value`,
and `DefCxOwned` carries it across the pack boundary. That is the
third ABI 9 change.

A candidate's `abort` input is part of the definition, so the channel
adapter binds its abort ring to the slot, and `Abort` reaches whichever
occupant is loaded.

### Load

The slot keeps a `RingBuffer` handle for each of its ports, and the
current occupant as a `DlStep`.

- `load(name)` looks the name up in `allow`. On a library's first open
  it hashes the file through one handle and refuses a hash that differs
  from the baked one before anything is mapped. After `Pack::open` it
  checks that the path still names the file it hashed (device and
  inode). A library already mapped skips both, since its bytes were
  checked when it was first opened. It checks the pack's ports against
  the slot's, drops the current occupant, and creates the new one over
  the slot's rings. If that create fails, it re-creates the previous
  occupant and returns the error.
- `occupant()` is the current candidate's `Occupant`. The slot builds
  one per candidate at make, from the baked hash and the path's file
  name, so reporting it allocates nothing.
- `execute` executes the occupant, if there is one.
- `ended()` is `Unloaded` while the slot is empty, and the occupant's
  end otherwise.

`initial` is created at build. If it fails, for example an async
candidate that the pack's `place` refuses, the slot starts empty with
`End::Failed` and its reason, and a later `Load` can fill it. With no
`initial`, the slot starts empty.

### Residency

A pack stays mapped once it has been opened. `LIBRARIES` holds every
library for the life of the process, because guest threads and TLS
destructors may still run its code. Loading a different occupant
destroys the old instance but does not unmap the old library.

`LIBRARIES` caches by canonical path, so two builds of a pack must
never share a path. The store below guarantees that.

`Pack::open` runs inside the cycle. Mapping a pack and running its
initializers costs that cycle whatever the dlopen takes. If that proves
too slow, the slot opens every candidate at make instead, and `Load`
only creates.

### Pack store

`metor build` copies each candidate's artifact into a store, and writes
the stored path and the hash into the slot's params:

```
<target>/.metor/store/<blake3 of the bytes>/<cdylib name>
```

- The copy goes through `copy_atomic`: a temp file beside the
  destination, then a rename. A path that exists already holds those
  exact bytes, so it is never written again. A signed macOS dylib is
  never overwritten in place, and replacing one that way gets the
  process killed on `dlopen`.
- A rebuilt pack is a new hash and a new path, so `LIBRARIES` opens it
  fresh and the old build stays mapped for any instance still using it.
- The path in the params is data the build wrote, not a name a command
  can supply, so the slot does not check it against a store root.
- A first load hashes the file before opening it. blake3 runs at
  gigabytes per second, so a pack of a few megabytes costs a few
  milliseconds. That is small next to the dlopen in the same cycle, and
  it catches a store file that was changed or corrupted after build.
- blake3 is already in the workspace.

The store holds candidates only. Target packs keep their paths for
now. Loading an uplinked sequence or hot-swapping a running pack
starts here later: put the bytes in the store, then extend `allow`.

## ChannelRegistry

`SequenceRegistry` is one whole-registry message where the latest one
wins, so no single channel can publish it. `ChannelRegistry` is a
built-in cyclic system whose params are the registry:

```json
{ "id": "channels", "ty": "channel_registry",
  "params": { "channels": [ { "name": "mode", "available": ["commissioning", "safe_mode"] },
                            { "name": "arm", "available": [] } ] } }
```

It encodes the message once, at make, and writes the bytes on its
first cycle and then once a second. A late panel therefore gets it
without a request. Python fills the params from the target's channels.

## Pack ABI 9

- `execute` returns three more status words for a step that has ended:

  | Word | Meaning |
  |---:|---|
  | 0 | Ok |
  | 1 | Panicked |
  | 2 | Completed |
  | 3 | Aborted |
  | 4 | Failed; the reason is a fault line on the occupant's `log` |

  `DlStep::ended` reports the last word it read. An unknown word is
  still a panic.
- `Instance` carries `adapter` in place of `thread`.
- `DefCxOwned` carries the instance's params.

Reset, abort, and load add no exports. Reset is `destroy` then `create`,
abort is a ring, and load is `open` then `create`.

## Python

```python
cmds = fsw.add("cmds", Subscribe([SequenceCommand], listen="0.0.0.0:2241"))
fsw.commands(cmds.sequence_command)

arm = fsw.add("arm", ArmControls(motor_health=motors.health),
              channel="arm", start="stopped")

mode = fsw.add("mode", DlSlot(allow=[commissioning(timeout_s=30.0), safe_mode()],
                              initial="commissioning", attitude=nav.est),
               channel="mode", start="stopped")

registry = fsw.add("channels", ChannelRegistry())

fsw.add("down", Publish([registry.registry, arm.events, mode.events, mode.mode_cmd],
                        listen="0.0.0.0:2240"))
```

- `channel=` and `start=` go on `Target.add`. Passing both `channel=`
  and `thread=` raises.
- `fsw.commands(..)` wires its sources into every channel's `commands`
  input, including channels added after the call.
- `ChannelRegistry()` takes its params from the target's channels when
  the config is emitted.
- `DlSlot` takes its ports from its first candidate's stub and types
  its keyword inputs and output handles from it. The recorder lists
  each candidate's pack by path in the slot's params, not in the
  target's `packs`.

## Tests

Unit, next to the code:

- `apply`, table-driven: every command from every state; each refusal
  names the state.
- Kani over `apply`: an accepted command is `Stop` or `Abort` exactly
  when the channel is live.
- `Adapter` serde: `thread` and `channel` round-trip, a config with
  neither parses, and one with both is refused.
- Make refusals: `Thread` on a cyclic system, `Channel` on an async
  one, a `DlSlot` without a channel.
- Channel adapter over a counting step: `Stop` skips execution while
  commands still apply; `Reset` makes a fresh step and releases the old
  one's reader slots, in a ring with no spare slot; an inner panic
  leaves the adapter alive; commands for another channel produce no
  event.
- `Load` then `Reset` restarts the loaded system, not the initial one.
- Sequence step: completes; `sleep` measures from the first poll;
  `wait_for` times out; abort beats a deadline that falls on the same
  poll; `next_cycle` ignores abort; a panic ends as `Panicked`; a
  `Failed` carries its text.
- `Seq` futures polled with a hand-set handle, with no coordinator.
- `Abort::requested` latches; an `abort` input with no channel reads
  nothing.
- `ChannelRegistry` encodes once and writes on cycle 0 and then once a
  second.
- `channel_status` follows every transition. After a `Load` it carries
  the candidate's index, file name, and hash; on an empty slot it
  carries `u32::MAX` and zeros.
- Pack store: the same bytes land at one path and are not rewritten;
  changed bytes land at a new path; a path outside the store is refused.

Coordinator, in `coordinator/tests.rs`:

- `Stop` then `Start` resumes a sequence at its await.
- `Reset` after a panic brings back a cyclic system, and `Start` runs it.
- A duplicate channel name, an over-length name, and a system port
  named `commands` or `events` under a channel all fail build.

Integration, in `tests/`:

- The echo-pack fixture gains two sequences with the same ports. A
  `DlSlot` loads one, starts it, lets it complete, loads the other, and
  aborts it. The events arrive in order on `mode.events`.
- `Load` of a candidate whose definition no longer matches is refused,
  and the slot keeps its occupant.
- `metor build` fails on candidates whose definitions disagree.
- A `SequenceCommand` sent over `Subscribe` from a test client stops and
  resets a channel end to end under `metor run`.

## Shipped deviations

Config and ABI:

- serde's `flatten` over an `Option<Adapter>` took one key silently when a
  config named both, so a small `adapter_keys` serializer spells the two
  keys and refuses a config with both.
- `Coordinator::latched()` stays, as the panicked subset of `ended()`.
  `metor run` fails on it, so a sequence that completes does not fail a
  run.
- An ended step is kept, not dropped; only an unwind drops it. A `DlStep`
  or `Thread` whose system panicked lives until the coordinator drops.
- `End::Unloaded` has no status word; `Status::of_end` sends it as
  `Failed`. Only the host-side slot reports it.

Records:

- Record names are snake_case (`sequence_command`,
  `sequence_channel_event`, `sequence_registry`), following `log`. The
  packet ids come from the schema name and match the wkt ids.
- `MAX_LEN` is a constant, so the registry's is a fixed 16 KiB. A
  registry past it fails its write; `ChannelRegistry` does not check the
  size at make.
- `ChannelStatus::hash` is a `FrameBytes<32>`: one `U8` component of shape
  `[32]`. A plain `[u8; 32]` becomes 32 components.
- The table's known records include the three a channel adds, beside
  `status`, so a `Subscribe` of `SequenceCommand` resolves.

Channel adapter:

- `abort` stays an input on the outer definition, and config edges into
  it are refused. The adapter then rebuilds the inner port list by
  popping what it appended.
- `apply` returns `Result<(), Refusal>`; an accepted transition is the
  command itself, so there is no `Transition` enum.
- An accepted `Abort` emits `Progress { detail: "abort requested" }`,
  because the wire has no abort-requested event and a cyclic system may
  never end.
- `Reset` emits `Loaded { name }`, with the last loaded name or the
  instance id. Every `LoadError` emits `Refused`. A failed re-make emits
  `Failed` once.
- `Reset` reloads the last load in place on the existing step, and makes
  the system again only when the step is gone or nothing was loaded. A
  `Load` with no step is refused and changes nothing.
- A panic reports `Failed { reason: "panicked" }`. The wire's `Stopped`
  is kept for an operator `Stop`, which the panel shows as stopped.
- An `Abort` the full ring cannot take emits `Refused`, and event
  reasons are cut to 512 bytes so every refusal fits its record.
- `SequenceChannelEvent::MAX_LEN` is 1024, not the plan's 256.
- `Channel::ended()` is always `None`: the coordinator skips ended steps,
  and a channel must read its commands after its system ends.

Sequences:

- `#[sequence]` refuses an `Abort` parameter; the step already declares
  `abort`, and `seq.aborted()` and the waits cover it.
- The return check is a path ending in `Result`; another error type
  fails at the generated `call`.
- `next_cycle` counts polls, not timestamps.
- A failed sequence writes a `kind=failed` fault line, which is how a
  pack's host sees the reason behind status word 4.

Slots and the store:

- A bad `initial` ends the slot as failed at build instead of failing
  the build.
- There is no `store` param and no containment check; the hash gate
  is the one check a load makes on the file.
- `DlSlot::load` recomputes a candidate's ports under the slot's id and
  params only, and compares them by name and record. A candidate whose
  ports come from dynamic config inputs is refused at load; `metor
  build` checks with the full context.
- The slot pass resolves one slot at a time, in step order, writing each
  back before the next, so a slot can read another slot's output.
- The slot pass opens each candidate from its pack directory, so the
  build process maps the library there as well as from the store.
- A candidate is resolved when it has `path`, `hash`, and `def` and no
  `pack`, which makes the pass idempotent.
- Generated pack modules hide every `abort` input, a cyclic `Abort`
  parameter's too, since only a channel feeds it.

Python and tests:

- A `DlSlot` names each candidate by its type, and takes its inputs as
  keywords of the first candidate's; a candidate with inputs of its own
  is refused.
- `start` is emitted even when it is `running`.
- `ChannelRegistry` writes every `round(cycle_rate)` cycles, once a
  second under a wall clock.
- The end-to-end test is `tests/sequences.rs`, over
  `tests/fixtures/echo-pack/slot.py`. It checks `channel_status` through
  its announced metadata (the artifact is a string), not by decoding the
  table. A `Publish` does not replay the registry to a late client; the
  test reads the next rewrite.
- `Target.add` returns the system's own type, so a type checker does not
  see a channel's `events` and `channel_status` on the handle.

## Decisions

Settled 2026-09-23.

1. Making a step allocates: the sequence future, the `DlStep`, and the
   re-parsed params. That happens at build, `Reset`, and `Load`, which
   count as initialization. Encoding an event allocates its channel
   string once per command. Both run at command rate, never per cycle.
2. `Pack::open` inside a cycle is accepted. The fix, if needed, is
   opening every candidate at make, not a `Loading` state.
3. `Progress` events are descoped.
4. Candidates load from a content-addressed store, the base for
   hot-swap and uplinked sequences later.
5. `wait_for` keeps its predicate closure. It breaks the no-callbacks
   rule because it is easier to write and read than the pull-style form.
6. `channel_status` is in.

## Open decisions

1. Channels on async systems. Adding them later means a per-member
   stop and cancel in the thread group, and a shared `Threads` handle
   the recipe can re-place with. Nothing in this slice blocks that.
