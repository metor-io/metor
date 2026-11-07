# Target groups: implementation plan

Implements `docs/target-groups.md`. Each step is one commit that passes the
Python tests and the Rust tests it names. Steps 1 to 6 build on each other in
order. Step 7, resolver ports, changes only the resolver and the step 6
fixture, and can be dropped.

Tests cover a nominal case, an off-nominal case, and a failure case for each
behavior. Python tests run with `python3 -m unittest discover -s tests` in
`python/metor-config`. Rust integration tests that start deployments take
ports from `METOR_DEPLOY_PORTS`, as now, so parallel tests do not collide.

## 1. Dotted link ids

A link's id may be dotted, so a `Publish` or `Subscribe` can be added inside a
composite.

- `_builtins.py`: `Link._bind` accepts an id whose parts are each identifiers.
  `_endpoint(id)` returns `{id with . as _}_ep`.
- `_composite.py`: no change; `Scope.add` already passes the dotted id.
- `docs/composites.md`: drop the limit on links inside composites.

Tests:
- `test_composite.py`: a composite holding a `Publish` emits id `rack.pub` and
  resource `rack_pub_ep`; a `Subscribe` with `Peer` to that link emits
  `{"target": ns, "link": "rack.pub"}`.
- `test_composite.py`: an id with an empty or non-identifier part, such as
  `a..b` or `a.b-c`, raises `ConfigError`.
- `tests/run.rs`: a two-target fixture whose links sit in composites resolves
  and runs, and the subscriber connects. This checks that nothing in Rust
  refuses a dotted link id; the wire `link` field and `DefIndex::locate`
  compare ids as strings.

## 2. `recorder` replaces `gateway()`

- `_deployment.py`: delete `gateway`. Add `recorder` as a `@composite` in a
  new `_recorder.py`:
  `recorder(s, *, peers: Sequence[SystemHandle], listen: str, path: str | None = None) -> Recorder`.
  It adds one `Subscribe(transport=Peer(peer))` per peer, named
  `_namespace(peer)`, then `Db(listen, path, all=True)` on thread `db`.
  `Recorder` is a `NamedTuple` holding `db`.
- `__init__.py`: export `recorder` and `Recorder`; drop `gateway`.
- Move `examples/adcs-fsw3/target.py`, `tests/fixtures/echo-pack/deploy.py`,
  and `test_deployment.py` to `recorder`. The gateway target becomes a plain
  `Target` with a `rec` composite on it.
- `DESIGN.md` "Links and gateways": describe `recorder`.

Check before moving on: the gateway's subscriber ids change from `a` to
`rec.a`. `test_gateway_records_every_target` asserts the db records
`a.sampler.sample.value`, so it shows whether recorded paths depend on the
subscriber's id. If they do, the recorded names change for every user of a
gateway; stop and decide with the user. `test_build_matches_golden` changes
with the ids and its golden file is regenerated.

Tests:
- `test_deployment.py`: `recorder` on a target emits one subscriber per peer
  and a db on thread `db`; the db's commands still come from subscribers that
  peer it.
- `test_deployment.py`: a peer whose namespace is dotted gives a dotted
  subscriber id.
- `test_deployment.py`: a peer whose target has no namespace raises
  `ConfigError`; a peer named `db` collides with `rec.db` and raises.
- `tests/deploy.rs` and `examples/adcs-fsw3/tests/deploy.rs`: pass as before.

## 3. Groups in Python

New `_group.py`, modeled on `_composite.py`:

- `GroupBuild[R]` holds the deferred body. `group(fn)` returns
  `Callable[P, GroupBuild[R]]` and checks arguments at the call, as
  `composite` does.
- `Group(deployment, path)`:
  - `target(name=None, **kw) -> Target` makes `Target(namespace=path or
    path.name, **kw)` and records it on the deployment. A `namespace` keyword
    raises `TypeError`.
  - `add(name, build) -> R` runs a child at `path.name`.
  - `loop(record) -> Loop`, `path`.
- `_deployment.py`:
  - `Deployment(targets=(), hosts=None)`; `targets` becomes optional.
  - `add(name, build) -> R` runs a top-level group.
  - `_namespaces` also refuses a namespace that sits under another.
  - `_envelope` expands `hosts`: each target gets the value of the longest
    key equal to its namespace or a dot-prefix of it. A key that matches no
    target raises.
- `__init__.py`: export `group` and `Group`.

Tests in `test_group.py`:
- Nominal: a two-level tree emits `flight.cpus.cpu1` and its siblings in add
  order; a group added twice runs twice; `g.target("io")` gives `{path}.io`;
  a parent adds a `Subscribe` to a target a child returned and connects the
  child's loop, as in the doc's `flight`.
- Hosts: `{"flight": a, "flight.cpu3": b}` gives `cpu3` `b` and the other
  CPUs `a`; a group in `targets=` and one in `add` mix.
- Failure: a non-identifier name; two groups at one path; `g.target()` and a
  child group under the same path (leaf rule); a `namespace` keyword; a
  `hosts` key matching nothing; wrong arguments to a group raise `TypeError`
  at the call.
