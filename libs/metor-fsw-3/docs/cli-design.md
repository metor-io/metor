# The `metor` CLI

Status: draft.

## Today

`metor` has four commands: `run`, `build`, `pack dev`, and `abi-version`.
Output is raw: cargo's progress, the paths `pack dev` wrote, `tracing`'s
default formatter, and children's lines behind a `NS │` gutter. A failure
is one `metor: <message>` line. Errors in `target.py` are a Python
traceback, then `the target file exited with exit status: 1`.

## Goals

- **Attractive.** The CLI is the part of metor people look at most. It
  gets the care of a product UI: a designed palette, a type hierarchy,
  consistent spacing, and Unicode used with intent. `glow` and `amp` are
  the bar.
- **Quiet on success, precise on failure.** A good run prints a few lines.
  A bad one says what failed, where, and what to do.
- **One way in.** `metor run` from anywhere in a project does the right
  thing. Flags exist for the cases that need them.
- **Human on a TTY, machine on a pipe.** The same events render as styled
  text, plain text, or NDJSON.
- **No cost to the flight loop.** Rendering runs off the coordinator thread
  and allocates nothing after the run starts.
- **One vocabulary.** Target, system, port, pack, link, sequence. Commands,
  flags, and messages use these words and no others.

## Look

### Palette

Colours are semantic tokens in one `Theme`, never literals at call sites.

| Token     | Use                                          |
|-----------|----------------------------------------------|
| `text`    | primary content                              |
| `muted`   | labels, field names, timing                  |
| `faint`   | rules, borders, gutters                      |
| `accent`  | the one highlight: headings, the active item |
| `ok`      | success glyphs, nominal state                |
| `warn`    | warnings, overruns                           |
| `error`   | errors, latched systems                      |
| `hue[0..8]` | one per target, assigned in order          |

- Hierarchy comes from three text tones and bold, not from many colours.
  Most of the screen is `text` and `muted`; colour marks what changed or
  what is wrong.
- `Theme` has a dark and a light variant in truecolor. The variant comes
  from the terminal's background, queried with OSC 11, then `COLORFGBG`,
  then dark.
- Each token is one RGB colour per variant. `anstyle-lossy` converts it
  to 256 or 16 colours when the terminal lacks truecolor.
- Target hues are chosen to stay apart in both variants and under the
  common forms of colour blindness. Each also carries a letter or glyph,
  so colour is never the only signal.

### Glyphs

| Glyph            | Meaning                         |
|------------------|---------------------------------|
| `✓ ✗ ▲ ●`        | ok, failed, warning, running    |
| `○ ◌`            | idle, waiting                   |
| `⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏`     | spinner                         |
| `▏▎▍▌▋▊▉█`       | progress, to 1/8 cell           |
| `→ ← ⇄`          | wiring and links                |

There is no ASCII fallback.

### Tables

Most output is a table: `show`, `doctor`, `pack list`, the summary. A
table is aligned columns and whitespace, with no borders.

- A header row in `muted`, then rows in `text`. The key column is bold.
- Columns separate by two spaces. Numbers right-align; text left-aligns.
- A group, such as a target, is a bold title line in its hue, then its
  rows indented two columns. A blank line ends the group.
- Empty cells are blank, not `-` or `none`.
- Widths fit the terminal. The widest text column truncates with `…`
  first; nothing wraps.

### Layout

- A two-column left margin. Blank lines separate sections.
- Numbers group digits and carry units.

### Motion

- Spinners only on work that takes longer than 200 ms, redrawn at 10 Hz
  inside synchronized output (DEC mode 2026).
- When a phase finishes, its spinner becomes `✓` and the line stays. The
  scrollback reads as a clean record of the run.

### Terminal features

- File paths and error codes are OSC 8 hyperlinks: `target.py:42` opens
  the file, `M0201` opens `metor explain`.
- `NO_COLOR`, `CLICOLOR_FORCE`, and `--color auto|always|never` are
  respected. A dumb terminal or a pipe gets the `Plain` renderer.
- On GitHub Actions, diagnostics are also emitted as annotations and
  phases as log groups.

## Commands

```
metor new <pack|target> NAME   scaffold a pack crate or a target project
metor check  [FILE]            evaluate and resolve; run nothing
metor run    [FILE]            build, check, run
metor build  [FILE] [-o OUT]   write the resolved deployment
metor show   [FILE] [WHAT]     the deployment as tables
metor graph  [FILE] [WHAT]     the deployment's wiring as a picture
metor watch  [FILE]            rerun on source change
metor pack   build|show|list   pack author commands
metor doctor                   check the toolchain
metor explain CODE             the long form of an error
metor version [-v]             version, ABI, toolchain
metor completions SHELL
```

`abi-version` folds into `version -v`. `pack dev` becomes `pack build`;
`dev` stays as an alias.

### Finding the project

Like cargo, `metor` walks up from the working directory to the nearest
`target.py` whose directory has a `pyproject.toml`. `FILE` overrides it.
Every message names paths relative to the project root.

