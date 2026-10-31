# CLI 4. `explain`, `graph`, completions, and `new`: implementation plan

Phase 4 of `docs/cli-design.md`.

## Facts the plan relies on

- `diagnostic.rs` maps every error to an `M` code in one `match` per error
  type. Codes group by family: `M01xx` names and flags, `M02xx` the target
  file and deployment, `M03xx` the coordinator build, `M04xx` packs and
  cargo, `M05xx` the run.
- A `ResolvedDeployment` holds each target's systems in step order. Each
  `InputConfig` names its sources as `system.port` in the same target. An
  input fed by a later system is a loop.
- Links are resolved to addresses: a dialing `fsw.subscribe` or
  `fsw.publish` holds a `TcpDial { addr }` grant, a listening one a
  `TcpListen { addr }`. The peer's name is gone, so a link is found by
  matching a dial's port to a listener's port in another target.
- `show::select` and its `names` already list every target and system name.
- A dev pack is a directory with a `pyproject.toml` holding
  `[tool.metor.pack]` and a `Cargo.toml` building a `cdylib` that calls
  `export_pack!`. `metor run` builds dev packs named in the project's
  `[tool.uv.sources]`.

## Tasks

### T1. `explain`

`metor explain M0201` prints the long form of a code: a title, what it
means, its usual causes, and the fix. `metor explain` lists every code
and its title as a table. An unknown code is `M0106`, with a suggestion.

The texts live in `src/cli/codes.md`, one `## M0201 Title` section per
code, embedded with `include_str!` and split at load. A test checks that
every code `diagnostic.rs` returns has a section, and every section a
code.

A small Markdown renderer writes the subset the texts use, through
`Theme`: headings in `Accent`, paragraphs wrapped to the width, `inline
code` in `Accent`, fenced code indented and `Muted`, and `-` lists with
the bullet glyph. It is our own rather than `termimad`, so it uses our
tokens and glyphs and adds no terminal crate.

Each human diagnostic ends with `→ metor explain M0201` in `Faint`, after
the help line.

Tests: the parser over a section with each element; render snapshots
plain and at 60 columns; the code coverage test; an unknown code.

### T2. `graph`

`metor graph [FILE] [TARGET]` draws each target's wiring. Targets stack
vertically, each under its `show` title line, so the picture stays as
narrow as one target. This departs from the design doc's side-by-side
columns, which need routing across the page.

Each system is a box in step order. A box has a title row, `id` bold and
kind `Muted`, then a row per input port with a source and a row per
output port with a consumer. Edges run in lanes to the left of the boxes,
as `git log --graph` does:

```
  fsw  100 Hz wall

         ╭──────────────────────────────╮
         │ plant    fsw.subscribe       │  ⇄ plant.pub
     ╭───┤ sensors                    ▸ │
     │   ╰──────────────────────────────╯
     │   ╭──────────────────────────────╮
     │   │ nav      adcs_pack.Nav       │
     ╰──▶┤ sensors                      │
       ╭─┤ estimate                   ▸ │
       │ ╰──────────────────────────────╯
       │ ╭──────────────────────────────╮
       │ │ ctrl     adcs_pack.Ctrl      │
       ╰▶┤ estimate                     │
         ╰──────────────────────────────╯
```

- One lane per output port, from its row to its last consumer's input
  row, tapped with `├▶` at each consumer between. A loop's lane runs up.
- Lanes are packed greedily: a lane takes the innermost column free over
  its span, so short edges hug the boxes. A tap that crosses an inner
  lane draws `┼`.
- A link system shows `⇄ target.system` beside its box, found by port.
- Lanes are `Accent` and box borders `Muted`; each target's title is in
  its hue. ASCII mode uses
  `+-|>` and `<`/`>`.

The layout is a pure function from a target to a grid of styled cells;
drawing is separate. `--as mermaid` and `--as dot` write text instead:
one subgraph or cluster per target, an edge per input labelled with its
port, and a dashed edge per link. `--format json` writes one `graph`
event of nodes and edges.

Tests: lane packing over nested, disjoint, and loop spans; snapshots of
the fixture target and a two-target deployment, Unicode and ASCII;
Mermaid and DOT snapshots; an unknown target name.

### T3. Completions

`metor completions SHELL` prints the registration script for bash, zsh,
fish, elvish, or PowerShell. It uses `clap_complete`'s dynamic engine
(`unstable-dynamic`), so the script calls back into `metor` to complete.

Commands and flags complete from the clap definition. `--target` and
`show`/`graph` names complete from a cache: every successful `prepare`
writes the deployment's names to `.metor/names` beside the target file,
one per line. Completion reads the nearest one and never evaluates
Python, so it stays fast.

Tests: the names file written by `check` on the fixture; candidates from a
cache; candidates with no cache; each shell's script names `metor`.

### T4. `new`

`metor new target NAME` makes a runnable project in `NAME/`:

```
NAME/
  pyproject.toml     metor-config, and the pack as a path source
  target.py          one target at 10 Hz wall running `hello`
  systems/NAME/      a pack, as `new pack` writes it
```

`metor new pack NAME` writes a pack crate in `NAME/`: `Cargo.toml`
(`cdylib`, an empty `[workspace]` so it stands alone), `pyproject.toml`
with `[tool.metor.pack] id`, and `src/lib.rs` with one system that counts
cycles and logs every hundredth. Run inside a project, it prints the
`[tool.uv.sources]` line to add.

The metor dependencies point at the source tree this binary was built
from, as relative paths, when it still exists, and otherwise at the git
repository's default branch; the repository has no release tags to pin. Names are checked as Python and Rust
identifiers once hyphens become underscores; an existing directory is
refused. Files are templates with `{name}`-style holes, embedded with
`include_str!`.

Tests: the rendered files for a name; bad names and an existing
directory; an integration test that runs `metor new target` and then
`metor check` in it.

## Order

T1, T2, T3, T4. Each task ends with tests and a commit; the phase ends
with an adversarial review.
