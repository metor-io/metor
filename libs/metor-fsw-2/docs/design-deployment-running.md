# Deployments: running

A deployment is a set of targets that are configured, built, and run
together. [Shared configuration](design-deployment-shared-config.md) made one
`target.py` emit a `Deployment { ir_version, targets: [Wiring, …] }` envelope
and gave `run`, `build`, and `package` a `--target <ns>` selector. This
document designs the running part of
[the rough design](rough-deployment-design.md): what `metor-fsw run` does
with several members and no `--target`, how their output reaches one
terminal, and what the first implementation must keep true so a later
`deploy` command can render the same deployment into per-host NixOS
configurations.

Cross-target comms and the gateway are not designed here. See "Assumptions".

## Design

`cmd_run` (`src/cli.rs`) is one pipeline today: find the file, refresh
packs, load, select, apply overrides, print the preflight, provision, resolve,
`run_for`. Every step after selection takes one `Wiring`. This document
splits that pipeline at the one place it already bends: the member set.

1. **Running one member is the leaf, and it does not change.** A process
   that runs one `Wiring` is what the CLI is today, and what a deployed host
   will run: `metor-fsw run <bundle> --target <ns> [clock overrides]`. The
   argument list of that command is a pure function of (bundle, namespace,
   overrides), `member_argv`, and it is the only thing the launcher and a
   future NixOS renderer share.
2. **Running several members is N leaves in N child processes.** The parent
   builds once, writes one bundle per member, spawns
   `member_argv` per bundle with its output piped, prefixes each line with
   the member's namespace, and waits. It is a free function over a plan,
   `Vec<Member { bundle, namespace }>`, with one thread per pipe and no
   supervisor type.
3. **The bundle is the hand-off.** A bundle is already "one member, its
   frozen IR, its libraries, cargo-free" ([packaging.md](packaging.md)). The
   launcher writes the same bundle `package --target` writes, into a temp
   dir, and the child runs it exactly as a deployed host would. Nothing new
   crosses the process boundary.

A member set of one runs the leaf in-process, as today. The launcher's job
is to compose leaves, and one leaf needs no composition; "One member" below
gives the reasons it is not spawned.

## Goals

- `metor-fsw run target.py` with several members runs every member, in its
  own process, from one build of the shared packs.
- Each member's lines arrive on the parent's stderr prefixed with its
  namespace, in the CLI's existing style, with colour intact on a terminal.
- One member failing fails the run: the parent stops the rest and exits
  non-zero naming the member.
- `run --target <ns>`, `run <bundle>`, and a one-member `target.py` behave
  as they do today, byte for byte on the terminal.
- The way one member is run is a pure function the NixOS renderer can
  emit; the deployable unit is the bundle `package --target` already writes.

## Non-goals

- `Publish`/`Subscribe`, address allocation, mDNS naming from the namespace,
  and readiness ordering between members (cross-target comms, discovery).
- The gateway.
- The NixOS renderer itself, host assignment, and the `metor-fsw`
  derivation. One section below states what this implementation keeps true
  for it.
- A graceful stop on `SIGINT`/`SIGTERM`. The coordinator has no signal
  handling today (no `ctrl_c`, `signal`, or `SIGTERM` anywhere under
  `src/`), and this document does not add one; see "Decisions".

## Assumptions

- Each member still declares its own `TcpServer(addr=)` and its mDNS `name`
  (`LinkParams`, `src/telemetry/link/mod.rs`), and binds it at resolve. Two
  members on one host with the same address fail at the second bind, in the
  child, with the existing resolve error.
- The envelope's per-member host table, reserved by the shared-config doc
  for discovery, does not exist yet. The launcher does not read it; a local
  run places every member on this host.
- Process systems inside a member (`docs/process-systems.md`) are the
  member's own business. The launcher spawns members; the member's
  coordinator spawns its workers.

## The leaf

`run <path> --target <ns>` with a bundle path is unchanged: `load_bundle`
(`src/wiring/bundle.rs`), `select` against the frozen namespace,
`apply_overrides`, `print_preflight`, `resolve`, `run_for(cycles)`, then the
hard-stop check that turns stopped systems into a non-zero exit
(`src/cli.rs`, `cmd_run`). Two additions:

- `--no-preflight` skips `print_preflight`. The launcher prints every
  member's preflight itself, in order, before any child starts, so the
  blocks do not interleave (below).
