# CLI 2. Commands for looking before running: implementation plan

Phase 2 of `docs/cli-design.md`: `check`, `show`, `doctor`, units on
flags, project discovery, and styled help. Exit codes landed in phase 1.

## Facts the plan relies on

- `run::prepare` builds dev packs, evaluates the target file, and
  resolves the deployment. `run::load` builds one target's coordinator.
- A `ResolvedDeployment` holds each target's `TargetConfig`: its packs and
  a `CoordinatorConfig` with the clock and a `SystemConfig` per system
  (`id`, `ty`, `params`, `inputs` of `port ← from[system.port]`, `grants`,
  `location`).
- Python's `metor_config` exposes `ABI_VERSION`, which must equal the
  host's `pack::ABI_VERSION`.
- `rustc --print target-libdir --target wasm32-unknown-unknown` names a
  directory that holds `.rlib`s only when the target is installed.
- The `Table` widget and `Theme` from phase 1 render borderless tables.

## Tasks

### T1. Units on flags

`--sim-dt 10ms`, `--wall 100hz`, `--epoch 2026-10-29T00:00Z`, and a new
`--for 30s` that stops on clock time beside `--cycles`. Durations parse
as `jiff::SignedDuration` (friendly format) and convert to
`std::time::Duration`, which refuses negatives. A bare number keeps its
current meaning: seconds for `--sim-dt`, Hz for `--wall`, microseconds
for `--epoch`. `--wall` takes an optional `hz` suffix.

`--for` converts to a cycle count once the clock is known: its period on
a wall clock, `dt` on a simulated one. Children get the converted
`--cycles`.

Tests: each parser over nominal, suffix, bare, negative, and garbage
input; `--for` with each clock.

### T2. Project discovery

Without a `FILE`, `metor` walks up from the working directory to the
nearest `target.py`, as cargo finds `Cargo.toml`. The search stops at a
directory holding `.git`. `M0206`'s help names where it looked.

Tests: found in the cwd, found two levels up, stops at `.git`, missing.

### T3. `check`

Runs `prepare`, then builds each target's coordinator with `load`, and
stops. Each target's build is a `Checked` phase. Exits 0, or with the
first error's diagnostic.

Tests: a good deployment exits 0 and prints a `Checked` line per target;
a build error exits 1 with its code.

### T4. `show`

`metor show [FILE] [WHAT]` prints the resolved deployment to stdout as
tables. Each target is a title line in its hue with its clock, then a
table of `SYSTEM`, `KIND`, and `INPUTS` (`port ← system.port`). `WHAT`
narrows it: `fsw` prints one target, `fsw.nav` one system with its params
and grants. A name that matches nothing gets `M0104` with a suggestion.
With `--format json`, the selection is one `config` event.

Tests: snapshot of the ADCS-shaped fixture; a target filter; a system
filter; an unknown name.

### T5. `doctor`

Checks, each one line in a table with `✓` or `✗` and, on failure, the
fix on the next line:

| Check | How |
|---|---|
| python | the interpreter `eval_target` would use, `--version` |
| metor-config | it imports, and its `ABI_VERSION` equals the host's |
| uv | `uv --version`, optional |
| cargo | `cargo --version` |
| wasm32 | the target libdir holds an `.rlib` |

Each check is a function returning a `Check { name, status, detail, fix }`
so `pack_dev` can run the wasm32 check before a wasm build and fail with
its fix (`M0413`) instead of cargo's error.

Exits 1 if a required check fails.

Tests: each check's result over a fake probe; the table snapshot with a
failure.

### T6. Styled help

`Cli` gets `clap::builder::Styles` built from `Theme`: headings in
`Accent`, flags bold, placeholders `Muted`. Each command gets an
`after_help` example. Output flags group under an `Output` heading.

Tests: `metor --help` and `metor run --help` contain the examples, and
under `NO_COLOR` no escape codes.

## Order

T1 and T2 first: small and used by the rest. Then T3, T4, T5, T6. Each
task ends with tests and a commit; the phase ends with an adversarial
review.
