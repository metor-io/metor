# Links and names: implementation plan

Implements the "Links and names" section of `18-io.md`, and
`TcpListener::adopt` from its stellarator section. Chunk F of
`18-io-plan.md`. Seven tasks, in order. Each ends with:

- `cargo test -p metor-fsw-3 --no-fail-fast`;
- `cargo clippy --all-targets` clean;
- the `python/metor-config/tests` suite;
- `cargo test -p adcs-fsw3`;
- one commit.

Order: F1 maps `Transport` onto the TCP resource kinds. F2 moves the links
onto `via`. F3 moves the deploy pass onto resources. F4 adds `connect()`
for cyclic systems and the resolver list. F5 reads bound addresses from
resources. F6 adds the `harness` resolver and moves the tests onto it. F7
updates `DESIGN.md`. Harness comes after static, as the top-level plan
asks. An antagonistic review follows F7.

`examples/adcs-fsw3/target.py` and `sim.py` do not change in any task.

## Links hold no capability

Links are async systems, and async systems do not take `Cap<K>` in this
slice. So a link's `via` is the name of a resource, resolved at make:

- a `TcpListen` resource is bound at build, or taken from the descriptors
  the harness passed down, and the opened listener is handed to the link,
  which adopts it with stellarator's `TcpListener::adopt`;
- a `TcpConnect` resource is data, its address, which the link dials with
  stellarator's `TcpStream::connect` and its backoff, as today.

There is no `Endpoint` interface: no `Cap` param takes either kind. A
`Via` enum, resolved at make, says which kind the link got.

## Depends on

Names are those of `18a-io-stellarator-impl.md` and the other chunk
plans.

From A:
- `Op::Resolve`, `Op::Connect { addr: RawAddr }`, `Out::Addr(RawAddr)`,
  `Out::Handle(RawHandle)`, and `RawAddr: From<SocketAddr> + TryInto<SocketAddr>`.
