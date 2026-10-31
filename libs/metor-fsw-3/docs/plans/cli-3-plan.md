# CLI 3. One view of a deployment, and `watch`: implementation plan

Phase 3 of `docs/cli-design.md`. The summary landed in phase 1; this phase
makes a multi-target run read as one program, and adds `metor watch`.

## Facts the plan relies on

- `run_children` spawns `metor run <envelope> --target NS` per target and
  relays each child's stdout and stderr line by line. Children get the
  parent's colour settings and render their own text.
- A child drops its phase, `Running`, and summary events, so the parent
  shows only its run time (`docs/plans/cli-1-plan.md`, T8).
- `--format json` writes one versioned object per line: `log` (secs,
  level, target, system, message, fields), `diagnostic` (code, message,
  help, file, line), `ended` (secs, interrupted, targets).
- A pack's dylib cannot be replaced in a process that loaded it, so a
  rebuild needs a new process.

## Tasks

### T1. Children over NDJSON

Children always run with `--format json`. The parent reads each child's
stdout as events and renders them through its own `Ui`:

- `log` becomes `Event::Log`, its hue from the target's place in the
  deployment;
- `diagnostic` becomes a `Report` rebuilt from its parts, reading the
  target file's line again;
- `ended` adds the child's row to the parent's summary, which lists every
  target in deployment order;
- any other line, and every stderr line (a panic message), is shown as
  `Output`.

A child drops only phase and `Running` events. The colour flags and
`COLORFGBG`/`FORCE_COLOR` no longer go to children. With the parent in
`--format json`, the same events are written again, so one stream holds
every target.

Decoding is `relay.rs`: one function per event type, each from a
`serde_json::Value`, tested without processes.

Tests: each decoder over a good and a malformed line; a two-target run's
summary has both rows; a latched child's error shows once with its target;
JSON output stays one object per line.

### T2. `metor watch`

`metor watch [run flags]` runs `metor run` with the same flags as a child
that owns the terminal, and restarts it when a source changes:

- the target file's directory, for `*.py` and `pyproject.toml`;
- each dev pack's `src/`, `Cargo.toml`, and `pyproject.toml`.

`.metor/`, `target/`, and hidden directories are ignored. Changes are
debounced by 300 ms. On a change the parent sends the child SIGINT, waits,
prints `↻ Changed <path>`, and starts it again. A child that exits on its
own is not restarted until the next change. Ctrl-C reaches both, and the
parent exits once the child has.

`notify` with `notify-debouncer-mini` watches. Choosing what to watch and
what a change means are pure functions over paths.

Tests: the watch set for a fixture project; ignored paths; a change event
mapped to the path shown.

## Order

T1, then T2. Each task ends with tests and a commit; the phase ends with
an adversarial review.