- `ui::init_tracing` gates ANSI on the same `supports-color` decision
  `if_supports_color` already uses, instead of `stderr().is_terminal()`
  (`src/cli/ui.rs`). That honours `CLICOLOR_FORCE`, `FORCE_COLOR`, and
  `NO_COLOR` in the fmt layers as `owo-colors` and miette already do.

Nothing about the coordinator, the link server, or the bundle loader
changes. A leaf is a foreground process that logs to stderr, never forks,
and exits non-zero when a system hard-stops. That contract is what both the
launcher and a systemd unit rely on.

## The launcher

### Plan

`cmd_run` builds the member set first:

- one source `.py`: `refresh_source_packs` once, `load_source` once,
  `validate_deployment` (already inside `ingest_ir`), then `--target`
  selects one member or, with no flag, every member in envelope order;
- one or more bundles: `load_bundle` each, wrap them in a `Deployment` (as
  `load_run_wiring` already does for one), `validate_deployment`, then
  select as above. A bundle set gets the same namespace rules as a file:
  every member named, distinct, no dotted prefix.

One member runs in-process. Several become a plan:

```rust
/// One member the launcher runs: its bundle and the namespace it must match.
struct Member { bundle: PathBuf, namespace: String }
```

For a source file the parent provisions every member as `cmd_build` does
(`provision_artifacts`, or `locate_artifacts` under `--no-build`), then
`write_bundle`s each into `<tempdir>/<ns>.bundle` with the file as
provenance. For a bundle set the plan is the paths as given. The temp dir
lives until the launcher returns; a `SIGINT` skips its drop and the OS temp
cleanup reclaims it, the precedent `load_bundle` sets for `.metor` unpacks.

### One command per member

```rust
/// How one member runs, anywhere: the leaf's argument list.
fn member_argv(bundle: &Path, namespace: &str, overrides: &Overrides) -> Vec<OsString>
```

renders `run <bundle> --target <ns> --no-preflight` plus the forwarded
overrides. `Overrides` is the clock subset of `RunArgs` (`--wall`,
`--sim-dt`, `--cycle-rate`, `--cycles`); it exists so the function's input
is data, not the whole `RunArgs`. The executable is
`std::env::current_exe()`, as `src/proc/host.rs` re-executes the host for a
worker. The child inherits cwd and environment; the parent adds
`CLICOLOR_FORCE=1` when its own stderr supports colour, and nothing else.
`RUST_LOG` flows through unchanged, so a filter set on the parent filters
every member.

### Overrides

Every clock override applies to every member, exactly as it applies to the
whole `Wiring` today: "a flag always beats the target's own setting"
(`apply_overrides`). `--cycles 100` runs each member for 100 of its own
cycles; `--wall` paces every member. The parent forwards the flags and the
child applies them, so the bundle is the unmodified member and the overrides
stay in the argument list, where the renderer can see them.

`--serve <ADDR>` names one socket and cannot apply to several members. With
several members and no `--target` it is rejected before any build:

```text
`--serve` names one socket; pick the member it applies to with --target (plant, fsw)
```

`--release`, `--no-build`, `--cargo-arg`, and `--no-manifest-sidecar` are
build flags. They act in the parent and are not forwarded; a bundle is
cargo-free.

### Launch, wait, exit

The parent spawns every member in envelope order, all before waiting on any.
Order means spawn order and nothing more: there is no readiness gate, and a
member that needs a peer up retries its own connection. That is the comms
section's problem.

Each child's stdout is dup'd onto its stderr pipe, so one pipe per member
carries both streams in the order the child wrote them (`run` writes nothing
to stdout today). One `std::thread` per member drains that pipe line by
line, writes each line prefixed under one locked `stderr` write, and on EOF
`wait`s the child and sends `(index, ExitStatus)` on an `mpsc` channel the
main thread receives from. Threads because stellarator has neither a child
nor an async pipe primitive (`libs/stellarator/src/os.rs` wraps fds; nothing
spawns or waits a process). No struct owns the threads; the launcher
function's scope does.

The verdict is fail-fast. The first non-success status kills the remaining
children (`Child::kill`), drains their pipes, and returns

```text
member `fsw` exited with status 1
member `plant` was killed by signal 9
```

as a miette error, so the parent exits 1 like a failing single run. Every
member succeeding (a `--cycles` run) returns `Ok`. An unbounded run never
ends with a success from a child, so early exit is always a failure.