- A dial target is data given to the backend's constructor; dial target
  `i` has the handle `RawHandle::new(i, 0)`. On its handle, `Resolve`
  answers the stored address and `Connect` makes and connects a socket
  (A's decision 11).
- **A1.** `stellarator::net::TcpListener::adopt(std::net::TcpListener)`
  (A step 7). fsw-3 gains no dependency for it.

From B:
- `Cap<K>`, `Handle<K>`, `Pending<O>`, the `Interface` trait, `Dial`.
- `Listen::accept(&self) -> Pending<Handle<Stream>>`. `Dial::connect` is
  F4's.

From C:
- `CoordinatorConfig.resources: Vec<ResourceConfig { id, backing }>`,
  `Backing::{Real(RealKind), Sim(SimKind), Peer(String)}`, grants, C3's
  interface check, `MakeCx.resources`, `MakeCx.io` with `IoShared`.
- `RealKind::TcpListen { addr }` and `RealKind::TcpConnect { addr }`, each
  providing one interface: `Listen` and `Dial`. The opener `io::open`
  (C4). A `TcpConnect` granted to a cyclic system is not opened: its
  address goes to the `Os` constructor.
- Python `fsw.resource(name, kind)` returning a `ResourceHandle`, in
  `metor_config/_io.py`.

E and G are not needed.

## F1. `Transport` maps onto the TCP kinds

Files: `src/link/transport.rs`, `src/io/kind.rs` (C's `RealKind`),
`src/io/open.rs` (C's opener), `python/metor-config/metor_config/_io.py`,
`python/metor-config/tests/test_io.py`.

1. C's two kinds stay, each providing one interface. `Transport` is
   deleted, and its variants map onto them:

   | `Transport` | Becomes |
   |---|---|
   | `Listen { addr, max_connections }` | `RealKind::TcpListen { addr, max_connections }` |
   | `Connect { addr }` | `RealKind::TcpConnect { addr }` |
   | `Peer` | `Backing::Peer(name)` |

   `TcpListen` gains `#[serde(default = "max_connections")]
   max_connections: usize`, default 8, so C's JSON still decodes.
2. The opener binds a `TcpListen` (refusing `max_connections == 0`),
   keeping today's error texts from `Transport::bind`. `Transport::bind`,
   the link's `Endpoint` enum, and `resolve` leave `transport.rs`;
   `Dialer`, `source`, `listen`, `dial`, `accept`, and the backoff stay.
3. Python: `TcpListen(addr, max_connections=8)` and `TcpConnect(addr)`
   keep C's JSON, with `max_connections` emitted when not 8.

Tests (moved from `transport.rs` to the opener's tests, same cases):
- nominal: `TcpListen` on `127.0.0.1:0` reports a nonzero port;
  `TcpConnect` JSON decodes;
- off-nominal: `max_connections` defaults to 8; a `TcpListen` without it
  decodes;
- failure: zero connections and an unassignable address fail with the
  resource's name and the address.

## F2. Links take `via`

Files: `src/link/{mod,publish,subscribe,db,transport}.rs`, new
`src/link/via.rs`, `src/coordinator/{build,table}.rs`,
`python/metor-config/metor_config/{_builtins,_target,_io}.py`,
`python/metor-config/tests/{test_target,test_deployment}.py`,
`tests/golden/{target,deployment,deploy}.json` and their sources,
`tests/link.rs`.

1. **`LinkParams`**:

   ```rust
   pub struct LinkParams {
       /// The resource the link listens or dials through.
       pub via: String,
       #[serde(default, skip_serializing_if = "Option::is_none")]
       pub namespace: Option<String>,
       #[serde(default)]
       pub link: String,
   }
   ```
2. **`Via`**, in `link/via.rs`:

   ```rust
   /// A link's resource, opened on the coordinator thread at make.
   pub(crate) enum Via {
       Listen { listener: std::net::TcpListener, max_connections: usize },
       Dial { addr: SocketAddr },
   }
   pub(crate) fn resolve(via: &str, resources: &[ResourceConfig], io: &mut Option<Rc<RefCell<IoShared>>>) -> Result<Via, ParamError>;
   ```

   `resolve` finds the resource by name. A `TcpListen` is opened through
   `io::open`, or taken from `IoShared::inherited` (F6). A `TcpConnect`
   resolves its address once. Any other kind, or an unresolved `Peer`, is
   `ParamError::Via { link, resource, found }`.
3. **Registration.** `link::entry` wraps `async_entry`: its make calls
   `via::resolve` on the coordinator thread, then places the system as
   `async_entry` does, handing the `Via` to the link's ctor
   (`Publish::new(params, via)`). `register_builtins` registers the three
   links through it.
4. **Systems.** At the start of `run`:
   - `Via::Listen` → `TcpListener::adopt` (A1), slots from
     `max_connections`;
   - `Via::Dial` → a `Dialer` over the address, one slot.

   The enum `source` matches on is renamed `Bound`, holding the adopted
   types. A `Db` given a `Dial` writes a `fault("db_dials", ..)` and
   returns; the Python side refuses it first.
5. **One owner per listener.** Build refuses a `TcpListen` resource used
   both as a link's `via` and as a grant: `BuildError::ViaGranted {
   resource }`.
6. **`Db`'s UDP** keeps binding its UDP socket directly on the listener's
   address. It is not a resource (open question 1).
7. **Python.**
   - `Link.__init__` takes `via: ResourceHandle | None` with the three
     shorthands. Exactly one of the four is given.
   - `Target.add` asks a link for its resource after the name is known:
     `listen=` makes `TcpListen(addr, max_connections)` named after the
     link, `connect=` makes `TcpConnect(addr)`, `peer=` makes a
     `Backing::Peer` named after the link pointing at the peer link's
     resource. `via=` of a resource on another target also makes a local
     `Peer` resource named after the link.
   - Params carry `via`, not `transport`.
   - `listen_port()` reads the link's resource. `_check_peers` and
     `_check_ports` read resources.
8. Goldens regenerate. `test_peer_transport_and_copies` checks the peer
   resource instead of `params.transport`.

Tests:
- `link/via.rs`: nominal, a `TcpListen` resolves to a bound listener and
  a `TcpConnect` to its address; failure, a `Tty` and an unresolved `Peer`
  are `Via` errors naming the link and the resource.
- `tests/link.rs`: every existing test passes with configs that list a
  resource and name it in `via`; the helpers `listen_any` and `publish`
  emit `resources`;
- `test_target.py`, nominal: `Publish(listen=..)` emits a resource named
  after the link and `via` in params; `via=` with a handle emits no extra
  resource;
- off-nominal: `via=` to another target's resource emits a local peer
  resource named after the link;
- failure: two of `via`/`listen`/`connect`/`peer` raise; `Db(via=dial)`
  raises; a `via` to a resource of another kind (a `Tty`) raises at
  emission and, if hand-written, fails at build; a listener used as `via`
  and granted to a system fails `ViaGranted`;
- `examples/adcs-fsw3`: `target.py` emits and runs unchanged
  (`tests/deploy.rs` there still passes).

## F3. Deploy rewrites resources

Files: `src/deploy/{mod,error,tests}.rs`, `src/deploy/index.rs`.

1. `resolve` walks `coordinator.resources`. Each `Backing::Peer(peer)`
   becomes `Backing::Real(RealKind::TcpConnect { addr })`, from `hosts`
   and the peer resource's fixed port. Resources are typed in
   `ResourceConfig`, so the pass edits them directly; `Part<T>` stays for
   `command_ids` on `DbParams`.
2. `peer_link` and `listen_port` take a `&ResourceConfig`. `fixed_port`
   reads a resource. `check_ports` walks resources, naming
   `{namespace}.{resource}`.
3. **Db commands.** A peer resource whose owner target's link with
   `via == resource` is `DB_TYPE` collects the dialer's message ids, as
   now. `DefIndex::locate_resource(from, target, resource)` replaces
   `locate` for peers. `DeployError::NoSuchLink` becomes `NoSuchResource`;
   `NotAListener` names the resource.
4. `unresolved` is "a resource is `Peer`, or a slot is unresolved".

Tests, in `deploy/tests.rs`:
- nominal: a peer resource resolves to `TcpConnect` on the peer's host
  and port; the fixpoint test still holds; a `Db` learns its subscribers'
  command ids through the resource;
- off-nominal: a peer resource granted to a cyclic system's `Cap<Dial>`
  is resolved too (a resource is a resource); a `hosts` IPv6 host is
  bracketed;
- failure: a peer naming a missing target, a missing resource, a dialing
  resource, and port 0 each fail with their error; two resources binding
  one host and port fail with `PortTaken`.

## F4. `connect()` for cyclic systems, and the resolver list

Files: `src/io/cap.rs` (B's `Dial::connect`), `src/io/pending.rs`,
`src/deploy/config.rs` (`resolve` list),
`python/metor-config/metor_config/_deployment.py`, `tests/io.rs`.

1. **`Dial::connect(&self) -> Pending<Connect>`** submits `Resolve` and,
   when it completes with `Out::Addr`, `Connect { addr }`, one cycle
   later. A failed `Resolve` is the connect's result. The `Os` makes the
   socket for each `Connect`. `Connect`'s output is `Result<Handle<Stream>,
   IoError>`. Every reconnect resolves again.
2. **Resolvers.** `DeploymentConfig.resolve: Vec<Resolver>`, default
   `[Static]`; `Resolver::{Static, Harness}`. Python
   `Deployment(resolve=["static"])`. `metor build` and `metor run` apply
   them in order:
   - `static` is F3's pass;
   - `harness` is F6.

Tests:
- unit, in `io/system.rs` over C5's test `Os`: nominal, `connect` submits
  `Resolve` then `Connect`, in two cycles, and completes with a `Stream`
  handle; failure, a `Resolve` failure ends the connect without a
  `Connect`; a `Connect` failure (`ECONNREFUSED`) is the connect's result;
- integration, `tests/io.rs`: a cyclic system with a `Cap<Dial>` on a
  `TcpConnect` to a std listener connects and sends bytes the listener
  reads;
- `deploy/config.rs`: a deployment without `resolve` decodes as
  `[Static]`; an unknown resolver name fails to decode.

## F5. Bound addresses come from resources

Files: `src/link/mod.rs` (`BOUND`, `record_bound`, `bound_ports`),
`src/io/open.rs`, `src/cli/run.rs` (`print_ports`), `tests/link.rs`,
`tests/run.rs`.

1. The opener records each bound `TcpListen` resource as `(id,
   local_addr)` in the process registry. `link::record_bound` is deleted.
   `bound_ports()` moves to `io::bound_ports()` and keeps its type.
2. `--print-ports` prints `{resource} {addr}`, in resource order. For
   shorthand links the resource name is the link name, so the existing
   tests read the same lines.

Tests:
- `tests/run.rs` `test_target_exits_after_fixed_cycles` still lists
  `cmds` and `pub`;
- a `TcpListen` resource no system uses is not opened and not printed;
- a failed bind records nothing.

## F6. The `harness` resolver

Files: new `src/cli/harness.rs`, new `src/cli/listen_fds.rs`,
`src/cli/{run,children,mod}.rs`, `src/io/{open,shared}.rs` (take an
inherited fd), `src/coordinator/build.rs` (`BuildOptions.inherited`),
`src/deploy/config.rs`, `tests/{deploy,sequences,run,link}.rs`,
`tests/common/mod.rs`, `tests/fixtures/echo-pack/{deploy,slot}.py`,
`examples/adcs-fsw3/tests/deploy.rs`, `examples/adcs-fsw3/README.md`.

1. **Selecting it.** `metor run --resolve harness` overrides the
   deployment's list for this run. A deployment may also list `harness`
   first. Tests use the switch, since `target.py` cannot change.
2. **Binding** (`harness.rs`), before any child starts:
   - every `TcpListen` resource in every target is bound on
     `127.0.0.1:0` with `std::net::TcpListener`, whatever its configured
     address;
   - each resource's `addr` is rewritten to the bound address;
   - each `Peer` resource naming it becomes `TcpConnect` to that address;
   - `static` then resolves what is left.
   `Harness { held: Vec<(Name, OwnedFd)> }` keeps the listeners. A bind
   failure stops the run before any spawn.
3. **Passing them down.** For each child, `children::spawn` maps that
   target's fds to 3.. in `pre_exec` (`children.rs:56-60`), in two
   phases, so no `dup2` clobbers a held fd that is still to be moved:
   first each held fd is `dup2`ed to `n + 3 + i`, above every target
   slot, then each of those to `3 + i`. The second `dup2` clears
   `FD_CLOEXEC`. Both are async-signal-safe. It sets `LISTEN_FDS=n` and
   `LISTEN_FDNAMES=a:b:..` (resource ids). `LISTEN_PID` is not set (open
   question 2). A one-target run hands the held listeners to its own build
   in process.
4. **Taking them** (`listen_fds.rs`, `open.rs`): at startup `metor run`
   reads and unsets the three variables, takes fds 3.. as `OwnedFd`
   named from `LISTEN_FDNAMES`, and passes them to build through
   `BuildOptions.inherited: Vec<(String, OwnedFd)>`. `BuildOptions` then
   drops `Clone` and `Copy` (E derived them); it is passed by value.
   Build puts them in `IoShared::inherited`
   before the first make. The opener takes the inherited fd for a
   `TcpListen` resource of that name instead of binding, for a link's
   `via` and for a grant alike. A name with no resource is closed with a
   warning; a count mismatch fails the run.
5. **Readiness.** The parent binds a `UnixDatagram` in a temp dir and sets
   `NOTIFY_SOCKET`. A child sends `READY=1` once its coordinator is built,
   before its first cycle. The parent waits for every child's `READY=1`
   (or its exit), then prints ports and forwards `READY=1` to its own
   `NOTIFY_SOCKET` if it has one.
6. **`--print-ports`** under `harness` prints `{namespace}.{resource}
   {addr}` for every target from the parent, after readiness.
   `PrintPortsNeedsTarget` remains for `static`.
7. **Tests move onto it.**
   - `tests/deploy.rs`, `tests/sequences.rs`, and
     `examples/adcs-fsw3/tests/deploy.rs` run with `--resolve harness
     --print-ports`, read the db's or the links' addresses from stdout,
     and drop `free_ports`, `ports_env`, and `PORTS_ENV`.
   - `tests/common/mod.rs` gains `harness_ports(child) -> Vec<(String,
     SocketAddr)>` and loses `free_ports` and `ports_env`.
   - `tests/fixtures/echo-pack/deploy.py` and `slot.py` drop their port
     variables and use fixed ports.
   - `examples/adcs-fsw3/target.py` still reads `METOR_DEPLOY_PORTS`; no
     test sets it, and the harness ignores the ports it picks. The README
     says to use `--resolve harness` instead.

Tests:
- unit (`harness.rs`): nominal, a three-target envelope gets every listen
  resource bound and every peer rewritten to its bound address;
  off-nominal, a target with no listeners gets no fds; failure, an
  envelope with a peer to a dialing resource fails as `static` would;
- unit (`listen_fds.rs`): parsing `LISTEN_FDS`/`LISTEN_FDNAMES`; a
  count mismatch is an error; absent variables give none;
- unit (`children.rs`): the fd plan for held fds `[5, 3, 4]` moves each
  to its slot without loss (checked on the plan, without forking); a
  spawned child given two listeners whose fds are already 4 and 3 sees
  both, named right;
- integration: the deploy, sequences, and ADCS deploy tests pass under
  `harness` with no fixed ports; two of them run concurrently (cargo's
  default) without collision;
- integration (`tests/run.rs`): a child that fails to build never sends
  `READY=1`, and the parent reports it and exits non-zero; SIGINT before
  readiness still ends every child.

## F7. `DESIGN.md`

Files: `DESIGN.md`, `TODO.md`.

The "Links and gateways" and "Configuration" sections say links take a
resource through `via=`, list the shorthands, name the two resolvers,
and describe what `metor build` and `metor run --resolve harness` do.
Same length or shorter.

Check: every type the section names exists (`grep` each).

## Exit criteria

- `examples/adcs-fsw3/target.py` and `sim.py` are unchanged; `target.py`
  runs under `metor run` with `static` and with `harness`.
- The deploy tests pass under `harness` without fixed ports.
- No test sets `METOR_DEPLOY_PORTS` or `METOR_SLOT_PORTS`, and
  `free_ports` is gone.

## Versions

- `CONFIG_VERSION` 2 to 3, Rust and Python: link params replace
  `transport` with `via`, and the envelope gains `resolve`. A version-2
  file fails with the version error, not a decode error. The version list
  in `test_version_error_reports_versions` becomes `[0, 2, 4]`; inline
  `"config_version":2` literals in `deploy/config.rs` tests,
  `deploy/tests.rs`, `tests/slot.rs`, and the Python tests become 3.
- ABI unchanged: the links are builtins.
- metor-config: minor bump.

## Not done here

- The `sim` resolver and links over simulated resources (`SimListen`,
  `SimDial`). They need async systems on the reactor (`TODO.md` §8).
- Async systems taking `Cap<K>`, and links holding capabilities.
- `mdns` and gossip resolvers.
- Real kinds over Unix domain sockets.
- `Db`'s UDP as a resource.
- `LISTEN_PID` checking.

## Open questions

1. **`Db` binds UDP directly,** outside the resource model. It needs a
   UDP resource beside its TCP one, which changes the gateway's Python.
2. **`LISTEN_PID`.** `std::process::Command` cannot set it to the child's
   pid without `setenv` after fork, which is not async-signal-safe.
   Children treat an absent `LISTEN_PID` as theirs and unset the
   variables. The other path is `posix_spawn` by hand.
3. **"Replacing `METOR_DEPLOY_PORTS`"** cannot remove the variable from
   `target.py`, since that file must not change. Tests stop setting it;
   the file's fallback ports are then what `static` uses.
