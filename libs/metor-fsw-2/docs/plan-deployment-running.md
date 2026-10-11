# Plan: deployments, running

Implements [design-deployment-running.md](design-deployment-running.md).
Five work packages. Each ends with a green tree. Paths are relative to
`libs/metor-fsw-2` unless they start with `examples/` or `libs/`.

Test commands used throughout:

```sh
cargo test -p metor-fsw-2                       # Rust unit + integration
cargo clippy -p metor-fsw-2 --all-targets       # lints, house rule
cargo test -p metor-fsw-2 --lib cli             # the CLI's unit tests alone
cargo test -p metor-fsw-2 --test launch         # WP4's integration test
cargo test -p adcs-fsw2                         # example targets (skip w/o python3)
```

## Sequencing

```text
WP1 ─┬─> WP2 ──┐
     └─> WP3a ─┴─> WP3b ─> WP4 ─┬─> done
                                └─> WP5
```

WP1 lands first: it creates `src/cli/launch.rs` and the `Overrides` type
both later packages use. WP2 (member set, plan) and WP3a (the launcher's
pure pieces: prefix, pump, verdict) touch disjoint code and can run in
parallel. WP3b (`launch` itself, and the `cmd_run` arm that calls it) needs
both. WP4 needs WP3b. WP5 is prose and can go alongside WP4.

## Deviations from the design doc

Found while grounding the plan.

1. `tests/py_eval.rs` does not gate on `$METOR_PYTHON`; it gates on a
   `python3 >= 3.10` on `PATH` (`have_python()`) and *unsets*
   `$METOR_PYTHON` so the interpreter resolution is the default one
   (`resolve_interpreter`, `src/wiring/py.rs`). WP4 copies that gate. The
   design doc's testing section was corrected to say so.
2. `tests/fixtures/deployment_target.py` and `trivial_target.py` are
   eval-only fixtures: their edges (`m.connect(a.out, b.in_)` on an
   `Alarms`) do not resolve. WP4 adds fixtures built from the real
   `TcpServer`/`Downlink` builders (`metor_config/_builtins.py`) so each
   member resolves and runs.
3. `select`'s `TargetRequired` arm (`src/cli.rs`) stays. The design doc says
   the message is "retired"; that is true for `run`, which no longer calls
   `select` with `None` on several members, but `package` still does and
   still needs it. Only `docs/cli.md`'s wording changes (WP5).
4. On a fail-fast kill the parent does not drain the killed members' pipes
   to EOF. A killed member's process-system workers inherit the pipe and,
   with no parent-death signal (`src/proc/worker.rs`), could hold it open
   indefinitely; the parent `wait`s the children it killed and returns,
   letting the pump threads die with the process. The success path drains
   to EOF as designed.

## Verified facts

| Fact | Where | Consequence |
| --- | --- | --- |
| `[[bin]] name = "metor-fsw"` | `Cargo.toml:95` | integration tests use `env!("CARGO_BIN_EXE_metor-fsw")` |
| `owo-colors 4.3.0` → `supports-color 3.0.2`; `miette 7` → `supports-color 2.1.0` | `Cargo.lock` | both check `FORCE_COLOR`, then `CLICOLOR_FORCE`, before `NO_COLOR`; `IGNORE_IS_TERMINAL` exists only in 3.x |
| `supports-color` is not a direct dep | `Cargo.toml` | WP1 adds `supports-color = "3"`; no new lock entry |
| toolchain 1.98.0 | `rust-toolchain.toml` | `std::io::pipe()` and `Stdio: From<PipeWriter>` are available |
| `tempfile` is a regular dep | `Cargo.toml:74` | the launcher's temp dir needs nothing new |
| `init_tracing` gates ANSI on `stderr().is_terminal()` | `src/cli/ui.rs:51` | WP1 switches it to `supports_color::on(Stderr)` |
| `AlarmsParams::alarm` is serde-defaulted | `src/alarm/mod.rs:34` | a fixture member may carry `Alarms(alarms=[])` if the resolver wants a cyclic system |
| `run` writes nothing to stdout | `src/cli.rs`, `cmd_run` | one pipe per child can carry both streams |
| `Command::spawn` for workers inherits stderr | `src/proc/host.rs:209` | a member's workers write into the member's pipe; see deviation 4 |

