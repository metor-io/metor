# Handoff: implement IO as a queue

You are implementing a planned feature in metor-fsw-3, a Rust flight-software
framework at `libs/metor-fsw-3` in the repo `/Users/sphw/code/metor/metor`.
The design and plans are done, reviewed, and committed (`87be4d3e` on
`sphw/rewrite-fsw`). Your job is to build it, one chunk at a time.

## Read first

1. `libs/metor-fsw-3/CLAUDE.md`, the style and testing rules. They are strict.
   Follow them in every line of code.
2. `libs/metor-fsw-3/DESIGN.md`, how fsw-3 works today.
3. `docs/plans/17-keep-the-core-small.md`, the rules that keep the core small.
4. `docs/plans/18-io.md`, the design. It is authoritative.
5. `docs/plans/18-io-plan.md`, the chunk split, the crate layout, and the
   settled rules.
6. The chunk plan you are implementing (`18a`…`18g`). Chunk D is removed;
   `18d` is a stub.
7. `TODO.md` §8, the deferred work. Do not build it.

The long draft `18-io-queues.md` and the research `18-io-queues-report.md`
are background only. Where they disagree with `18-io.md`, `18-io.md` wins.

## Order

A, then B, then C, then E and G, then F. Do one chunk per session unless told
otherwise. Start with **chunk A** (`18a-io-stellarator-impl.md`, titled "IO
core: ops, buffers, Os, reactor").

## How to work a chunk

1. Read the chunk plan end to end, and the code it names. The plan cites
   `file:line`; check each citation still holds before you rely on it.
2. Implement the plan's steps in order. Each step must build and pass its
   tests before the next one starts.
3. Tests per `CLAUDE.md`: unit tests next to the code, IO tests as
   integration tests under `tests/`, and nominal, off-nominal, and failure
   cases. Run the Kani and Miri checks the plan names, with the commands the
   plan gives (narrow filters, `--no-default-features` where stated).
4. Commit at each step boundary once that step is verified. Use
   `git commit -F <file>` for messages: the shell is fish, and fish
   substitutes backticks inside double-quoted `-m` strings. End each commit
   message with the attribution lines your session's instructions give.
5. When the chunk's exit criteria hold, spawn an antagonistic review agent
   on a different model to review the chunk's diff against its plan and
   `18-io.md`. Fix what it finds, then commit.
6. Report to the user: what landed, deviations from the plan and why, and
   anything left open.

## Rules that are easy to get wrong

- **Do not redesign.** If the plan is wrong or the code disagrees with it,
  stop and ask the user. Do not silently change the design. Small, local
  fixes are fine; say what you changed.
- **Do not build deferred work:** layers, the stellarator rewrite onto `Op`,
  group reactors, async systems taking `Cap<K>`, `SimProxy`, mixed real+sim
  targets, the `sim` resolver or sim links, extra sim kinds, Windows in the
  `Os`, the quiesce timeout.
- **Allocation only at init.** Nothing allocates after build in steady
  state. The plans include allocation tests; keep them passing.
- **No `unwrap`** without a `PANIC Safety` comment. Prefer returning errors.
- **Comments are rare, short, and plain.** Doc comments describe the code as
  it is, in plain English, with no history and no mannered prose.
- **Prefer functions to macros, free functions to supervisor structs, and no
  trait until two implementations exist.** Reuse existing fsw-3 primitives
  (rings, `Record`, `Params`, `DefCx`, `DynInputs`/`DynOutputs`, `LinkStatus`'s
  `changed` idiom) before inventing new ones.
- **Shared resources have the OS's semantics and nothing more.** No extra
  guard rails.
- **Linux 5.19 is the minimum.** io_uring tests run on Linux only. The dev
  host may be macOS, where `PollOs` is used; gate `UringOs` tests with
  `cfg(target_os = "linux")`.
- **Never pass `--features wiring-model`** (it no longer exists).
- **Do not touch** unrelated uncommitted files in the tree (for example
  `examples/adcs-fsw2/target.py`, `libs/metor-fsw-2/...`,
  `libs/metor-fsw-3/Cargo.toml` edits you did not make,
  `examples/adcs-fsw3/TODO.md`). Stage only your own files.

## Known loose ends to check while building

- Chunk A: whether Miri can run stellarator's reactor for the executor
  test in step 6. If it cannot, scope the test down and say so.
- Chunk F: that `harness`'s two-phase `dup2` fd passing fits
  `src/cli/children.rs`.
- Chunk C: the Python output form for `io_complete`'s outputs, and the
  `EXEC` executor swap. Verify against the builder and stellarator.

## Chunk A in one paragraph

A puts the op types (`Sqe`, `Cqe`, `Op`, `Out`, `IoStatus`, `IoError`,
`RawHandle`, `Token`, `BufRef`, `RawAddr`, `TtyOp`, `GpioOp`, `OpKind`) and
the buffer table (one contiguous arena per system, with its directory inside
it, frozen when the io steps are built) in fsw-3 core, which builds for
wasm32. Under the `host` feature it adds the `Os` trait (`push`, `submit`,
`reap`, `register`; dial targets go to the constructor), the `Reactor` (token
mapping, `push_own` for its own high-bit tokens, `cancel_all`, `in_flight`,
teardown in `Drop`), `PollOs` (Unix), and `UringOs` (its own ring; one fixed
buffer per arena; linked socket and connect for dials). stellarator gains
only `Executor::turn` with a 1 µs simulated clock, with tasks dropped before
the executor, and `TcpListener::adopt(std::net::TcpListener)`. No crate
outside fsw-3 and stellarator changes, and stellarator's public API is
unchanged. The plan has the exact signatures, steps, and tests.