### `check`

The fast path for editors, pre-commit, and CI: build dev packs, evaluate,
resolve, build each coordinator, and stop before the first cycle. It
catches everything `run` would catch at start, in under a second when the
packs are fresh.

### `show`

```
$ metor show

  adcs-fsw3  3 targets · 2 links · 4 packs

  plant  120 Hz wall
    SYSTEM      KIND                  INPUTS
    plant       adcs_pack.Plant       torque_cmd ← loop
    pub         Publish               listen [::]:2240

  fsw  100 Hz wall
    SYSTEM      KIND                  INPUTS
    plant       Subscribe             ⇄ plant.pub
    nav         adcs_pack.Nav         sensors ← plant.sensors
    ctrl        adcs_pack.Ctrl        estimate ← nav.estimate
    commission  adcs_seqs.Commission  sequence · wasm

  gateway  db
    subscribes  plant, fsw
```

`metor show fsw.nav` prints one system: its pack, params with values, and
each port with its record type and peer.

### `graph`

The one place boxes earn their keep: a picture of the wiring, drawn with
box-drawing characters. Each target is a column of
systems in step order. Edges run down a column or across a link; loops
are the only edges that run back up, and they are labelled. That fixed
shape means no general graph layout is needed.

```
  plant                            fsw
  ╭─────────────╮                  ╭─────────────╮
  │ plant       │──sensors──╮      │ plant   sub │
  ╰─────────────╯           │      ╰──────┬──────╯
        ▲   ╭─────────────╮ │             │
        │   │ pub         │◀╯   ⇄ link ──▶│
        │   ╰─────────────╯        ╭──────▼──────╮
        │                          │ nav         │
        │                          ╰──────┬──────╯
        │                          ╭──────▼──────╮
        ╰─── torque_cmd (loop) ◀───│ ctrl        │
                                   ╰─────────────╯
```

`--as mermaid` writes text for docs and other tools instead.

### `doctor`

```
$ metor doctor

  ✓ python         3.12.6    .venv/bin/python
  ✓ uv             0.5.4
  ✓ cargo          1.91.0
  ✗ wasm32         not installed
                   → rustup target add wasm32-unknown-unknown
  ✓ metor-config   in repo · ABI 7
```

The same checks run lazily inside other commands, so a missing wasm target
fails with the fix, not a cargo dump.

## Output

### Phases

Each step before the first cycle is one line. Cargo's output collapses to
a single line with a gauge, and comes back in full only on failure or
`-v`.

```
$ metor run

  ⠹ Building    adcs_pack  ▕██████▋     ▏ 41/63
```

and once done:

```
  ✓ Built       adcs_pack, adcs_seqs                2.1s
  ✓ Evaluated   target.py                           0.3s
  ✓ Resolved    3 targets · 2 links
  ● Running     plant  fsw  gateway        ctrl-c to stop
```

### Logs

Each line is time, level, source, message, and fields in `muted`. The
source is the system's path, `target.system`: the target in its hue, the
system in `text`. It is one aligned column, the same name `show` and the
summary use. A deployment of one target drops the target. Time is
mission elapsed by default, or sim time on a simulated clock; `--time
utc` switches.

```
  +00:00.008     plant.pub        listening on [::]:2240
  +00:00.012     fsw.plant        connected to [::1]:2240
  +00:01.500  ▲  fsw.ctrl         saturated  axis=z  cmd=0.021
  +00:02.004  ✗  fsw.commission   latched off  step=detumble
```

Info carries no level glyph. Warnings and errors tint the message too.
An overrun and a latched system are each a log line, so the log is the
whole record of a run.

### Summary

On exit, a table says how the run ended:

```
  ■ Stopped after 12.0 s

    TARGET   CYCLES   MAX CYCLE   OVERRUNS
    plant     1 440     0.52 ms
    fsw       1 200     1.10 ms          2

  ✗ fsw.commission latched off at +00:02.004 in step detumble
```

### Errors

Errors are `miette` diagnostics: a code, a headline, the source location,
and a help line, rendered by miette's graphical handler, with a footer
naming `metor explain` and the code. A config error points at the `target.py` line that added the system;
the Python recorder keeps each call site for this. A Python exception
shows only the frames in the user's files.

```
M0201

  × `nav` has no input port `sensor`
    ╭─[target.py:42:28]
 41 │ plant = fsw.add("plant", Plant())
 42 │ nav = fsw.add("nav", Nav(sensor=plant.sensors))
    ·                          ───┬──
    ·                             ╰── did you mean `sensors`?
    ╰────
  help: `Nav` takes `sensors`, `params`

  more: metor explain M0201
```

Names typed on the command line get suggestions too:

```
M0101

  × no target `fws`
  help: did you mean `fsw`?  targets: plant, fsw, gateway

  more: metor explain M0101
```

`metor explain M0201` prints the long form from `codes.md`, which is
wrapped at 76 columns, with code blocks muted and inline code in the
accent colour.

