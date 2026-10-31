# CLI 1. The output layer: implementation plan

Phase 1 of `docs/cli-design.md`: `Theme`, the widgets, `Event`,
`Reporter`, diagnostics, collapsed cargo output, and the log format. No new
commands. At the end, `metor run` on `examples/adcs-fsw3` looks like the
design's Phases, Logs, and Errors sections.

## Facts the plan relies on

- `cli::main` prints `metor: {error}` and exits 1 (`cli/mod.rs`). Every
  command returns a `thiserror` enum: `RunError`, `PackDevError`,
  `CargoError`, `ConfigError`, `DeployError`, `BuildError`.
- `run::install_logging` installs `tracing_subscriber::fmt` on stderr and
  `LogLayer`. `LogLayer` writes an event to the current thread's
  `LogPort`, which is set only while a system runs (`log.rs`).
- `Log::info` and `Log::fault` write only to the port. A system's own lines
  and faults never reach the terminal today; only `tracing` events do, with
  a module path as their source.
- Every system has a `log` output of `LogEvent` and a `status` output of
  `SystemStatus` (`coordinator/status.rs`). The Python config already names
  both (`metor_config/_generated.py`, `FRAMEWORK_PORTS`).
- `Publish` takes its inputs from config (`takes_config_ports`,
  `link/publish.rs`), so a built-in system can read any system's outputs.
- `cargo::cargo` inherits cargo's stderr and parses `--message-format=json`
  on stdout. `pack_dev` prints each written path with `eprintln!`.
- `children::prefix` writes `NS │ line` for each child line.

## Layout

```
src/cli/ui/
  mod.rs       Ui: the chosen Reporter, Theme, and caps; global flags
  theme.rs     Theme tokens, dark and light, colour depth, glyphs
  caps.rs      terminal detection: tty, depth, unicode, links, width, bg
  event.rs     Event and its payloads
  human.rs     the styled renderer
  plain.rs     the unstyled renderer
  json.rs      the NDJSON renderer
  spinner.rs   Spinner
  table.rs     Table
  logline.rs   one log line into a reused buffer
src/cli/console.rs   the fsw.console system
```

## Tasks

### T1. Theme and caps

`Caps` is detected once: is stderr a TTY, colour depth
(`supports-color`, `NO_COLOR`, `CLICOLOR_FORCE`, `--color`), Unicode
(`supports-unicode`, `--ascii`), hyperlinks (`supports-hyperlinks`), width
(`terminal_size`), and background (`terminal-colorsaurus`, then
`COLORFGBG`, then dark). Detection takes the environment as an argument so
tests do not touch the process.

`Theme::new(&Caps)` returns `anstyle::Style` per token and the glyph set.
Truecolor values for dark and light; designed 256 and 16 palettes; plain
styles at depth 0. Eight target hues.

Tests: each depth yields styles with no truecolor above it; ASCII glyphs
are all ASCII; `NO_COLOR` wins over a TTY; `--color always` wins over a
pipe.

### T2. Event and Reporter

```rust
pub enum Event<'a> {
    Phase(Phase<'a>),
    Diagnostic(&'a miette::Report),
    Log(&'a LogLine<'a>),
    Ended(&'a Summary),
}
pub trait Reporter: Send {
    fn event(&mut self, event: Event<'_>);
}
```

`Phase` is `{ verb, subject, state: Started | Progress(n) | Done(Duration)
| Failed }`. `Ui` picks `Human`, `Plain`, or `Json` from `Caps` and
`--format`, and is a process global behind a `Mutex`, set in `cli::main`.

Global flags on `Cli`: `--color`, `--format human|json`, `--ascii`, `-v`,
`-q`. `Plain` is `Human` with depth 0 and no spinner redraws.

Tests: `insta` snapshots of one fixed event sequence through each
renderer; `Json` lines parse and carry `"v": 1`.

### T3. Diagnostics and exit codes

Add `miette` with `fancy`. Each error enum derives `Diagnostic` with a code
per variant: `M01xx` usage and names, `M02xx` config, `M03xx` packs and
cargo, `M04xx` running. `help` where there is a fix to give.
`#[diagnostic(transparent)]` on wrapping variants. `url` points to
`metor explain`.

`cli::main` renders through `miette::GraphicalReportHandler` with
`GraphicalTheme` built from `Theme`, through the `Reporter`.

`ResolvedDeployment::target` gets a `strsim` suggestion and the list of
namespaces in `help`.

Exit codes as in the design: 0, 1, 2 (clap), 3 latched, 4 child failed,
130 interrupted. `cli::main` maps the error to its code.

Tests: one snapshot per error family; `--target fws` suggests `fsw`; the
exit-code map covers each `RunError` variant.

### T4. Phases and collapsed cargo output

`cargo::cargo` pipes stderr, adds `--message-format=json-render-diagnostics`,
and reads stdout as it arrives. Each `compiler-artifact` emits
`Phase(Building, Progress(n))` with the crate name. Cargo's stderr is kept
in a buffer and replayed on failure or with `-v`. The total for a gauge is
not known without `--unit-graph`, which is unstable, so the line shows the
count and the current crate, no gauge.

`pack_dev`'s path list moves to `-v`. `prepare` emits `Built`,
`Evaluated`, and `Resolved`; `run` emits `Running` with the namespaces.

`Spinner` redraws at 10 Hz on its own thread inside DEC 2026, only while a
phase has run 200 ms, and turns into `✓` or `✗` when the phase ends.

Tests: the artifact counter over a recorded cargo JSON stream; a failing
build replays stderr; spinner frames at each tick through a fake clock.

### T5. The log line

`logline::write(buf: &mut String, line: &LogLine, theme: &Theme)` writes
time, level glyph, `target.system`, message, and fields. `buf` is reused;
nothing else allocates. Time is mission elapsed from the run's start, or
sim time on a simulated clock. Info has no glyph; warn and error tint the
message. A deployment of one target drops the target.

Tests: snapshots at depth 0 and truecolor; field quoting; a long message
truncates at the width.

### T6. The console system

`fsw.console` is a built-in system that `metor run` appends to each
target's config. It takes every system's `log` output as a config input,
like `Publish`. It runs last in the step order, so it sees each cycle's
lines in the same cycle.

Each step drains its inputs into a ring of `LogLine`s preallocated at
make, and a UI thread prints them through the `Reporter`. A full ring
drops lines and counts them, and the count prints as one warning.

`install_logging` replaces `fmt::layer` with a `ConsoleLayer` that prints
only events outside any system (no current `LogPort`); events inside a
system reach the console through the port. `log::has_port()` is added for
this.

Decoding a `LogEvent` builds owned strings. That allocation is the open
TODO in slice 3 ("Logging allocations") and is not fixed here.

Tests: a target with one system that logs and one that faults prints both
with the right source; an event outside a system prints once; an event
inside a system prints once; a full ring reports the drop.

### T7. Children

Each child gets `--color` and `--format` from the parent, and its namespace
is already the source prefix, so `children::prefix` passes lines through
unchanged and is removed.

Tests: `member_argv` carries the colour and format flags.

### T8. Summary

On exit, `run` emits `Ended(Summary)`: run time, and per target the
cycles, max cycle time from `status`, and overruns. `fsw.console` reads
`status` beside `log` to collect them. Latched systems print below the
table. Multi-target summaries wait for phase 3 (children over NDJSON);
here each child prints its own.

Tests: snapshot of a summary with an overrun and a latched system.

## Order

T1 and T2 first: everything else renders through them. Then T5 and T6, the
most visible change. Then T3, T4, T7, and T8. Each task ends with tests
and a commit; T6 gets an adversarial review before T7.