Ctrl-C needs no code. The terminal delivers `SIGINT` to the foreground
process group, which is the parent, every member, and every member's
workers. Each dies as a single run dies today. The parent's pipes close with
it; a member still writing gets `EPIPE` on a stderr it was about to lose.

`Child::kill` is `SIGKILL`. A killed member with process systems leaves its
workers until they notice (the worker has no parent-death signal,
`src/proc/worker.rs`). That is pre-existing for `kill -9` of any run and is
the cost accepted under "Decisions".

### One member

`run target.py` with one member, and `run <bundle>`, run in-process. A
one-child launcher would copy every dylib on each single-target run, and a
target with no namespace cannot be selected with `--target` (`select`,
`src/cli.rs`), so a bare `target.py` would have no in-process run at all.
The split is one `match` on the member count after the member set is
built; everything above it is shared.

## Log output

### Where the prefix goes

The parent prefixes, by pipe. Telling the child its prefix leaks: panics
land on the default hook, process-system workers `eprintln!`
(`src/proc/worker.rs`), miette renders the final error, and the fmt layers
write through `tracing-indicatif`'s writer. A pipe catches all of them, and
the child does not know it is a member.

### Style

A prefix is the namespace, bold, in a member colour, padded to the widest
namespace, then a dimmed `│` and a space:

```text
plant │ …
fsw   │ …
```

Member colours cycle through the preflight's dot palette in envelope order:
bright magenta, cyan, green, yellow (`print_preflight`, `src/cli/ui.rs`). It
is applied with `if_supports_color(Stream::Stderr, …)`, so a parent whose
stderr is a file or a `NO_COLOR` terminal writes plain text.

### Colour across the pipe

A child's stderr is a pipe, so `supports-color` would say no. The parent
exports `CLICOLOR_FORCE=1` when its own stderr supports colour and leaves it
unset otherwise; the child's `owo-colors`, miette, and (after the leaf change
above) fmt layers all follow. Lines then reach the parent with their escapes
intact, and the parent adds its own around the prefix only.

### Interleaving

Whole lines only. One pump writes one line per `write_all` on the locked
stderr, so two members never share a line. Lines from different members
interleave in arrival order, which is what a user watching two processes
wants. A trailing partial line is flushed at EOF with a newline.

### Banners

The parent prints each member's preflight itself, in envelope order, before
spawning. It has every provisioned `Wiring`, and `print_preflight` already
renders `target.py · fsw` for a member. The preflight shows the effective
clock, so the parent applies the overrides to a clone for display and
forwards the same flags. The children run with `--no-preflight`.

The build spinner and cargo lines print as `run` prints them today, once,
in the parent. The parent prints nothing else of its own: no header, no
per-member exit line on success. Failure is the miette error above.

### A two-member run

```text
$ metor-fsw run target.py --cycles 600
  ⠸ build adcs-systems (compiling adcs-systems) 4s

  target.py · plant · 3 system(s) · 120 Hz · sim dt 8.333 ms

  • plant     Plant
              adcs-pack 0.1.0 · process
  • presets   Presets
              builtin
  • downlink  Downlink
              builtin

  target.py · fsw · 4 system(s) · 1 slot(s) · 120 Hz · sim dt 8.333 ms

  • nav       Nav
              adcs-pack 0.1.0
  • ctrl      Ctrl
              adcs-pack 0.1.0
  • downlink  Downlink
              builtin
  • mode      slot
              allow ▸commissioning, safe_mode

plant │ 2026-09-09T10:00:01.204Z  INFO metor_fsw_2::telemetry::discovery: advertising fsw link over mDNS name=plant port=2240
fsw   │ 2026-09-09T10:00:01.211Z  INFO metor_fsw_2::telemetry::discovery: advertising fsw link over mDNS name=fsw port=2241
plant │ 2026-09-09T10:00:03.900Z  WARN metor_fsw_2::coordinator: async boundary dropped records system="plant" dropped=3
fsw   │ system `ctrl` stopped: Panicked
fsw   │ Error: 1 system(s) hard-stopped during the run
Error: member `fsw` exited with status 1
```

The spinner line clears when the build finishes, as today. The `INFO` lines
appear only under `RUST_LOG=info`; the default filter passes `WARN`
(`init_tracing`). The preflight blocks match the `build` listing's one
bold header per member.

## Deploy rendering (future)

The rough design wants a command that renders a deployment into per-host
NixOS configurations, for real machines or microVMs. It is not designed
here. The first implementation keeps these true so it can be:

1. **`member_argv` is the contract.** How a member runs is that argument
   list and nothing else: no environment the child needs, no temp file the
   parent must keep, no pid file. A renderer emits
   `metor-fsw run /nix/store/…-fsw.bundle --target fsw` as a unit's
   `ExecStart` and gets the process the launcher gets. Changing the leaf's
   arguments means changing the renderer, so `member_argv` is pinned by a
   unit test and lives beside the CLI (`src/cli/launch.rs`).
2. **The deployable unit is the bundle `package --target <ns>` writes**, with
   `--triple` for the host. The launcher writes the same bundle with the
   same `write_bundle`; a local multi-member run rehearses what deploy
   ships. `write_bundle` stays deterministic (`built_at_unix` pinnable), so
   a bundle is a fixed-output derivation input.
3. **The leaf is supervisor-friendly.** Foreground, stderr, non-zero on hard
   stop. Fail-fast is the launcher's dev-loop policy, not the leaf's;
   systemd restarts units on its own terms. The one gap is a graceful stop
   on `SIGTERM`, which both `systemctl stop` and the launcher's fail-fast
   want; it is a deferred follow-up in the coordinator,
   not this section.
4. **Hosts live on the envelope, not in the launcher.** The shared-config
   doc reserved a per-member table keyed by namespace for discovery. Deploy
   adds its host assignment to that same table rather than a second one:
   one map, `namespace → { advertised name, address, host, … }`, each field
   owned by the section that reads it, additive, IR bump on landing. The
   launcher reads none of it; local means every member here. Envelope order
   stays spawn order and gains no deploy meaning.
5. **Nothing in the tree builds `metor-fsw` under Nix today.** `flake.nix`
   references `nix/pkgs/metor-cli.nix`, `memserve.nix`, `metor-py.nix`, and
   `images/aleph/…`, none of which exist; `nix/pkgs/elodin-cli.nix` targets
   an `apps/metor` that is gone. The renderer needs a CLI derivation and a
   cross-built bundle per host (`package --triple`). Neither touches the
   launcher.

The renderer is then a pure function of (envelope, one bundle path per
member) to per-host configuration text. It runs no cargo and evaluates no
Python; `package` did both.

## IR and Python changes

None. `Deployment(targets=[…])`, the envelope, and `Wiring` are unchanged.

Two settings were considered and left out. A member that should not launch
with the rest is a `--target` choice at the command line; there is no
config-static "do not run me" that is true in development and in
deployment alike. A member's host is deploy's field on the envelope table
above, and no local run reads it. Neither belongs on `Target(...)`, and
`Deployment(...)` gains nothing until discovery defines the table.

## CLI

`RunArgs` (`src/cli.rs`):

```text
<PATH>...        One source `.py` deployment (built automatically), or one or
                 more bundles (cargo-free). Defaults to `target.py` in the
                 current directory.
--target <NS>    Run this member of the deployment, by namespace. With
                 several members and no --target, every member runs in its
                 own process with its output prefixed by its namespace; on a
                 bundle it must match the frozen namespace.
--no-preflight   Skip the pre-flight listing.
--serve <ADDR>   Serve the telemetry link on this address, overriding the
                 target's `TcpServer` state (or declaring one, with an
                 all-taps downlink, when the target has none). Applies to
                 one member; with several, requires --target.
```

Errors, all before any build:

```text
`--serve` names one socket; pick the member it applies to with --target (plant, fsw)
`run` takes one source `.py` or bundles, not both: `target.py`, `fsw.metor`
bundles: member 0 (`plant.metor`) has no namespace; every member of a deployment needs one
bundles: duplicate namespace `fsw`
```

