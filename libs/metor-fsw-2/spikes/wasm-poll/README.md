# wasm-poll — Phase 0 spike for the sequencing plan

A throwaway measurement harness, kept because its numbers decide later phases.
It is a **detached workspace** and is not a member of the metor workspace, so it
does not touch the main lockfile or CI.

```sh
./build.sh          # builds the guest for wasm32, runs the host harness
```

## What it answers

The sequencing plan proposes moving sequences onto a WebAssembly substrate:
one `.wasm` per sequence, polled once per cycle under a fuel budget. It flags
two unknowns as "measure, don't assume". This spike measures them on the real
ADCS predicates — `angular_distance` on the estimate delta, `omega.norm()` for
the rate gate, and a `target_for`-shaped look-at quaternion — running against
`nox`.

The guest is a commissioning-shaped ladder (warm-up → detumble → coarse → fine)
compiled to `wasm32-unknown-unknown`; the host drives it through `wasmi` and
compares against the identical ladder linked natively.

## Results

Apple M-series, `aarch64-apple-darwin`, `wasmi` 1.1 (interpreter, no JIT),
release build, best of 7 reps over 7,200 cycles at 120 Hz. Stable to ±2% across
runs.

| | ns/cycle |
|---|---:|
| native ladder | 67 |
| **wasm full cycle** | **2,873** |
|  … of which port marshalling | 7 |
|  … of which the poll itself | 2,865 |

- **Interpreter overhead: ~41x** versus native, on identical source.
- **Port marshalling: 7 ns**, 0.25% of a wasm cycle, for a 128-byte mailbox
  copied both ways.
- **Cost at 120 Hz: 0.034%** of an 8.333 ms cycle.
- **Fuel: 7,449 units per poll**, and half that budget traps the guest.
- Native and wasm ladders agree on every terminal state (both `Completed`,
  re-armed 33 times per rep).

## What this means for the plan

**Unknown 1 — math cost: resolved, and more favourably than expected.** 41x
sounds alarming as a ratio and is irrelevant as an absolute: a sequence costs
0.034% of its cycle. A hundred concurrent sequences would still be under 4%.
Pushing math into host functions (plan Phase 2) is therefore an *optimisation,
not a prerequisite* — the substrate is viable with all math inside the guest.
Phase 2 can be scoped down or deferred accordingly.

**Unknown 2 — port marshalling: resolved, a non-issue.** At 7 ns it is a
rounding error next to the poll, and it is the same copy the existing process
backing already pays. No design work is needed to avoid it.

**Bounded execution: demonstrated.** Fuel metering cuts a poll off mid-flight,
which is the property no natively-linked sequence can offer and the main safety
argument for the substrate. A real slot budget can be set from the measured
7,449 units with a wide margin.

## The finding that is not in the numbers

**`metor-fsw-2` cannot be a guest dependency.** `cargo check -p metor-fsw-2
--target wasm32-unknown-unknown` fails: the crate unconditionally pulls
`stellarator`, `memmap2`, `libloading`, `mdns-sd` and `gethostname`, and `errno`
refuses to build for the `unknown` OS. `nox`, by contrast, compiles to wasm
untouched — which is why the math half of this spike was possible at all.

So Phase 1 needs a **guest-side facade**: the author-visible surface
(`Input::latest`, `Output::publish`, `now`, `check`, `Outcome`, `Params`) split
out from the host machinery (rings, shared memory, dlopen, discovery). This
spike sidesteps it by reimplementing ~30 lines of the runtime, which is fine for
a measurement and is not a design. Sizing that split is the first real task of
Phase 1 and was not visible from the plan.

## Caveats

Read the numbers with these attached:

- `wasmi` on a development Mac, not a flight interpreter on flight hardware.
  The ratio should hold; the absolute figures will not.
- The guest uses a flat mailbox, not the real port API. 128 bytes is
  representative of a small slot contract, not a large one.
- The ladder is a hand-written state machine, not the async future a real
  occupant compiles to. Arithmetic is representative; control flow is simpler.
- A JIT (Wasmtime/Cranelift) would land far closer to native, but a JIT is
  probably not what anyone wants to defend in a flight review.

## A trap worth remembering

The first synthetic orbit put velocity almost exactly along **+Y** — which is
the singular direction for `point_minus_y_at`, since pointing the −Y body axis
at +Y is a 180° flip where `w = 1 + dot = 0` and the quaternion degenerates to
zero before `normalize()`. The gate then evaluated `NaN < 0.2`, which is
`false`, so the phase silently ran to its timeout and the harness spent 30
simulated seconds timing the *timeout path* while reporting confident-looking
numbers.

Two lessons, both now enforced in the harness: it asserts the ladder reaches
`Completed` (a run that ends `Pending` or `Failed` never exercised the deep
path), and it rejects a feed whose pointing gate is ever non-finite.

Worth noting `angular_distance` itself is well behaved — it returns exactly
`0.0` for identical and near-identical quaternions, which the guest's unit test
pins. The NaN came from the look-at construction upstream, not the metric.
