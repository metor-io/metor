# metor-expr Phase 0 — the compiler spike, on nox

Parent design: `python-expressions.md` (revision 3, incl. "Nox is the
math backend"). This plan covers only its Phase 0: prove an embeddable
Python→wasm compiler at the latency and size the design needs, with
**nox as the numeric backend and semantic oracle**, and pin the ground
rules everything later builds on. Frames, decorators, the run rule, and
all panel/FSW integration are out of scope — they consume this crate in
Phase 1+.

Ground rules inherited from the design doc:

- Numeric semantics are nox's. The compiler is verified by a
  **differential harness**: every semantic test compiles its source to
  wasm and runs it under wasmi, computes the same thing against native
  nox in-process, and asserts the results are identical. Nox is the
  single reference implementation — there is no CPython or other
  interpreter in the loop.
- The `noxpr` graph layer is **out of bounds** — unwired today, slated
  for removal. Nox enters as a kernel library (`Tensor`/`ArrayRepr`,
  `Dyn` arrays, `nox_array::ArrayView`) and as the reference, never as
  an IR. The compiler's own typed IR is the only program representation.

## The crates

`libs/metor-expr`, a workspace member. Edition 2024, `[lints] workspace =
true` (the fsw-2 convention). Dependencies:

- `rustpython-parser` 0.4 — source → Python AST. Proven embeddable by
  waspy; we take the parser only, nothing else from that stack.
- `wasm-encoder` 0.256 / `wasmparser` 0.256 — already in `Cargo.lock`.
- `nox` (workspace) — dev-dependency for the differential harness in
  this phase; shapes/dtype vocabulary only in the lib itself.
- Dev-only: `wasmi` 1.1 (the version metor-fsw-2 ships) to execute every
  compiled module in tests, fuel-metered, exactly as the hosts will.

`libs/metor-expr/prelude`, a `no_std` guest crate (workspace member,
excluded from default builds like the wasm fixtures): wraps nox's
dynamic-shape kernels and libm behind `extern "C"` entry points, built
`--target wasm32-unknown-unknown --profile wasm-release` by a regen
script, with the resulting `prelude.wasm` checked in beside its source.
The compiler embeds it with `include_bytes!` and **emits generated
functions into it** — the template-module approach: parse the prelude
once, append generated functions/data/exports with `wasm-encoder`,
re-emit. No binaryen, no C, no toolchain at expression-compile time; the
wasm32 target is needed only when regenerating the prelude, exactly as
`tests/fixtures/seq-fixture` already requires.

Public API, deliberately small:

```rust
pub fn compile(source: &str) -> Result<Module, Diagnostics>;

pub struct Module {
    pub wasm: Vec<u8>,        // closed module: no imports, ever
    pub manifest: Manifest,   // what the host needs to call it
}

pub struct Manifest { pub functions: Vec<FnSig> }
pub struct FnSig { pub name: String, pub params: Vec<(String, Ty)>, pub ret: Ty }
pub enum Ty { F64, I64, Bool, Tensor { dtype: Dtype, shape: Vec<usize> } }

pub struct Diagnostics(Vec<Diagnostic>);          // never panics on bad input
pub struct Diagnostic { pub span: Span, pub message: String }
```

The manifest stays host-side in this phase; embedding it behind
`expr_describe()` joins the ABI work in Phase 1, where the vocabulary
aligns with the serialized `SystemDescriptor` forms.

## Semantic ground rules (the decisions this plan ratifies)

Python syntax, nox numerics. Each rule gets a pinned test; the
differential harness checks each against native nox.

1. **Ints are `i64` and wrap** — dtype semantics, per the design doc.
   The stated divergence from CPython is bignums; it is documented, not
   papered over.
2. **`/` always yields `f64`** (true division, Python 3 style),
   including `int / int`.
3. **`//` and `%` use floor semantics** — result sign follows the
   divisor, as in Python. Wasm's `rem` is truncating, so the emitter
   carries the correction.
4. **Integer division and modulo by zero trap** — a trap is a contained
   diagnostic, and we refuse to manufacture a value. Float division by
   zero yields ±inf/nan per IEEE-754.
5. **`**`** compiles to repeated multiplication for small integer
   literal exponents, else `pow` from the prelude.
6. **`bool` is not secretly int**: comparisons yield `bool`;
   `if`/`while` conditions must be `bool`; chained comparisons
   (`a < b < c`) keep Python semantics. No `True + 1`.
7. **Promotion, not narrowing.** `i64 → f64` promotion in mixed
   arithmetic; `f64 → i64` only via explicit `int(x)` truncation.
8. Rejected outright, with span diagnostics: strings, lists, dicts,
   sets, classes, closures, imports, `try`, `with`, generators, `del`,
   globals mutation. The subset is small on purpose.

## Milestones

Each milestone ends green (`cargo test -p metor-expr`) and is committed
before the next begins.

**M0 — the prelude spike (de-risks the nox decision).** Build the
prelude crate with a first kernel set — elementwise add/sub/mul/div/neg
over `Dyn`-shaped f64 arrays (via `Array<T, Dyn>` / `ArrayView`), dot,
and the libm set (`sin cos tan asin acos atan atan2 exp log pow sqrt`) —
to `wasm32-unknown-unknown`. Measure the module size (watch faer: the
linalg paths must stay out of the build; if nox's `Dyn` machinery drags
in more than double-digit KB, fall back to `nox-array` views + hand
loops in the prelude, keeping full nox as oracle only — a deliberate
fallback, decided by measurement). Then prove the template approach:
append a hand-built function that calls a prelude kernel, run it under
wasmi, and confirm dead prelude functions can be dropped by call-graph
walk. **This milestone ends with a go/no-go note in this file.**

**M1 — scalars end to end.** `def` with annotated `f64`/`i64`/`bool`
params and return, exported by name, params passed by value. Expression
set: arithmetic, comparison, `and`/`or`/`not` (short-circuit),
conditional expressions, calls between `def`s in the module. Statement
set: assignment (annotated and inferred), augmented assignment,
`if`/`elif`/`else`, `while`, `break`/`continue`, `return`. Scalar math
is native wasm opcodes — the prelude is only entered for
transcendentals. Every test compiles a source string and runs the module
under fuel-metered wasmi.

**M2 — semantics pinned.** The ground-rules list above, each as a test,
plus the differential harness: each case runs compiled-under-wasmi and
against native nox in-process, asserting identical results. The harness
is the enforcement mechanism for "Python syntax, nox numerics."

**M3 — tensors.** `Tensor[f64, 3]` / `Tensor[f64, (3, 3)]` params and
returns through static per-function arg/ret buffers (`expr_arg_ptr(i)` /
`expr_ret_ptr()` — the region ABI's shape, minus state and describe).
Elementwise arithmetic with nox's broadcast rules, indexing with
constant and variable indices (bounds-trap), `for i in range(N)` with
constant `N`, reductions as loops, `dot`. Codegen calls prelude kernels
with compile-time-constant shape arguments; shapes are static per
compiled expression, so buffers are data segments and no guest allocator
exists. Harness tests compare whole tensors against native nox.

**M4 — measurements.** A results section appended to this file,
mirroring `spikes/wasm-poll/README.md`: compile latency for a one-liner
and a 100-line module (debug and release), module size with full,
GC'd, and absent prelude, wasmi ns/eval and fuel/eval for a
representative expression, and prelude-kernel call overhead vs an
open-coded loop for a length-3 vector op (the common case must not pay a
kernel-call tax if that tax is visible — if it is, small constant shapes
open-code and large ones call kernels, decided by this measurement).

## Acceptance

- One-liner compile **< 1 ms release, < 20 ms debug** (the panel's
  budget is a 200 ms debounce; a keystroke must never feel it).
- Scalar-only module **single-digit KB**; prelude adds only reachable
  kernels (call-graph GC proven in M0).
- Every compiled module: **zero imports**; scalar-only functions stay
  within one linear-memory page.
- Malformed source of any kind produces `Diagnostics` with spans — the
  compiler never panics (fuzz-ish test over truncated/mangled inputs).
- Differential harness green on every ground rule against native nox.

## Style and hygiene

House rules apply: design narrative in the crate docs (`lib.rs`), short
function docs, no play-by-play comments. Plain functions over macros;
one validation gate (the type checker) then trust — no defensive
re-checks downstream. Test helpers live in test modules. Commit at each
milestone boundary with the standard trailers.

## Explicitly deferred

- `Frame`/`State` classes, `@system`, binding, bare-expression
  desugaring, name resolution (Phase 1).
- `expr_describe()` / manifest-in-module, state slots (Phase 1, with the
  region ABI).
- Dtypes beyond `f64`/`i64`/`bool` (the `Ty` enum leaves room).
- Quaternion/spatial builtins from nox (post-Phase 1, with the frame
  work).
- Everything panel and FSW.

## Results

### M0 — go/no-go: **GO on the template approach, FALLBACK on the nox guest**

The spike splits cleanly in two, and the two halves came out differently.

**The template-module mechanism: GO, unreserved.** Parsing the checked-in
prelude with `wasmparser`, appending functions/types/exports/data with
`wasm-encoder`, and re-emitting produces modules that validate, run under
fuel-metered `wasmi`, and carry zero imports. Dead-function GC by call-graph
walk works and is worth much more than expected — see the sizes below. Seven
tests in `libs/metor-expr/src/tests.rs` pin all of it, including a generated
function driving a tensor kernel through a spliced data segment and a runaway
loop burning its fuel grant.

**The nox guest: FALLBACK triggered.** The plan named a size trigger; what the
measurement actually found was worse than size, on three independent counts.

1. **`Array<T, Dyn>` is `Vec`-backed.** `DynArray` (`nox/src/array/dynamic.rs`)
   stores `Vec<T>` plus two `SmallVec`s, and every op allocates a fresh output.
   nox is `no_std` but `extern crate alloc` is unconditional, so a nox guest
   needs a global allocator and takes heap traffic per sample — which the parent
   design doc rules out in as many words.
2. **The `Dyn` path is numerically wrong today.** Measured against the same
   operations at fixed shapes:
   - `Array::<f64, Dyn>` rank-2 add returns the wrong length —
     `DynArray::default` sizes the buffer with `dims.iter().sum()` where it
     means `.product()` (`dynamic.rs:61`), so `[2,3] + [2,3]` yields five
     elements, not six.
   - Scalar-against-vector broadcast returns shape `[]` and an empty buffer
     instead of the vector.
   - `dot` on two rank-1 `Dyn` vectors panics inside faer
     (`Assertion failed: size == len`, `faer/src/mat/mod.rs:182`), because the
     rank-0 output allocates zero elements by the same bug.

   The fixed-shape path (`Tensor<f64, Const<N>, ArrayRepr>`) is correct on every
   one of these and is what the workspace's own ADCS code uses. `Dyn` is
   exercised by a single 2×2 test, whose shape happens to make sum and product
   agree.
3. **faer rides along, and it is most of the module.** `RealField:
   faer::SimpleEntity + faer::ComplexField` (`nox/src/fields.rs:59`) puts faer
   on the type-level path for every float op, and `dot` calls its gemm. Guest
   `wasm-release` sizes:

   | prelude contents | bytes |
   |---|---|
   | libm transcendentals only (baseline) | 18,417 |
   | + nox `Dyn` elementwise `add`, `mul` | 29,560 |
   | + nox `Dyn` `dot` | **60,646** |
   | + `nox_array::ArrayView` kernels (what shipped) | **21,525** |

   Three nox `Dyn` kernels cost +42.2 KB over baseline, 31.1 KB of it from
   `dot`'s faer path alone. The equivalent hand-written kernels cost +3.1 KB.

**Decision, per the plan's documented fallback.** The guest takes
`nox_array::ArrayView` — a `&[T]` plus a `&[usize]`, whose only dependency is
`zerocopy` — with the loops written out in `libs/metor-expr/prelude/src/tensor.rs`,
implementing nox's right-aligned broadcast rule directly. Native nox stays the
harness oracle **at fixed shapes**, which is where it is correct and well
exercised; the oracle must not use `Dyn`. Nothing touches `src/noxpr/`, which
is not even compiled today (`nox/src/lib.rs` declares no `mod noxpr`).

The three `Dyn` bugs — `dynamic.rs:61`'s sum-where-it-means-product, and the
`dot` panic inside faer that follows from it — are **reported upstream for a
separate fix** and deliberately left alone here. nox is not modified by this
work, and this crate no longer depends on the path they affect.

**One more thing the harness has to know.** Guest `libm::exp(1.0)` and host
`f64::exp(1.0)` differ by 1 ULP (`2.7182818284590455` vs `2.718281828459045`).
Bit-identical differential results therefore require the host side to call
`libm` too, not `std`. nox with `default-features = false` already routes
`RealField` through `libm` — but `exp`, `log`, `tan`, and `pow` are not in
`RealField` at all, so M2's harness needs `libm` directly for those.

### M0 — measured

| what | value |
|---|---|
| prelude, `wasm-release`, `opt-level = "z"` + fat LTO | 21,525 bytes, 69 functions, **0 imports** |
| prelude linear memory | 2 pages (down from 17; see below) |
| compiled module, scalar-only (no kernels reached) | **4,990 bytes**, 11/69 functions kept |
| compiled module, `sin` | 9,438 bytes, 19/69 |
| compiled module, all transcendentals | 12,085 bytes, 31/69 |
| compiled module, all tensor kernels | 6,766 bytes, 28/69 |
| compiled module, every kernel | 16,253 bytes, 58/69 |

Scalar-only clears the "single-digit KB" bar with room to spare, and every
module clears "zero imports". The 11 functions a kernel-free module still
carries are the element-segment roots — reachable by index rather than by call,
so the walk cannot drop them; shrinking that floor is a Phase 1 nicety, not a
blocker.

The prelude's linear memory needed one deviation to be reasonable: wasm-ld
defaults to a 1 MiB shadow stack placed first, which made every module claim 17
pages before a single expression existed. `regen-prelude.sh` passes
`-zstack-size=65536 --global-base=65536`, putting the whole module inside 2
pages. Guest code has no deep recursion to fund — fuel bounds it long before
the stack does.

### M0 — deviations from the plan

- **The prelude crate is excluded from the root workspace, not a member.** It is
  a `no_std` `cdylib` with its own `#[panic_handler]`; it links for
  `wasm32-unknown-unknown` and for no other target, so membership would break
  `cargo build --workspace`. It carries its own `[workspace]` table and
  `[profile.wasm-release]`, and `scripts/regen-prelude.sh` drives it by
  `--manifest-path`.
- **`sqrt` is not a prelude kernel.** wasm has `f64.sqrt` as an instruction;
  routing it through a call would be a pure tax.
- **Kernel ABI is descriptor-based.** Elementwise kernels take one pointer to an
  `EwDesc` — three buffer addresses, a rank, and three shapes — laid into the
  arena as a spliced data segment, so a call site is `i32.const desc; call
  $k_add`. Every buffer is statically placed, so the descriptor can be baked at
  compile time. This is what makes M3's "no guest allocator" hold.

### M4 — measurements

Apple silicon, `--release` unless stated, `cargo test -p metor-expr --release
measure -- --nocapture`. Every figure is reproducible from
`src/tests/measure.rs`, which asserts the acceptance bars and prints the rest.

**Compile latency.** Both bars cleared with an order of magnitude to spare.

| | one-liner | 100-line module |
|---|---|---|
| release | **136 µs** (bar: < 1 ms) | 355 µs |
| debug | **821 µs** (bar: < 20 ms) | 3.9 ms |

Against the panel's 200 ms debounce a keystroke will not feel this even in a
debug build. The figure includes the 64 MiB-stack thread `compile` spawns for
the parse, so that hardening is already paid for here.

**Module size**, all with zero imports.

| module | bytes |
|---|---|
| prelude, as checked in | 21,445 |
| compiled, no kernels reached | **5,009** |
| compiled, `sin` + `exp` | 9,950 |
| compiled, elementwise tensor kernels | 6,410 |
| compiled, every kernel reachable | 16,176 |

**Evaluation**, under fuel-metered wasmi.

| expression | ns/eval | fuel/eval |
|---|---|---|
| `(x * 9.81 + y) / 2.0` | **32** | **5** |
| `sin(x) * cos(x)` | 178 | 136 |

**Kernel call versus open coding — the plan's open question, answered.** Both
forms are expressible in the language, so both were compiled and measured.

| operation | via kernel | open-coded |
|---|---|---|
| length-3 `sum(a + b)` | 790 ns / 752 fuel | **43 ns / 14 fuel** |
| length-3 `dot` | 71 ns / 48 fuel | **42 ns / 14 fuel** |
| length-8 `dot` | 133 ns / 98 fuel | **85 ns / 34 fuel** |
| length-32 `dot` | 385 ns / 338 fuel | **183 ns / 130 fuel** |
| length-128 `dot` | 1396 ns / 1298 fuel | **614 ns / 514 fuel** |

The tax is not merely visible, it is decisive, and **open coding wins at every
length measured** — there is no crossover in this range. Two separate effects:

- A `call` plus the kernel's own prologue is ~34 fuel, which swamps a length-3
  contraction that is 14 fuel open-coded.
- The elementwise kernels are far worse than the contraction ones — 752 fuel to
  add three elements — because `k_add` runs the *general* broadcast machinery
  per call: two shape copies, two stride computations, and an odometer per
  element. That generality costs ~230 fuel per element at length 3.

So the plan's contingency applies, and Phase 1 should act on it in two
independent ways:

1. **Open-code small constant shapes in the compiler.** Shapes are static, so
   the emitter already knows when a loop is three iterations. The measurement
   says the threshold is high — everything up to at least 128 elements is
   cheaper unrolled — but module size grows with the unroll, so the practical
   rule is a fuel-versus-bytes knob rather than a single number.
2. **Give the elementwise kernels a contiguous same-shape fast path.** Most
   real call sites have identical operand shapes and need no broadcasting at
   all; the odometer is being paid for a case that is rarely taken.

Note the compiler is not obliged to choose one: kernels stay the fallback for
shapes large enough that unrolling would bloat the module, and the checker
already knows which case it is in.

### M4 — one thing the sweep turned up

Open-coding a 128-term dot product tripped `MAX_DEPTH`, which was 96: a
left-associative chain of *n* terms nests *n* deep. Hand-written source never
gets there, but generated source does — and the compiler open-coding small
shapes for itself is exactly a generator. The limit is now 512, which is still
bounded and still far below what the 64 MiB parse stack can carry.

### Kernel catalog as landed

`sin cos tan asin acos atan atan2 exp log pow sinh cosh tanh floor ceil round
trunc fmod_floor` (libm, scalar `f64`); `k_add k_sub k_mul k_div k_pow k_atan2`
(elementwise, broadcasting, one pointer to an `EwDesc`); `k_neg k_dot k_sum`
(flat, length in elements); `k_matmul` (row-major `(m,k)@(k,n)`).

`sqrt`, `abs`, `min`, `max`, `floor`, `ceil`, `trunc`, and `round` are *not*
reached from compiled code — wasm has instructions for all of them, so the
emitter uses those and a module that only wants them reaches no kernel at all.
The prelude keeps its own copies because they cost nothing once GC'd away.

M0's `expr_arena` / `expr_arena_len` were removed in M3. Compiler-owned buffers
now start at the linker's `__heap_base`, so the compiler owns the layout, the
prelude reserves nothing, and there is no fixed ceiling on how much a program
may place.

### The other divergence M3 found: contractions are not fused

nox's `dot` reaches faer, which fuses each multiply-add. Core wasm has **no
scalar FMA instruction**, so no guest kernel can reproduce that rounding —
measured as a one-ULP difference on `dot([0.01, -0.02, 0.005])`.

`dot` in this language is therefore *defined* as a sequential non-fused sum,
which is what the guest can actually compute, and `a_contraction_is_not_fused`
pins the divergence in both directions so it cannot change unnoticed.

This qualifies a claim in the parent design doc: "panel-side native nox and
guest-side wasm nox are the same code, which is what makes identical results in
both hosts a property of the build rather than a promise." For elementwise
arithmetic that holds and is tested. For contractions it cannot, because the
instruction sets differ. The property that matters operationally is untouched —
panel and vehicle run *the same module* — but a Rust `nox::dot` of the same
numbers can differ in the last place, and anything that compares a Python
expression against a Rust implementation of the same formula needs to know it.