The env var the parent sets is `CLICOLOR_FORCE=1`, only when its own stderr
supports colour. Both `supports-color` versions in tree read it, so
`owo-colors` (preflight, prefix), miette (the child's final error), and the
fmt layers (after WP1) agree. `FORCE_COLOR` is not used: it carries a level
(`1`/`2`/`3`) and would cap a truecolor terminal at 16 colours for anything
that reads the level. A user-set `FORCE_COLOR` is inherited and wins
regardless, in both versions.

---

## WP1: leaf changes and `member_argv`

Files:

- `Cargo.toml`: add `supports-color = "3"` under the cli group.
- `src/cli/ui.rs`: `init_tracing`'s `let ansi = std::io::stderr().is_terminal();`
  becomes `supports_color::on(supports_color::Stream::Stderr).is_some()`.
  Drop the `IsTerminal` import (line 13); nothing else uses it.
- `src/cli.rs`:
  - `RunArgs` gains `/// Skip the pre-flight listing.` `#[arg(long)] no_preflight: bool`.
  - `cmd_run` wraps `ui::print_preflight` in `if !args.no_preflight`.
  - `mod launch;` beside `mod ui;`.
  - The `RunArgs` literal in `refresh_run_packs_skips_and_noops` gains
    `no_preflight: false`.
- `src/cli/launch.rs` (new): module doc (one paragraph: the leaf's argument
  list is the contract the launcher and a renderer share), then:

  ```rust
  /// The clock overrides `run` forwards to every member.
  pub(super) struct Overrides { wall: bool, sim_dt: Option<f64>, cycle_rate: Option<f64>, cycles: Option<usize> }
  pub(super) fn overrides(args: &RunArgs) -> Overrides
  /// How one member runs, anywhere: the leaf's argument list.
  pub(super) fn member_argv(bundle: &Path, namespace: &str, overrides: &Overrides) -> Vec<OsString>
  ```

  `member_argv` renders `run <bundle> --target <ns> --no-preflight` then
  `--wall` | `--sim-dt <secs>` (never both; `RunArgs` already groups them),
  `--cycle-rate <hz>`, `--cycles <n>`. Floats render with `{}` so the child
  parses what the parent had.

Tests to add (`src/cli/launch.rs` test module):

- `member_argv_is_the_leaf_contract`: no overrides → exactly four
  arguments; each override appears with its value; `--wall` and `--sim-dt`
  never both. This is the renderer's pin; its doc comment says so.
- `overrides_from_args`: a `RunArgs` literal maps field for field.

What could go wrong: `tracing-subscriber`'s `with_ansi` is the only colour
gate in the fmt layers; `tracing-indicatif`'s writer passes bytes through
and hides its bars when stderr is not a terminal on its own (indicatif's
draw-target check), so forcing ANSI does not resurrect progress bars in a
pipe. `supports_color::on` is uncached and reads the environment on every
call; call it once in `init_tracing`.

Green when:

```sh
cargo test -p metor-fsw-2 --lib cli && cargo clippy -p metor-fsw-2 --all-targets
```

## WP2: the member set, the plan, temp bundles

Files:

- `src/cli.rs`:
  - `RunArgs::path: Option<PathBuf>` → `paths: Vec<PathBuf>`, doc from the
    design's CLI section. Positional `Vec` is optional in clap derive; empty
    means `detect_target()`.
  - `cmd_run` becomes: build the member set, then one `match` on its
    length. Concretely:
    - `classify(&paths) -> Result<Input>` where `Input` is
      `Source(PathBuf)` | `Bundles(Vec<PathBuf>)`: one `.py`, or ≥1
      `is_bundle` paths, else
      ``run` takes one source `.py` or bundles, not both: `a`, `b``. An
      enum, not a struct; it is the one place the two inputs differ.
    - Source: `refresh_run_packs` once, `load_source`, then `--target` →
      `vec![select(..)?.clone()]`, else `deployment.targets.clone()`.
    - Bundles: `load_bundle` each into a `Deployment { ir_version: IR_VERSION, targets }`,
      `validate_deployment`, then select as above. `load_run_wiring` dissolves
      into this; its bundle arm was the one-bundle case of it.
    - `--serve` with several members and no `--target` → the design's
      error, before any provisioning. The available namespaces come from
      the members.
    - One member: the existing tail (`apply_overrides`, preflight,
      `provision_run_artifacts` for a source, `resolve`, `run_for`, the
      hard-stop check), unchanged.
    - Several: for a source, `provision_run_artifacts` per member, then
      `write_bundle` each into `tempdir.path().join(format!("{ns}.bundle"))`
      with `PackageOptions { release, target: build_target(&args.cargo_arg), provenance: Some(source), built_at_unix: None }`;
      for bundles, the paths as given. Collect `Vec<launch::Member>`. In
      this package the arm then returns today's
      `pick one with --target` error via `select(&deployment, path, None)`;
      WP3b replaces that line with `launch::launch`.
  - `select`'s error mapping moves into `fn deployment_fault(label: &str, err: LoadError) -> miette::Report`
    so the bundle-set `validate_deployment` failure renders through the same
    arms with label `bundles`. `NamespaceRequired { index }` names
    `paths[index]` in that arm, per the design's message.
  - `refresh_run_packs` and `provision_run_artifacts` keep `path: &Path`;
    the source arm passes its one path. Their `is_bundle` checks become
    dead on the bundle arm (never called there) and can go.
- `src/cli/launch.rs`: add
  `pub(super) struct Member { pub bundle: PathBuf, pub namespace: String }`
  and `pub(super) fn plan(members: &[Wiring], bundles: &[PathBuf]) -> Vec<Member>`
  pairing each member's namespace with its bundle path. `plan` is pure
  over paths; `cmd_run` does the writing. (Two members, two bundle paths;
  a zip.)

Tests to add (`src/cli.rs` and `src/cli/launch.rs` test modules):

- `classify_paths`: `["target.py"]` → `Source`; `["a.bundle", "b.metor"]` →
  `Bundles`; `["target.py", "a.bundle"]` → the error; `[]` → handled by
  the caller (`detect_target_in` is already tested).
- `serve_needs_a_target_on_several_members`: two `WiringBuilder` members
  and `serve: Some(..)`, `target: None` → the exact message; with
  `target: Some("fsw")` → `Ok`.
- `plan_pairs_members_with_bundles_in_order`: two members → two `Member`s,
  envelope order, `<ns>.bundle` names.
- `bundle_set_validation_names_the_file`: two loaded members, one without a
  namespace → ``bundles: member 0 (`plant.metor`) has no namespace…``.
  Build the `Deployment` in memory; no bundle on disk is needed for the
  rendering.
- `write_bundle` on artifact-less `WiringBuilder` members into a
  `tempfile::tempdir()` and `load_bundle` back: the temp-bundle path works
  end to end without cargo (`bundle.rs` already tests this shape; one
  assertion that the namespace round-trips is enough).

What could go wrong: clap only rejects a malformed argument tree at
runtime; `command_tree_is_well_formed` catches a positional `Vec` that
conflicts with `--target`. The `RunArgs` literal in
`refresh_run_packs_skips_and_noops` changes again (`paths: Vec::new()`).
`load_bundle` on a `.metor` unpacks into a temp dir it deliberately keeps
(`bundle.rs`, "leaking the temp dir is the cost"); the parent loads N
archives to read namespaces and the children unpack them again, so a
`.metor` set leaks N dirs per run, the same as N single runs today. The
launcher's own `TempDir` must outlive `launch` (WP3b): bind it in `cmd_run`,
not inside a helper that returns before the children start.

Green when:

```sh
cargo test -p metor-fsw-2 --lib cli && cargo clippy -p metor-fsw-2 --all-targets
```

## WP3a: prefix, pump, verdict (pure)

Files:

- `src/cli/launch.rs`:

  ```rust
  /// One padded, styled prefix per member: bold namespace in a cycling colour, dimmed `│`.
  fn prefixes(members: &[Member], colour: bool) -> Vec<String>
  /// Copy `from` to `to` a line at a time, each under `prefix`; a trailing partial line gets its newline.
  fn pump(from: impl BufRead, prefix: &str, to: &mut impl Write) -> io::Result<()>
  /// The run's outcome from every member's exit status: the first failure names its member.
  fn verdict(results: &[(&str, ExitStatus)]) -> miette::Result<()>
  ```

  `prefixes` cycles `bright_magenta`, `bright_cyan`, `bright_green`,
  `bright_yellow` from `owo_colors::Style`, the preflight's palette
  (`print_preflight`, `src/cli/ui.rs`), and pads before styling, as the
  preflight does ("ANSI escapes would defeat a format width"). The `colour`
  argument is passed in so the tests control it; `launch` passes
  `supports_color::on(Stderr).is_some()`.

  `pump` reads with `read_until(b'\n')`, writes `prefix + bytes` in one
  `write_all` per line (the caller hands it a locked `Stderr` in WP3b), and
  passes bytes through untouched: lossy UTF-8 is not applied, the escapes
  a coloured child emits are the point.

  `verdict` renders ``member `fsw` exited with status 1`` from
  `ExitStatus::code()`, and ``member `plant` was killed by signal 9`` from
  `std::os::unix::process::ExitStatusExt::signal`.

Tests to add (`src/cli/launch.rs` test module):

- `prefixes_align_and_cycle`: `plant`/`fsw` pad to width 5 with
  `colour: false` giving `plant │ ` and `fsw   │ `; with `colour: true` the
  namespaces differ in escape sequence and the `│` is dimmed.
- `pump_prefixes_every_line`: a `Cursor` over `"a\nb"` → `"P a\nP b\n"`; an
  embedded `\x1b[31m` passes through; an empty reader writes nothing.
- `verdict_names_the_first_failure`: all `from_raw(0)` → `Ok`;
  `[("plant", 0), ("fsw", 1 << 8)]` → the status message; `from_raw(9)` →
  the signal message. `ExitStatusExt::from_raw` is unix-only; the module is
  already unix-shaped by `std::io::pipe` use, so `#[cfg(unix)]` the test.

What could go wrong: `ExitStatus::from_raw` takes the raw `wait` status, so
an exit code is `code << 8` and a signal is the bare number. `owo-colors`'
`if_supports_color` reads the environment; the tests use the `colour` bool
and `Style` directly so `NO_COLOR` on the test host does not flip them.

Green when:

```sh
cargo test -p metor-fsw-2 --lib cli::launch && cargo clippy -p metor-fsw-2 --all-targets
```

## WP3b: `launch`

Files:

- `src/cli/launch.rs`:

  ```rust
  /// Run every member in its own process and wait; the first failure stops the rest.
  pub(super) fn launch(members: &[Member], overrides: &Overrides) -> miette::Result<()>
  ```

  Body, in order, all in one function with no owning type:
  1. `exe = std::env::current_exe()`, `colour = supports_color::on(Stderr).is_some()`,
     `prefixes(members, colour)`.
  2. Per member: `let (reader, writer) = std::io::pipe()?;`
     `Command::new(&exe).args(member_argv(..)).stdin(Stdio::null()).stdout(Stdio::from(writer.try_clone()?)).stderr(Stdio::from(writer))`,
     plus `.env("CLICOLOR_FORCE", "1")` when `colour`. Spawn; push the
     `Child` onto a `Vec<Child>` and spawn one `std::thread` that runs
     `pump(BufReader::new(reader), &prefix, &mut std::io::stderr().lock())`
     per line (lock per line, not for the thread's life) and then sends
     its index on an `mpsc::Sender<usize>`. The writer ends are dropped by
     the `Command` after spawn; nothing in the parent may keep a clone.
  3. `recv()` once per member. On each index, `children[idx].wait()`; push
     `(namespace, status)`. On the first non-success: `kill()` and `wait()`
     every child not yet reported, then break.
  4. `verdict(&results)`.
- `src/cli.rs`: the several-member arm calls
  `launch::launch(&plan, &launch::overrides(&args))` after printing each
  member's preflight (`apply_overrides` on a clone, then
  `ui::print_preflight(&clone, path)`; for a bundle set the path is that
  member's bundle). The WP2 placeholder error goes.

Tests to add: none beyond WP3a's; the process behaviour is WP4's.

What could go wrong: a `PipeWriter` clone left alive in the parent means no
EOF ever arrives (the classic pipe bug); the `Command` owns both ends and
drops them on `spawn`, so do not `try_clone` into a local. A child that
exits while the parent still holds unread lines is fine: the pump reads to
EOF before signalling. A member's process-system workers inherit the pipe;
on the success path the coordinator's shutdown reaps them
(`kill_reap_reclaim`, `src/proc/host.rs`) and EOF follows; on the kill path
the parent does not wait for EOF (deviation 4). If the parent's own stderr
is closed by the terminal (`EPIPE`), `write_all` errors in the pump thread;
let the thread end, the `recv` still fires, and the run ends on the
children's statuses. `Child::kill` on an already-exited child is `Ok`;
`wait` after it reaps. `std::io::stderr().lock()` from several threads is
the intended use of the lock; hold it for one `write_all`.

Green when:

```sh
cargo test -p metor-fsw-2 && cargo clippy -p metor-fsw-2 --all-targets
```

and, by hand against the example tree (one member, in-process, unchanged):

```sh
cargo run -p metor-fsw-2 --bin metor-fsw -- run examples/adcs-fsw2/target.py --cycles 100
```

## WP4: integration

Files:

- `tests/fixtures/launch_target.py` (new): two members, `a` and `b`, each
  `Target(cycle_rate=100.0, sim_dt=0.01, namespace=..)`, a
  `link = t.state("link", TcpServer(addr="127.0.0.1:2250"))` (`2251` for
  `b`), and `t.add("downlink", Downlink(link))`. Add `Alarms(alarms=[])`
  only if `resolve` rejects a graph whose sole system is the downlink; run
  the fixture by hand once before writing the test.
- `tests/fixtures/launch_clash.py` (new): the same two members, both on
  `127.0.0.1:2252`. Whichever member loses the bind fails.
- `tests/launch.rs` (new), one `#[test]` running the cases in sequence
  (ports and the process-global cwd, the `py_eval.rs` reason), skipping
  with a note without a `python3 >= 3.10` on `PATH` (copy `have_python`
  from `tests/py_eval.rs`; `tests/common/mod.rs` is the dl-fixture
  helper set and is not the place for it). The binary is
  `env!("CARGO_BIN_EXE_metor-fsw")`; every invocation runs with
  `current_dir` set to `tests/fixtures` so `refresh_source_packs` finds no
  pyproject and is a no-op. Cases, each asserting on
  `String::from_utf8_lossy(&output.stderr)`:
  1. `run launch_target.py --cycles 20`: exit 0; `launch_target.py · a`
     appears before `launch_target.py · b`; every line containing `│`
     starts with `a │` or `b │` (stderr is a pipe here, so no escapes).
  2. `run launch_clash.py`: no `--cycles`. Spawn, poll `try_wait` for up to
     30 s, kill on timeout and fail. Expect exit 1, a prefixed line carrying
     the bind error, and the last line ``member `<ns>` exited with status 1``
     for whichever member loses the bind.
  3. `package launch_target.py --target a -o <tmp>/a.bundle` and the same
     for `b`, then `run <tmp>/a.bundle <tmp>/b.bundle --cycles 20`: exit 0
     with both prefixes.
  4. `run launch_target.py --target a --cycles 20`: exit 0 and stderr has
     no `│`.
  5. `run launch_target.py --serve 127.0.0.1:2260`: exit 1 and the
     `--serve` message; no build happens, so this is fast.

What could go wrong: the CLI evaluates the fixture with the embedded
recorder (`EMBEDDED_PACKAGE`, `src/wiring/py.rs`), so no venv is needed,
but a `VIRTUAL_ENV` on the developer's shell is honoured by
`resolve_interpreter` and must have `metor_config` importable or be
absent; the skip note should say which interpreter was tried. Ports
`2250`–`2252` must not collide with `examples/adcs-fsw2` (`2240`) or the
eval fixtures (`2240`/`2241`, never bound). A loopback bind skips mDNS
(`src/telemetry/discovery.rs`), so no advertisement lines appear in the
output. `--cycles 20` under a simulated clock finishes in milliseconds;
case 2's timeout is the only slow failure mode. On CI without `python3`
the whole test skips, as `py_eval` does.

Green when:

```sh
cargo test -p metor-fsw-2 --test launch && cargo test -p metor-fsw-2 && cargo test -p adcs-fsw2
```

## WP5: docs

Files:

- `docs/cli.md`:
  - "Run a target": add `--no-preflight` to the flag list and one sentence
    on several bundles as input.
  - "Deployments" (lines 117–137): add the three `run` lines from the
    design's CLI section; say that `run` with no `--target` runs every
    member in its own process with prefixed output; keep the
    `pick one with --target` line but attribute it to `package`; add the
    `--serve` line and the two after-run lines.
- `docs/wiring.md:88`: the sentence "The CLI selects a member with
  `--target <namespace>`" gains "or runs every member at once; see
  [cli.md](cli.md)".
- `docs/packaging.md`: no change; a bundle is still one member.
- `docs/design-deployment-running.md`: no change; it stays the rationale.

Green when the links resolve and:

```sh
cargo test -p metor-fsw-2
```