The last two are `validate_deployment`'s existing faults rendered against
the bundle set instead of a file (`select`'s fallback arm today). The
`TargetRequired` message from the shared-config doc is retired: no
`--target` on several members is now the launch.

After the run:

```text
member `fsw` exited with status 1
member `plant` was killed by signal 9
```

`docs/cli.md` gains, under "Deployments":

```sh
metor-fsw run target.py                       # every member, one process each
metor-fsw run dist/plant.metor dist/fsw.metor # a packaged deployment, cargo-free
metor-fsw run target.py --target fsw          # one member, in this process
```

`build` and `package` do not change.

## Alternatives considered

Threads in one process, one coordinator per thread. Rejected: a `Wiring` is
one coordinator, one clock, and one process (shared-config doc, "Flatten
every member"); the `logfwd` global queue, `set_now`, process-system
sessions, and the packs' per-dylib statics all assume one coordinator per
address space. It is also not what deploy runs.

Children re-evaluate the source, `run target.py --target <ns> --no-build`
per member. Rejected: N CPython evaluations of one file, the build profile
threaded to every child, and a child input the renderer cannot reuse.

A provisioned `wiring.json` hand-off with absolute artifact paths, read by
the child through a hidden flag or environment variable. Rejected: a third
input form for `run` with its own loader and no integrity checks, to save
the dylib copy the bundle costs. If the copy is measured to hurt,
`write_bundle` can grow a hard-link option below this design.

The child styles its own prefix. Rejected under "Where the prefix goes".

Keep-running on a member failure behind a `--fail-fast` flag. Rejected: a
half-deployment is not a state anyone asked for, and the survivors' peers
are gone. One policy, no flag.

Readiness-ordered launch. Rejected: the leaf has no notion of ready a peer
could wait on, and giving envelope order that meaning binds a comms
decision now.

A dedicated subcommand (`run-all`, `up`). Rejected: the shared-config doc
reserved `run` with no `--target` for this, and "run the file" reading as
"run what the file declares" needs no new verb.

## Decisions

The review accepted the proposals above as written:

1. A graceful stop on `SIGINT`/`SIGTERM` (a stop flag `run_for` checks per
   cycle, set from `cmd_run`) is a separate follow-up work package, deferred
   past this section. When it lands, the launcher's fail-fast moves from
   `SIGKILL` to `SIGTERM`.
2. The per-run dylib copy into temp bundles is accepted; measure it on the
   example's debug build, and hard-link inside `write_bundle` only if it
   shows.
3. Several bundle paths as `run`'s input are in scope.
4. `--no-preflight` is a documented flag.
5. `--cycles`, `--wall`, `--sim-dt`, and `--cycle-rate` apply to every
   member; only `--serve` is rejected without `--target`.
6. No address-collision pre-check; the losing member's bind error arrives
   under its prefix.
7. `examples/adcs-fsw2/target.py` stays one member until cross-target comms
   land; `tests/fixtures/` carries this section's two-member runs.

The implementation plan is [plan-deployment-running.md](plan-deployment-running.md).

## Testing strategy

Unit, no subprocess (`src/cli/launch.rs` tests):

- `member_argv`: a bundle path, a namespace, and each override combination
  render the expected argument list; `--serve` is not representable in
  `Overrides`. This test is the renderer's pin.
- Plan construction over a two-member `Deployment` built with
  `WiringBuilder`: two `Member`s in envelope order, bundle paths under the
  temp dir named by namespace; a one-member set yields no plan.
- The `--serve`-without-`--target` rejection and the source-plus-bundle
  rejection, from `RunArgs` values.
- The prefix: padding to the widest namespace, colour cycling, plain text
  under `NO_COLOR`.
- The line pump over a `Cursor`: prefixed lines, a trailing partial line
  gets its newline, an embedded escape sequence passes through.
- The verdict over a list of `(namespace, ExitStatus)`: all success is
  `Ok`; the first failure names its member and status; a signal renders as
  such.

Integration (`tests/launch.rs`, skipping without a `python3 >= 3.10` on
PATH as `tests/py_eval.rs` does, invoking `env!("CARGO_BIN_EXE_metor-fsw")`):

- `run tests/fixtures/launch_target.py --cycles 20` exits 0; stderr holds
  both preflight headers, `launch_target.py · a` before `· b`, and no `│`
  line without a known prefix. (`deployment_target.py` is an eval-only
  fixture whose edges do not resolve; the launch fixture uses the real
  `TcpServer`/`Downlink`/`Alarms` builders so each member runs.)
- A fixture whose two members declare the same `TcpServer` address, run
  with no `--cycles`: exits 1 within a timeout, stderr names the losing
  member's bind error under its prefix and ends with ``member `<ns>` exited
  with status 1`` for whichever member loses the bind. This proves fail-fast
  without a fixture that panics.
- `package --target a` and `--target b` from the fixture, then
  `run a.bundle b.bundle --cycles 20`: exits 0 with both prefixes; the
  bundles run cargo-free.
- `run … --target a --cycles 20`: exits 0 and stderr contains no `│`; the
  single-member path is untouched.

Example: `examples/adcs-fsw2/tests/bundle.rs` keeps passing with no edit to
`target.py`; the example stays one member ("Decisions", 7).