- `tests/typing/group_ok.py` and `group_bad.py`, run by `test_field_typing`:
  pyright sees the group's parameters and its result type.

## 4. Selecting targets in `metor run`

- `cli/run.rs`: `member: Option<String>` becomes `targets: Vec<String>`
  (`--target`, repeatable) and `except: Vec<String>` (`--except`,
  repeatable).
- `deploy/config.rs`: `select(&self, include, exclude) -> Result<Vec<usize>,
  ConfigError>`. A pattern matches a namespace equal to it or starting with
  `{pattern}.`. No includes means every target. A pattern matching nothing is
  `NoSuchTarget`; an empty result is a new `NothingSelected`.
- `run`: one selected target runs in this process; several go to
  `run_children` with the selected namespaces. `source_width` and the relay
  cover only those. A child still gets `--target {namespace}`, which matches
  only itself under the leaf rule.
- `cli/completions.rs`: the cache lists every group path as a `target`
  name.
- `cli/mod.rs` help and `codes.md`: the new flags and error.

Tests:
- `deploy/config.rs` unit tests: a prefix selects its subtree; `flight` does
  not match `flightx`; `--except` removes; repeated patterns; an unmatched
  pattern and an empty result are errors.
- `tests/run.rs`: a three-target fixture with dotted namespaces; `--target`
  a group runs exactly its two targets, by the summary table; `--except`
  leaves one out; an unmatched pattern exits with M0102.

## 5. Selecting in `metor show`

- `cli/show.rs`: `select` matches the longest namespace that is the name or a
  dot-prefix of it, then the rest as a system id. A group path gives a new
  `Selection::Targets(Vec<usize>)`, rendered as each target in turn and as a
  JSON array. `names` adds group paths.

Tests in `cli/show.rs`:
- `a.b.c.sys` finds system `sys` on namespace `a.b.c`; a dotted system id on a
  dotted namespace; a group path gives its targets.
- A namespace that is also a prefix of a system id resolves to the longest
  namespace.
- An unknown name lists suggestions, as now.

## 6. End-to-end fixture

`tests/fixtures/echo-pack/groups.py`: a `flight` group of three `string`
groups, each with a CPU target and a gateway target, plus a `ground` target
with a `recorder`. Each gateway reads its CPU through a subscriber the parent
adds after the CPU exists, and each CPU reads its gateway. Ports come from
`METOR_DEPLOY_PORTS`.

Tests in `tests/deploy.rs`:
- Nominal: samples cross both directions of each cycle; the recorder's db has
  a value from every CPU and gateway.
- Off-nominal: `--target flight.string1` runs two targets; its links to the
  ground fail to connect and it still runs its cycles.
- Failure: `--target flight --except flight` exits with `NothingSelected`.

These tests start `metor run` children; each must stop them on exit. The
existing deploy tests leave children running (seen on 2026-11-06), so fix or
at least check that before adding more.

## 7. Resolver ports

- `deploy/mod.rs`: after `pending`, a new `assign_ports` gives each listener
  that some peer dials and that listens on port 0 a port: the next one, from
  2240, that no other listener on its host uses, in `SystemIndex` order. It
  updates `Listeners` and writes the port into the listener's `tcp_listen`
  resource or `Db` `listen` param. A listener on port 0 that nobody dials
  keeps port 0.
- The first port comes from the caller of `Pending::resolve`, which reads
  `METOR_FIRST_PORT` in the CLI and defaults to 2240, so tests can move it.
- `deploy/error.rs`: `NoFreePort { host }` when a host runs out.
- `DESIGN.md` links section and `docs/target-groups.md`: describe it.
- Move `groups.py` to `Listen("[::]:0")` and `METOR_FIRST_PORT`.

Tests in `deploy/tests.rs`:
- Nominal: two dialed port-0 listeners on loopback get 2240 and 2241, and
  their dialers' addresses match.
- Off-nominal: a hand-written 2240 on the same host makes the assigned port
  2241; listeners on two hosts both get 2240; an undialed port-0 listener
  stays 0; a dialed `Db` on port 0 gets a port.
- Failure: a first port of 65535 with two listeners gives `NoFreePort`.
- `tests/deploy.rs`: the step 6 tests pass with assigned ports.

## 8. Review

An Opus agent reviews the whole branch against the doc and this plan, as an
adversarial reviewer. Fix what holds up, then open a PR stacked on
`sphw/fsw3-composite`.

## Risks

- Recorded paths may change with the recorder's subscriber ids (step 2).
- `Db(all=True)` in `recorder` records every system added before it on its
  target, not only its subscribers. On a target that also runs flight
  systems, that records more than the old gateway did.
- An alarm id, `{ns}.{system}.{name}`, holds at most 96 bytes, and group paths
  make namespaces longer. A deep tree can fail with `AlarmPrefixTooLong`.
- The panel's component tree splits names on `.`, so deeper namespaces
  should nest there. This is untested.
- Evaluating the whole tree builds every target's dev packs on each `metor
  run`.
