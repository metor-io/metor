# metor-code: design sketch

## Background: Revel Code

[RevelCode](https://www.revel.io/revelcode) is a compiled, plain-text DSL for hardware control and test automation that "reads like Python" and is written by test engineers, not just software engineers. Revel's [AI write-up](https://www.revel.io/newsroom/using-ai-to-build-faster-with-revel) adds that a program holds channels, constants, alerts, checks, sequences and state machines, and that "if it compiles it cannot crash." Neither page publishes a grammar; the only inline syntax shown is a wait condition, `|chamber_psi| >= 150.0`, with channels between pipes.

Ideas worth borrowing:

- **Fixed-cycle, in-order execution.** Code runs at a deterministic cycle rate, PLC-scan style, with no threads or locks in user code.
- **Sequences as numbered steps.** Open a valve, wait for a threshold, close it; operating modes are state machines.
- **Alerts and sustained trips as first-class declarations.** An alert fires on the cycle its condition holds; a trip can require the condition to persist for a duration.
- **Total programs.** The compiler rejects unbounded loops and recursion and constant divide-by-zero; runtime faults go to an error channel instead of crashing.
- **Hot reload that keeps state, or refuses.** A change that cannot keep state is rejected and the running system is left untouched.
- **Channels as the only hardware interface**, so simulated devices with the same channel names can stand in for real ones.
- **Templates** that stamp one controller across many devices with per-device setpoints.
- **Cycle-stepping test harness** usable from pytest without the runtime, plus AI skills that generate and review code because everything is plain text.

## Goals and non-goals

metor-code is a small, total language for systems and sequences that compiles with Cranelift to a native pack the existing Dylib adapter and `DlSlot` load unchanged. A `.mc` file is one more way to write a pack; the coordinator, rings, channels and Python target config do not learn it exists.

Goals:

- **ABI-native.** Output is a shared library exporting the eight `metor_fsw_*` symbols at `ABI_VERSION` 15, so it slots into `metor build`, the content-addressed store, `Load` and the panel with no host change.
- **Readable by operators.** Sequences read as steps; alerts, redlines and trips are one line each.
- **Cannot crash, cannot stall.** No unbounded loops or recursion, no allocation after `create`, no panics. Every loop has a static bound, so worst-case cycles per step is computable.
- **Deterministic and testable.** Same inputs, same outputs, same cycle; a JIT path steps it from pytest without a coordinator.
- **Stateful reload (later phase).** A rebuilt program loads over the old one keeping state, or the load is refused and the occupant keeps running.

Non-goals:

- General-purpose programming: no heap, strings beyond literals, closures, generics or user-defined traits.
- Drivers and IO. A `Cap<K>` stays a Rust concern; metor-code reaches hardware only through ports a Rust system writes.
- Async systems on their own threads. metor-code emits cyclic systems and sequences only.
- A wasm backend. Cranelift does not emit wasm; untrusted code keeps using the wasm adapter.

## Language overview

Frames are what metor-code programs pass to each other. A file declares frames at the top level, and each sequence, machine or system declares the frames it reads and writes as typed ports. The statements follow RevelCode: Python-style indentation, `=` to write, `wait` and `wait until`, and `machine` / `state` / `entry` / `loop`.

```python
use frames from "stand_io"              # PistonPos, PistonValves, ... from a Rust pack

frame PistonReq:
    extend: bool
    retract: bool

frame Supply:
    pressure_cmd: f64

sequence piston_cycle:
    in piston = piston_control.state     # another program's output
    out req: PistonReq
    out supply: Supply

    req = PistonReq(extend = False, retract = False)
    supply.pressure_cmd = 0.0

    run start_pump                       # start it, wait for it to complete
    supply.pressure_cmd = 80.0

    req.extend = True
    wait until piston.current == piston_control_states.extended
    req.extend = False

    wait 1.0

    req.retract = True
    wait until piston.current == piston_control_states.idle
    req.retract = False
    supply.pressure_cmd = 0.0

machine piston_control:
    in req = piston_cycle.req
    in pos: PistonPos                    # wired in the target
    out valves: PistonValves

    state idle:
        entry:
            valves = PistonValves(extend = False, retract = False)
        loop:
            if req.extend:
                goto extending

    state extending:
        entry:
            valves.extend = True
        loop:
            if pos.fraction >= 0.98:
                goto extended
    # extended and retracting follow the same shape
```

**Frames.** `frame Name:` declares a `#[repr(C)]` frame with scalar, array, enum or nested-frame fields; the `timestamp` field is implicit. A declared frame is a record of the pack: it is listed in `pack_def`, and its fields are announced as components like any Rust frame's. `use frames from "pack"` imports frames from a Rust pack's `pack_def` or the registry ICD, and `metor code bindgen` emits the matching `#[derive(Frame)]` Rust struct, so Rust and metor-code share one definition.

**Ports.** `in x: T` reads the newest `T` on that input (fsw-3's `latest`). `in x = prog.port` binds to another metor-code program's output and takes its type. `out y: T` is an output. An unbound `in` is a constructor argument of the generated Python class, wired like any Rust system's.

**Using frames.** `x.field` reads a field. `y = T(…)` writes a whole frame, and `y.field = v` changes one field of the output's current value. An output keeps its last value between writes, so field writes are safe, and it flushes once at the end of any cycle it was written in, stamped with `now`. Each output has exactly one writer, so two programs can never fight over a field.

**Sequences** run top to bottom once per start. `wait 1.0` waits in seconds; `500 ms` and `5 min` also parse. `wait until c` suspends until `c` holds, and an optional `timeout d` fails the sequence when it does not. `fail "msg"` ends it `Failed`. `start s`, `abort s` and `run s` (start, then wait for completion) command another sequence, and `s.status` reads its status. `redline "name": c` aborts when `c` holds on any cycle, and an `on abort:` block runs on abort or failure.

**Machines.** The first `state` is the initial one. `entry:` runs once on entering a state, and `loop:` runs every cycle while in it. `goto s` changes state at the end of the cycle. Every machine has a `state` output, a frame with one field, `current`, of the enum `<machine>_states`.

**Systems** are cyclic code with parameters, Revel's templates: one declaration, many instances, each added in Python with its own params and inputs.

```python
system heater(setpoint = 40.0):
    in temp: Thermal
    out heater: HeaterCmd
    integ = 0.0                          # state: kept across cycles
    alert overtemp critical: temp.c > 80.0 for 2.0
    loop:
        err = setpoint - temp.c
        integ = clamp(integ + err * dt, -10.0, 10.0)
        heater.duty = clamp(KP * err + 0.01 * integ, 0.0, 1.0)
```

**Types.** `bool`, `i8`–`i64`, `u8`–`u64`, `f32`, `f64`, fixed arrays, enums and frames. Locals are inferred, and a float literal is `f64`. There are no references, heap values, closures or generics.

**Totality.** The only loop is `for i in range(N)` with a constant `N`; there is no `while`. Helper `def`s are allowed, but the call graph must be acyclic. Constant division by zero and out-of-range constant indices are compile errors.

**Runtime faults.** Integer division by zero yields 0, a dynamic index clamps, and a NaN written to a field keeps that field's last good value. An input with no record yet reads as zeroes; `stale(x, 0.5)` is true when `x` is missing or older than 0.5 s. Each fault sets a bit, and the runtime logs one fault per kind per cycle on the system's `log`. Nothing traps.

**Alerts.** `alert name severity: c [for d]` fires on the cycle `c` has held for `d` seconds. Active alerts go to an `alerts` output every cycle, and each rising edge is a fault record on `log`.

## Execution model on the fsw-3 ABI

Compiled code never touches a ring. A small Rust runtime, `mc-rt`, owns the pack's `export_pack!`, the rings and the codecs; each step it copies the newest inputs into a flat port block, calls one compiled function, then writes whichever outputs that function marked. The generated code is pure arithmetic over memory the runtime laid out at `create`.

The host steps the pack through the ABI as usual; only the accented box is new compiler output.

| metor-code | fsw-3 |
| --- | --- |
| `frame` | a `#[repr(C)]` frame `RecordSchema` in the pack's records, laid out by the compiler |
| `system`, `machine` | a cyclic system type registered in the pack's `SystemTable` |
| `sequence` | a sequence type behind a channel adapter: `abort` input, `Status::Completed` / `Aborted` / `Failed` |
| `system` params | instance params, decoded at `create`; their JSON Schema types the Python class |
| locals at a block's top level | a fixed state block allocated once at `create`, laid out by the compiler |
| `in x: T` | an `Input<T>`; the runtime copies `latest` into the input block |
| `out y: T` | an `Output<T>`; the runtime holds its current value and writes it when marked |
| `in x = prog.port` | an input whose producer is that program's output, wired at emit |
| `start s`, `abort s`, `run s` | a `SequenceCommand` output wired to `s`'s channel `commands` input |
| `s.status` | an input on `s`'s `channel_status` output |
| machine `state` output | an output of `<Machine>State { current }` |
| `now`, `dt` | the `i64` passed to `metor_fsw_execute`, and the target's cycle period |
| `redline` | one `redline!` per declaration, each calling a compiled predicate |
| `on abort:` | an `on_abort` guard whose handler calls a compiled function |
| runtime fault | a bit in the call's fault word; one fault record per kind on `log` |
| `alert` | an `alerts` output frame, plus a fault record on each rising edge |

**Wiring.** Ports are wired the fsw-3 way. Each program is a generated Python class whose unbound `in`s are constructor arguments, added with `fsw.add` like a Rust system. A bound `in x = prog.port`, and every `start` / `run` / `status` reference, names a program by its instance id. The default id is the program's own name, and it resolves when the target is emitted, so add order only sets step order. A reference to a system added later becomes a loop edge and reads the value of the cycle before.

**The call.** Each compiled system exports `extern "C" fn(cx: *mut McCx) -> u32`. `McCx` is `#[repr(C)]`: pointers to the input block, the presence mask, the output block, the written mask, the state block and the params block, plus `now`, `dt` and the fault word. The return value is the fsw-3 `Status` word, so a sequence reports its end without another channel.

**Sequences.** The compiler lowers a sequence body to a resumable function: each `wait` and `wait until` is a numbered resume point stored in the state block, and timers are `time` fields there. `mc-rt` wraps it in a `#[sequence]`-shaped async fn that calls `resume` once per poll and yields on `Ok`. Abort, redlines, the channel adapter's `Start` / `Stop` / `Reset` / `Abort` / `Load`, and the run and abort clocks all come from fsw-3 unchanged.

**Registration.** The compiler emits a static descriptor symbol, `mc_program`: every type's name, kind, doc, ports with their record schemas, params schema, state and port block layouts, and function pointers. `mc-rt`'s table builder reads it and registers each type. `metor_fsw_pack_def` therefore reports metor-code types like any Rust pack's.

**Bounded time.** Because loops are bounded and calls are acyclic, the compiler computes a worst-case instruction count per entry point and puts it in the descriptor, beside the measured execution time `SystemStatus` already reports.

## Compiler pipeline

One front end and one Cranelift lowering feed two back ends: `cranelift-object` for flight packs, `cranelift-jit` for tests and the dev loop. Both link the same `mc-rt`, so a JIT run and a flight run differ only in how the code got into memory.

1. **Parse** to an AST with spans. A hand-written lexer emits `INDENT` / `DEDENT` / `NEWLINE` tokens and unit-suffixed literals (`500 ms`); a pinned `chumsky` parser runs over that token stream, with its Pratt parser for expressions, recovery at `NEWLINE` / `DEDENT` and top-level keywords, and labels on the constructs operators trip on (`frame` fields, `in` / `out` ports, `wait until … timeout`, `goto`, `entry:` / `loop:`). Errors render with `ariadne`. chumsky stays in one module whose only output is the AST.
2. **Resolve** `use frames` imports, `frame` declarations, constants, ports and params. Frames compute their `#[repr(C)]` layout here; imported ones are checked against the layout their pack reports.
3. **Check** types, loop bounds, the acyclic call graph, `goto` targets, and that every output a sequence writes is also written in its `on abort:` block or explicitly marked `keep`. That last rule is the review check Revel runs with AI, made a compile error.
4. **Lower** to a small typed IR: flat blocks with explicit offsets into `McCx`, sequences turned into resume-point switches, `for` loops unrolled or kept as counted loops.
5. **Codegen** each entry point (`step`, `init`, `resume`, `redline_i`, `on_abort`) with `cranelift-frontend`. Floating point is emitted as written: no fusion into FMA and no reassociation, so x86_64 and aarch64 agree bit for bit. `sin`, `cos`, `atan2` and friends call `libm` inside `mc-rt`, never the platform libm.
6. **Emit.** AOT writes an object file with the entry points and `mc_program`, and links it with the prebuilt `libmc_rt.a` into a `cdylib` through the target's linker. JIT defines the same symbols in a `JITModule` and hands `mc-rt` the descriptor in process.

The CLI is `metor code build file.mc --target aarch64-unknown-linux-gnu`, producing `file.so` plus the generated Python module. `metor build` stores the library under `.metor/store/<blake3>/` like any pack, so a slot's allow list can name it.

**Testing.** The JIT back end backs a Python helper next to fsw-3's existing bindings: compile a file, set inputs, step `n` cycles, read outputs, alerts and faults, all from pytest without a coordinator. The same tests run against the AOT library through a one-system coordinator, to catch any JIT and AOT drift.

**Hot reload.** A rebuilt program is a new stored path, loaded with the slot's `Load`. The new descriptor carries the state layout by field name and type. `mc-rt` copies fields whose name and type match, initialises new ones from their defaults, and refuses the load if a field changed type or a sequence's resume points changed while it is running; the slot then keeps its occupant. This needs one ABI addition and is not in the first implementation; until then a reload starts cold.

## Worked example

The leak check from Revel's write-up (pressurise to 150 psi, hold five minutes, abort above 165 psi), in a target with a Rust driver pack whose frames it imports.

```python
use frames from "stand_io"              # Chamber, ValveCmd

sequence leak_check(target = 150.0):
    in chamber: Chamber
    out valve: ValveCmd

    redline "chamber over": chamber.psi > 165.0
    on abort:
        valve.open = False

    valve = ValveCmd(open = True)
    wait until chamber.psi >= target timeout 30.0
    valve.open = False
    wait 5 min
    if chamber.psi < target - 2.0:
        fail "leak"
```

```python
from stand_io import PressureXdcr, ValveDriver   # Rust pack
from stand import LeakCheck                      # generated from stand.mc
from metor_config import Target

fsw = Target(cycle_rate=100.0, namespace="stand")
pc = fsw.add("pc", PressureXdcr(uart="/dev/ttyS1"))
leak = fsw.add("leak_check", LeakCheck(chamber=pc.chamber, target=150.0), channel="leak_check")
fsw.add("valve", ValveDriver(cmd=leak.valve))
```

What the compiler emits for `leak_check`, as the Rust it is equivalent to:

```rust
#[repr(C)]
struct LeakCheckState { resume: u32, deadline: i64, alerts: u32 }

extern "C" fn leak_check_resume(cx: *mut McCx) -> u32;     // Status word
extern "C" fn leak_check_redline_0(cx: *mut McCx) -> bool;  // "chamber over"
extern "C" fn leak_check_on_abort(cx: *mut McCx);
```

The operator starts `leak_check` from the panel, or another sequence does with `run leak_check`. Each cycle `mc-rt` checks `chamber over`, calls `resume`, and writes `valve` if it changed. If the wait times out, the sequence aborts before ending `Failed`, so `on abort:` closes the valve. A redline or an abort command takes the same path, and the channel reports both clocks as it does for a Rust sequence.

## Open questions and phased plan

Decided:

- **Targets.** Linux on x86_64 and aarch64, plus macOS on aarch64. Cranelift's lack of 32-bit ARM does not matter. On macOS, `cranelift-object` emits Mach-O, `cc -dynamiclib` links the `.dylib` (and ad-hoc signs it, as Apple Silicon requires), and the existing Dylib loader takes it unchanged. The JIT path needs `MAP_JIT` pages, which `cranelift-jit` handles. Because `mc-rt` uses its own `libm`, macOS and Linux aarch64 give bit-identical results.
- **Stateful reload.** In the design, not in the first implementation. Until phase 4, a `Load` of a metor-code pack starts cold, like any pack today. The ABI 16 state export lands with it.
- **`fail` aborts first.** metor-code's `fail` calls `seq.abort()` before ending `Failed`, so `on abort:` always runs. Follow-up: make Rust `#[sequence]`s do the same, so an `Err` from the body raises the abort flag and runs the `on_abort` guards.
- **Frames have no single source.** A frame may be declared in `.mc`, in Rust, or both. Compatibility is checked by layout, not by origin: the compiler checks imports against their pack's layout, and fsw-3 checks every edge's record at `build`.

Open questions:

- **Messages.** v1 ports are frames only, read with `latest`. Do sequences need postcard message inputs, or a drained queue of frames, beyond what `start` / `run` / `abort` give?
- **Units on quantities.** In v1 only wait durations take unit suffixes, and a bare number is seconds. Unit-tagged frame fields (`psi: f64 [psi]`) would catch real bugs, and the ICD could carry the tags.
- **ICD source.** Import frame schemas from the registry ICD JSON, from a producer's `pack_def`, or both?

Phases:

1. **Cyclic systems on JIT.** Parser, checker, Cranelift lowering, `mc-rt` with frames, params, state and faults; pytest stepping helper.
2. **AOT packs.** `cranelift-object`, link with `libmc_rt.a`, load through the Dylib adapter and a `DlSlot`; JIT and AOT parity tests.
3. **Sequences and alerts.** Resume-point lowering, `redline`, `on abort:`, the abort-coverage check, `alerts` output; driven from the panel's sequence control.
4. **Machines and stateful reload (after the first implementation).** `machine`, state layout diffing, the ABI 16 state export.
5. **Operator tooling.** A Claude skill for metor-code, editor syntax and diagnostics, and generated docs per pack.