## Ergonomics

- **Units on flags.** `--sim-dt 10ms`, `--wall 100hz`, `--for 30s` (stops
  on clock time, next to `--cycles`), `--epoch 2026-10-29T00:00Z`. Bare
  numbers keep their current meaning.
- **`-v` / `-q`.** `-v` shows cargo and debug logs, `-vv` trace. `-q`
  prints errors only. `--log nav=debug,link=trace` filters by system, on
  top of `RUST_LOG`.
- **Exit codes.** 0 clean, 1 config or build error, 2 usage, 3 a system
  latched, 4 a child target failed, 130 interrupted.
- **`watch`.** Reruns on changes to `target.py`, pack sources, and
  `pyproject.toml`. Each rebuild is a new process, since a loaded library
  cannot be replaced.
- **Completions.** Static for commands and flags; dynamic for target
  namespaces and system names, read from the last `check`.
- **Environment.** Every `METOR_*` variable is listed in `metor help env`.
- **Help.** `--help` is styled with the same theme: grouped flags, an
  example for each command, and the defaults in `muted`.

## Machine output

`--format json` writes one NDJSON event per line to stdout: phase changes,
diagnostics, logs, per-target stats each second, and the summary. The
schema is versioned and covered by tests. Children of a multi-target run
send this stream to the parent, which renders their logs with one theme
and folds their stats into one summary.

## Architecture

The CLI is a producer of events and a set of renderers.

- `Event` is one enum: `Phase`, `Diagnostic`, `Log`, `Stats`, `Ended`.
- A `Reporter` trait takes events. `Human`, `Plain`, and `Json` implement
  it and are picked once at start from the TTY and flags.
- Commands only emit events. No command formats text.
- `Theme` holds the tokens and glyph set. Renderers style through it, so a
  palette change touches one file.
- A few small widgets compose the output: `Table`, `Gauge`, `Spinner`.
  Each writes styled lines and is tested on its own. Output is
  line-based; there is no full-screen or pinned region.
- During a run, the coordinator writes stats and log records into rings
  that already exist. A UI thread drains them and prints. Its buffers are
  sized at start; steady state allocates nothing and never blocks a cycle.
- Every error type derives `miette::Diagnostic` next to `thiserror`, with
  a code, labelled spans, help, and a `url` to `metor explain`.

## Crates

Most of these are in the workspace already or come with clap.

| Crate | Job |
|---|---|
| `miette` | Diagnostics: the derive, source spans, and the renderer |
| `strsim` | "did you mean" suggestions |
| `indicatif` | The spinner on a running phase |
| `anstyle`, `anstyle-lossy` | Styles, and RGB down to 256 or 16 colours |
| `supports-color` | Colour depth, with `NO_COLOR`, `FORCE_COLOR`, and `CLICOLOR_FORCE` |
| `terminal-colorsaurus` | The background colour, via OSC 11, to pick the theme variant |
| `unicode-width`, `terminal_size` | Column widths and the terminal width for alignment |
| `comfy-table` | Borderless tables (`presets::NOTHING`) that fit the width |
| `clap`, `clap_complete` | `Styles` for themed help; dynamic completion of target and system names (`unstable-dynamic`) |
| `jiff` | `--sim-dt 10ms` and `--for 30s` via `SignedDuration`, `--epoch` via `Timestamp` |
| `notify`, `notify-debouncer-mini` | File watching for `watch` |
| `insta` | Snapshots of rendered output |

- Durations parse as `SignedDuration` rather than `Span`, since a step
  has no calendar units, then convert to `std::time::Duration`, which
  refuses negatives. `--wall 100hz` keeps its own parser.
- The run-time log path must still allocate nothing in steady state. The
  spinner runs only during setup phases, and the other crates run only
  at setup or after the run.

## Testing

- Widgets and renderers: snapshot tests that feed fixed events and compare
  the text, at 60, 100, and 160 columns, in both theme variants,
  with and without colour.
- Commands: `trycmd` cases over the fixtures in `tests/`, covering a good
  run, a config error, a Python exception, a missing toolchain, and an
  interrupted run.
- `--format json`: a schema test so the stream stays stable.
- Visual review: `snapbox` stores styled output as SVG snapshots, so a
  PR that changes output shows the new look in its diff. `vhs` tapes
  cover the animated flows, such as spinners and the build gauge.

## Order

1. `Theme`, the widgets, `Event`, `Reporter`, diagnostics, collapsed cargo
   output, log format.
2. `check`, `show`, `doctor`, units on flags, exit codes, styled help.
3. Summary, children over NDJSON, `watch`.
4. `graph`, `new`, dynamic completions, `explain`.

## Later

Commands that act on a live deployment through the gateway: `metor tail`
for logs, `metor tlm` to query and plot recorded records, `metor seq` to
start and stop sequences, and `metor alarms`. These share the theme,
widgets, and renderers, so they look and behave like the rest.
