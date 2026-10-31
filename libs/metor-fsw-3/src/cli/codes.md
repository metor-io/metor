## M0101 `--epoch` needs a simulated clock

`--epoch` sets the time a simulated clock starts at. The target runs on a wall clock, either because the target file gives it no `sim_dt` or because the command line passes `--wall`, so there is no simulated clock to start.

Pass `--sim-dt` with `--epoch`, or give the target a simulated clock in the target file.

```
metor run --sim-dt 10ms --epoch 2026-10-29T00:00Z
```

```python
fsw = Target(cycle_rate=100, sim_dt=0.01, epoch=0)
```

## M0102 No target has that name

`--target` names a target the deployment does not have. Targets are named by their namespace, the `namespace` argument of `Target`.

The help line suggests the closest name and lists the targets. `metor show` lists them too. Check the spelling, and check that the target file gives the target a namespace.

```
metor run --target fsw
```

## M0103 The deployment has several targets

The command needed one target, and the deployment has more than one. The message lists their namespaces.

Pick one with `--target`.

```
metor run --target fsw
```

## M0104 Nothing in the deployment has that name

`metor show` or `metor graph` was given a name that is neither a target nor a system. A target is named by its namespace, such as `fsw`. A system is named `target.system`, such as `fsw.nav`. In a deployment of one target, the system's id alone also works.

The help line suggests the closest name. Run `metor show` with no name to list every target and system.

## M0105 Required checks failed

`metor doctor` found a tool the project needs missing or out of date. The message names each failed check: `python`, `metor-config`, `cargo`, or `wasm32`. A missing `uv` is only a warning.

Each failed check prints its fix under it. The usual ones:

- `python`: install Python 3, activate the project's virtualenv, or set `$METOR_PYTHON`.
- `metor-config`: `uv add metor-config`, or set `$METOR_CONFIG_PY`. If it imports but its ABI differs, install the metor-config that ships with this metor.
- `cargo`: install Rust from rustup.rs.
- `wasm32`: `rustup target add wasm32-unknown-unknown`.

## M0106 No such code

`metor explain` was given a code it does not know. Codes are an `M` and four digits, such as `M0201`, and appear in brackets after `error` in a diagnostic.

Check the code against the diagnostic it came from. A code from a different version of metor may not exist in this one.

## M0107 That name cannot name a project

`metor new` names the pack, its Python module, and its crate after the last part of the path it is given, so that part must work as a Python and a Rust identifier once `-` becomes `_`. It must start with a lowercase letter, hold only lowercase letters, digits, `-`, and `_`, and not be a word either language reserves.

```
metor new target my-sat
```

## M0108 The directory already exists

`metor new` writes into a new directory and does not touch an existing one, so nothing is overwritten. Pick a path that does not exist yet, or remove the old directory first.

## M0201 The target file failed

The target file exited with a non-zero status. Usually it raised an exception, and the traceback is printed above the diagnostic.

A `metor_config.ConfigError` means the Python config API refused something in the file: a system added twice, a loop never connected, a channel with no command source, and so on. Its message says what. Any other exception is an error in the file itself, or in a pack module it imports.

Fix the line the traceback points at. Running the file with Python directly shows the same traceback.

## M0202 PYTHONPATH could not be built

metor runs the target file with a `PYTHONPATH` made of `$METOR_CONFIG_PY` or the in-repo metor-config, each dev pack's `.metor` directory, and the inherited `PYTHONPATH`. One of these paths contains the path separator, `:`, so they cannot be joined.

Move the project or the pack, or point `$METOR_CONFIG_PY` at a path without a `:`.

## M0203 A program could not be started

metor could not start a process. Most often this is the Python interpreter that runs the target file. metor uses `$METOR_PYTHON`, then `$VIRTUAL_ENV/bin/python`, then `python3` from `PATH`. The same code covers starting the child processes of a multi-target run and of `metor watch`.

Set `$METOR_PYTHON` to a Python 3 interpreter, or activate the project's virtualenv. `metor doctor` shows which interpreter it finds.

```
export METOR_PYTHON=.venv/bin/python
```

## M0204 The config version does not match

The deployment config was written for a different config version than this metor reads. The target file's metor-config is older or newer than the `metor` binary, or a `.json` file was written by `metor build` from another version.

Install the metor-config that ships with this metor. For a `.json` deployment, write it again with this version's `metor build`. `metor doctor` checks the metor-config the target file imports.

## M0205 The config did not decode

The deployment config is valid JSON with the right version, but its contents do not match what metor reads. A field is missing, has the wrong type, or is unknown.

The usual causes are a hand-edited `.json` deployment, or a metor-config whose version number matches but whose code does not, such as a local checkout on another branch. Install the metor-config that ships with this metor, and write `.json` deployments with `metor build` rather than by hand.

## M0206 The target file does not exist

The path given on the command line is not a file.

Check the path. Without a path, metor uses the nearest `target.py` in the working directory or a directory above it.

## M0207 The target file made no deployment

The target file ran and exited cleanly, but wrote no config. A target file writes its config when it exits, as long as it created a `Target` or a `Deployment`.

Usual causes:

- The file creates no `Target`, for example because the code that does is behind a condition that was false.
- The file calls `emit(path)` with a path, so the config went to that file instead of to metor.
- The file exits with `os._exit`, which skips the exit hook that writes the config.

```python
fsw = Target(cycle_rate=100)
```

## M0208 A file could not be read or written

A file operation failed. The message names the path and the operating system's error. The usual paths are a `.json` deployment that `metor run` reads, the file `metor build -o` writes, the working directory, or a temporary file.

Check that the path exists and that you have permission to read or write it.

## M0209 No project found

No path was given, and there is no `target.py` in the working directory or any directory above it. The search stops at the root of a git repository.

Run the command from the project directory, or pass the target file.

```
metor run path/to/target.py
```

## M0210 A peer reference names an unknown target

A system refers to a target, by namespace, that the deployment has no member of. References like this come from a `Peer` transport or a subscriber's copied ports.

The Python API takes a peer as a link's handle, so a target file does not usually produce this. Check that every target the file creates is in its `Deployment`, and that a hand-edited `.json` deployment spells the namespace correctly.

## M0211 A peer reference names an unknown link

A system refers to a link, `target.link`, and that target has no system with that id. As with M0210, the Python API takes a link's handle, so this usually comes from a hand-edited `.json` deployment.

Check the link's id with `metor show`.

## M0212 A peer link does not listen on a fixed port

A link dials a peer link, and the peer cannot be dialed: it does not listen, or it listens on port `0`, which the operating system picks at run time. A dialer needs a fixed port to connect to.

Give the peer link a `Listen` transport with a non-zero port.

```python
link = plant.add("pub", Publish(all=True, transport=Listen("0.0.0.0:2240")))
fsw.add("plant", Subscribe(transport=Peer(link)))
```

## M0213 A peer link has no such port

A subscriber copies a port from a peer link, and the peer link does not publish a port with that name. This usually means a `Subscribe`'s `items` names a port of the peer target that the peer's `Publish` does not list.

Add the port to the peer's `Publish`, or remove it from the subscriber's `items`. `metor show` lists each link's ports.

```python
plant.add("pub", Publish([sensors.imu], transport=Listen("0.0.0.0:2240")))
```

## M0214 Two listeners bind the same port

Two systems listen on the same port of the same host. A link with a `Listen` transport, a `Db`'s `listen`, and a `TcpListen` resource all count. Targets with no entry in the deployment's `hosts` all run on loopback, so their ports must differ too.

Give each listener its own port, or put the targets on different hosts.

```python
Deployment([plant, fsw], hosts={"plant": "10.0.0.2", "fsw": "10.0.0.3"})
```

## M0215 Resolving peer ports loops

To find the type of a copied port, `metor build` follows it back to the system that produces it. The message shows a path that leads back to where it started: a link copies a port from a peer that copies it, directly or through other links, from the first link.

Publish a port from the target that produces it rather than from a copy of it, so that the copies do not form a loop.

## M0216 A copied port was left unresolved

A system's output still names a peer's port after resolution, which should have replaced it with the port's definition. metor's own resolution handles every reference it writes, so this points at a hand-edited `.json` deployment or a bug.

Write the deployment again with `metor build`. If it persists, report it with the target file.

## M0217 A peer dial has no holder

A resource dials a peer link, but no system is granted the resource, so `metor build` does not resolve its address. The Python API always grants a link's dialer to the link, so this points at a hand-edited `.json` deployment.

Grant the resource to the link that uses it, or remove it.

## M0218 A slot could not be resolved

A `DlSlot` could not be prepared. The message says why. Usual causes:

- The slot's params do not decode, in a hand-edited `.json` deployment.
- A candidate's pack did not open or is not built. Run `metor pack dev` in the pack.
- A candidate's definition failed, for example because its params are wrong for its type.
- The candidate's library could not be copied into `.metor/store` next to the target file.

## M0219 A slot's candidates have different ports

Every system a `DlSlot` allows must have the same ports as the first one, since the slot is wired once and any candidate can be loaded into it. The message names the first candidate and one that differs.

Remove the candidate, or change its type so its inputs and outputs match the first.

## M0220 A slot's library name is too long

A channel's status carries the file name of the library a slot loaded, in at most 64 bytes. The file name of a candidate's pack library is longer.

The file name comes from the pack's library name. Shorten `[lib] name` in the pack's `Cargo.toml`, or set `lib` under `[tool.metor.pack]` in its `pyproject.toml`, then run `metor pack dev`.

## M0301 A deterministic run needs a simulated clock

`--deterministic` runs every thread group on the coordinator thread, keyed on cycle times. That needs times that are fixed in advance, which a wall clock does not give.

Run on a simulated clock with an epoch, from the command line or the target file.

```
metor run --deterministic --sim-dt 10ms --epoch 0
```

```python
fsw = Target(cycle_rate=100, sim_dt=0.01, epoch=0)
```

## M0302 A deterministic run needs a step of at least 1 µs

The simulated clock's step is under one microsecond. A deterministic run counts time in whole microseconds, so its cycle times would not increase.

Set `--sim-dt`, or `sim_dt` on the `Target`, to at least 1 µs.

```
metor run --deterministic --sim-dt 10ms --epoch 0
```

## M0303 A deterministic run needs an epoch

A deterministic run's clock counts from zero, so the simulated clock needs a fixed start of 0 or later. The clock has no epoch, or a negative one.

Pass `--epoch`, or set `epoch` on the `Target`. A bare number is microseconds since the Unix epoch.

```
metor run --deterministic --sim-dt 10ms --epoch 2026-10-29T00:00Z
```

## M0304 Two systems have the same id

Two systems in one target share an id. `Target.add` refuses a name twice, so in a target file this usually means a system is named `io_submit` or `io_complete`, the ids of the io steps a target adds when one of its systems holds a `Cap`. In a hand-edited `.json` deployment, it can be any id.

Give each system its own name.

## M0305 A system declares an input twice

A system type defines two input ports with the same name. Port names come from the pack, so this is a bug in the pack's system definition.

Rename one of the inputs in the pack, then run `metor pack dev`.

## M0306 A system declares an output twice

A system has two output ports with the same name. Either the pack defines the name twice, or an output the config adds, such as a record port of a `Subscribe`, takes a name the type already uses.

Rename the port in the pack, or remove the duplicate from the target file.

## M0307 A system's type is not registered

A system names a type that no pack the target lists provides. Types are named `pack.system`. Usual causes:

- The pack is not built for this platform, or its module is stale. Run `metor pack dev` in the pack.
- A hand-edited `.json` deployment misspells the type or leaves out the pack.
- The type is a built-in from a different version of metor.

`metor show` lists each system's type.

## M0308 An input reads from an unknown system

An input names a producer that is not a system of the same target. A system reads only ports of its own target. The Python API refuses a port from another target, so this usually comes from a hand-edited `.json` deployment.

To read another target's port, subscribe to its link and pass the subscriber's copy.

```python
plant_sub = fsw.add("plant", Subscribe(transport=Peer(link)))
nav = fsw.add("nav", Nav(imu=plant_sub.port(sensors.imu)))
```

## M0309 A system has no such input

The config wires an input port the system's type does not define. The pack's generated module only offers the inputs the type has, so this usually means the module is older than the pack, or the `.json` deployment was edited by hand.

Run `metor pack dev` in the pack to regenerate its module, then check the input names with `metor show target.system`.

## M0310 A system has no such output

The config names an output port the system does not have, either as the source of an input or as an output entry of the system itself. A loop connected to a port of the wrong system, a stale pack module, or a hand-edited `.json` deployment are the usual causes.

Check the producer's outputs with `metor show target.system`, and run `metor pack dev` if the pack changed.

## M0311 An input and its producer carry different records

An input reads from an output that carries a different record than the input expects. Each port carries one record type, and the two must match.

Wire the input to a port that carries its record. `metor show target.system` lists each port with its record. A pack module's type hints catch most of these in an editor before the run.

## M0312 A port needs more alignment than a ring gives

A port's record needs an alignment above 16 bytes, which is the most a ring payload is aligned to.

Change the record in the pack so its alignment is 16 bytes or less, then run `metor pack dev`.

## M0313 An output's ring is too large

The ring behind an output would be larger than this host can address. The record's maximum length is too large.

Reduce the record's maximum size in the pack, then run `metor pack dev`.

## M0314 An undeclared input reads a later system

Systems run in the order they are added. Some inputs are not declared by the system's type and take their record from the producer, such as the ports a `Publish` or `Db` lists. Such an input needs its producer to be planned first, and this one reads a system added after it.

Add the producer before the system that reads it. A loop does not help here, since the reader still needs the producer's record when it is planned.

```python
nav = fsw.add("nav", Nav(imu=imu.data))
fsw.add("pub", Publish([nav], transport=Listen("0.0.0.0:2240")))
```

## M0315 A record is defined two ways

Two registered ports use the same record name with different definitions. Typically two packs, or a pack and metor's built-in types, were built against different versions of the crate that defines the record.

Build every pack against the same version of the record's crate and of metor-fsw-3, then run `metor pack dev` in each.

## M0316 Two records share a record id

Two different records map to the same record id, so their data cannot be told apart. The message names both.

Rename one of the records in its pack, then run `metor pack dev`.

## M0317 Two messages share a message id

Two message records use the same message id with different schemas. A message id names one schema on the wire.

Give one of the messages a different id in its pack, then run `metor pack dev`.

## M0318 A system cannot run that way

A system's type does not support how the target file runs it:

- A cyclic system or a sequence cannot run on a thread, so it takes no `thread=`.
- An async system runs on a thread, and cannot be behind a channel.
- A `DlSlot` must be added with `channel=`.

Change the keyword on `Target.add` to match the system's kind.

```python
fsw.add("db", Db(listen="0.0.0.0:2241", all=True), thread="db")
fsw.add("slot", DlSlot(allow=[Detumble(), Point()]), channel="adcs")
```

## M0319 A type declares a `status` output

The coordinator adds an output named `status` to every system, carrying its `SystemStatus`. The type declares its own output with that name.

Rename the output in the pack, then run `metor pack dev`.

## M0320 A type's definition is invalid

A system type was refused when its pack registered it. The message gives the reason. Common ones:

- An async system takes a `Sample`, a `Cap`, or an `Rng`. Only a stepped system takes these.
- The type sets an op budget but takes no `Cap`.
- The type sets an op budget of 0.

Fix the system in the pack, then run `metor pack dev`.

## M0321 Two systems use the same channel

Commands find a channel by name, so each channel name in a target must be unique. Two systems were added with the same `channel=`.

Give each channel its own name.

```python
fsw.add("detumble", Detumble(), channel="detumble")
fsw.add("point", Point(), channel="point")
```

## M0322 A port name is reserved for the channel

A system behind a channel gets a `commands` input and `channel_status` and `channel_message` outputs, and its `abort` input is written by the channel alone. The system's type already uses one of those names, or the config wires its `abort` input.

Rename the port in the pack, or remove the wiring to `abort`.

## M0323 A `Cap` parameter has no grant

The system's type takes a `Cap`, a handle to a resource such as a serial port, and the target file grants it none. Each `Cap` parameter needs exactly one resource.

Create a resource on the target and pass its handle as the parameter's keyword.

```python
uart = fsw.resource("gps_uart", Tty("/dev/ttyS1", baud=115200))
fsw.add("gps", Gps(uart=uart))
```

## M0324 A `Cap` parameter is bound twice

One `Cap` parameter of a system is bound to two resources. A target file grants each parameter once, so this comes from a hand-edited `.json` deployment, typically where a sim resource names the system as a holder of a parameter that also has a grant.

Bind each parameter to one resource.

## M0325 A grant names a parameter that is not a `Cap`

The config grants a resource to a parameter the system's type does not take as a `Cap`. The parameter is misspelled, the pack's module is stale, or the parameter is a plain param.

Check the type's `Cap` parameters, and run `metor pack dev` if the pack changed.

## M0326 A system holds a `Cap` and there is no io step

Systems that hold a `Cap` submit their IO through `fsw.io` steps. `Target` adds these, `io_submit` and `io_complete`, whenever a system it adds takes a `Cap`, so a target file does not produce this. A hand-edited `.json` deployment, or a pack module older than the pack, are the usual causes.

Run `metor pack dev` in the pack, or add an `fsw.io` system to a hand-written config.

## M0327 No io step follows a system that holds a `Cap`

A system that holds a `Cap` must have an `fsw.io` step after it, which submits its requests the same cycle. `Target` places `io_submit` after every flight system, so this comes from a hand-edited `.json` deployment or a stale pack module.

Add `fsw.io` after the named system.

## M0328 Too many IO operations in flight

The systems that hold a `Cap` ask, between them, for more operations in flight than an operation id can name, which is 65,536.

Lower the op budgets of the systems in their packs, or hold fewer resources in one target.

## M0329 Too many resources

The target creates more resources than a resource index can name, which is 65,536.

Create fewer resources, or split the target.

## M0330 The IO backend did not start

The host's IO backend could not be created. On Linux this is io_uring; elsewhere it is a poll-based backend. The message gives the operating system's error.

On Linux, io_uring can be disabled by the `kernel.io_uring_disabled` sysctl, by a container's seccomp profile, or limited by the locked-memory limit. Allow io_uring for the process, or run on a host where it is enabled.

## M0331 A grant names an unknown resource

A system is granted a resource that the target does not create. `Target` only accepts handles of its own resources, so this comes from a hand-edited `.json` deployment.

Create the resource in the target's `resources`, or fix the grant's name.

## M0332 A resource has the wrong interface

A `Cap` parameter takes one interface, such as `uart`, and the resource granted to it provides another. `Tty` provides `uart`, `Can` `can`, `GpioLine` `gpio_line`, `Udp` `udp`, `File` `file`, `Dir` `dir`, and `TcpListen` and `TcpDial` `endpoint`. `SimUart` and `SimGpioLine` provide `uart` and `gpio_line`.

Grant the parameter a resource of the kind it takes.

## M0333 A sim resource is granted directly

A sim resource names its holders itself: the system and parameter at its host end and at its device end. It cannot also be granted through a system's grants. `Target` writes sim resources this way, so this comes from a hand-edited `.json` deployment.

Remove the grant, and name the holder in the resource's `host` or `device` entry.

## M0334 A sim resource names an unknown holder

A sim resource names a system as the holder of one of its ends, and the target has no system with that id. `Target` derives holders from the systems it adds, so this comes from a hand-edited `.json` deployment.

Fix the holder's id, or add the system.

## M0335 A resource could not be prepared

A resource's backing could not be turned into something the IO backend opens. The message says why:

- A path or interface name contains a NUL byte.
- A `TcpDial` address, or a `Connect` transport, names a host that does not resolve.

Fix the path or address. A dialed host name resolves on the target when the build runs, so it must be known there.

## M0336 A system's definition failed

A system's type refused its config when its ports were computed. The message names the system and the reason. Common ones:

- An input that takes one producer was given several.
- An output names a record no registered port declares.
- The params do not match the type's schema.
- A pack's definition function refused the config, with its own message.

Fix the system's arguments in the target file. `metor show target.system` lists its ports and params.

## M0337 A system could not be made

A system's constructor failed while the target was built. The message names the system and the reason. Common ones:

- A param has an unknown key or a value of the wrong type.
- A link or `Db` could not bind its address, often because another process holds the port.
- A `Db` could not open its directory.
- A link's `conn_cap` is too small for its announce or largest record. Raise `conn_cap`.
- A wasm pack's guest memory cannot hold a port's copy ring. Raise `memory` in `Target.wasm`.
- The constructor panicked.

```python
fsw.wasm(seqs, memory=16 << 20)
```

## M0401 A pack file could not be read

`metor pack dev`, or a run that rebuilds dev packs, could not read one of the pack's files, usually its `pyproject.toml` or `Cargo.toml`. The message names the path.

Check that the directory is a pack: it holds a `pyproject.toml` with a `[tool.metor.pack]` table and a `Cargo.toml`. `metor pack dev` takes the pack's directory, and defaults to the working directory.

## M0402 A pack file could not be written

Building a pack's module writes to `.metor/<module>/` in the pack's directory: the library, the wasm module if any, `__init__.py`, and `py.typed`. One of these writes failed. The message names the path and the operating system's error.

Check permissions and free space in the pack's directory.

## M0403 A pack's TOML is invalid

The pack's `pyproject.toml` or `Cargo.toml` is not valid TOML, or lacks a field metor reads. `pyproject.toml` needs `[project] name`. `Cargo.toml` needs `[package] name`, unless `crate` and `lib` are both set under `[tool.metor.pack]`.

Fix the file at the line the message gives.

```toml
[project]
name = "adcs-pack"

[tool.metor.pack]
```

## M0404 A system's params schema did not decode

The pack's descriptor gives each system's params as a JSON schema, and one did not parse. The schema is generated from the params type in the pack, so this points at a bug in the pack or in metor.

Rebuild the pack against the metor-fsw-3 this metor ships with. If it persists, report it with the params type.

## M0405 A name is used by two keywords

A pack module gives each system one keyword per input, param, and `Cap`, so these names must not collide. The system has an input and a param with the same name, or a `Cap` whose name is an input, a param, or one of the reserved keywords `items`, `records`, and `grants`.

Rename the param, the port, or the `Cap` in the pack.

## M0406 The module could not be rendered

Rendering a pack's Python module from its descriptor failed. This is a bug in metor's module template, or a descriptor it does not expect.

Report it with the pack's descriptor. Rebuilding against the metor-fsw-3 this metor ships with can avoid a descriptor mismatch.

## M0407 The pack is already loaded

The process has already loaded this pack's library, and a loaded library cannot be replaced. Rebuilding it in the same process would leave the old code running.

Restart the command. Each run of `metor run`, `metor check`, and `metor pack dev` is a fresh process, and `metor watch` starts a new one for every rerun.

## M0408 The wasm module and the library differ

A pack with `wasm = true` is built twice: as a native library and as a wasm module. The two describe different systems or ports.

Code that is compiled only for one target, such as behind `#[cfg(target_arch = "wasm32")]`, is the usual cause. Make the pack's systems the same on both targets.

## M0410 cargo could not be started

metor runs cargo to build packs, and cargo could not be started. It uses `$CARGO`, or `cargo` from `PATH`.

Install Rust from rustup.rs, or put cargo on `PATH`. `metor doctor` checks it.

## M0411 The pack did not compile

cargo failed to build the pack. The compiler's errors are printed above the diagnostic.

Fix the errors and run the command again. `cargo build` in the pack's directory shows the same errors.

## M0412 cargo built no library

cargo succeeded but produced no `cdylib`, or for a wasm build no module, for the pack's crate. metor loads a pack as a dynamic library, so the crate must build one.

Set the crate type in the pack's `Cargo.toml`.

```toml
[lib]
crate-type = ["cdylib"]
```

## M0413 The wasm32 target is not installed

The pack sets `wasm = true` under `[tool.metor.pack]`, so it is also built as a wasm module, and the toolchain cargo uses has no `wasm32-unknown-unknown` standard library.

Install it for that toolchain.

```
rustup target add wasm32-unknown-unknown
```

## M0420 The pack's library did not load

The operating system could not load a pack's dynamic library. The message gives its error. Usual causes:

- The library is missing, because the pack was not built for this platform. Run `metor pack dev` in the pack.
- The library was built for another architecture.
- The library depends on a shared library that is not installed.

## M0421 The library is not a pack

The library loaded but does not export a function every pack exports. The message names it. The library is not a metor pack, or its crate does not call `export_pack!`.

Export the pack's systems with `export_pack!` in the crate's root, then run `metor pack dev`.

## M0422 The pack was built for another ABI

A pack and the host must agree on the pack ABI version. The pack was built against a different version of metor-fsw-3 than this metor.

Rebuild the pack against the metor-fsw-3 this metor ships with, then run `metor pack dev`. `metor doctor` shows this metor's ABI.

## M0423 The pack's descriptor did not decode

The pack describes its systems in a descriptor, and the host could not decode it. A pack built against another version of metor-fsw-3 with the same ABI number, or a library that is not a complete pack, are the usual causes.

Rebuild the pack against the metor-fsw-3 this metor ships with.

## M0424 The pack's descriptor is too large

A pack's descriptor must fit in a 1 MiB buffer, and this one does not. Descriptors grow with the number of systems and the size of their params schemas.

Split the pack into smaller packs.

## M0425 The pack's descriptor export failed

The pack's descriptor function returned a status the host does not expect, rather than a descriptor. The pack failed while describing itself, or was built against an ABI that uses other status codes.

Rebuild the pack against the metor-fsw-3 this metor ships with. If it persists, the pack's table builder is failing.

## M0426 The wasm module could not be read

A wasm pack's module file could not be read. It lives in the pack's `.metor/<module>/_libs/wasm32-unknown-unknown/` directory.

Run `metor pack dev` in the pack to build it.

## M0427 The wasm module did not load

The host could not instantiate a wasm pack's module. The message says why. Common causes:

- The module does not import its memory as `env.memory`. metor builds it with `--import-memory`; a module built another way lacks it.
- The module needs more memory than the limit set by `memory` in `Target.wasm`.
- The module lacks an export, or an export has the wrong signature, usually from an ABI mismatch.

Rebuild with `metor pack dev`, and raise `memory` if the module needs it.

## M0428 The wasm module trapped

A wasm pack's module trapped while the host loaded it or asked for its descriptor. The message gives the trap. A guest panic traps as `unreachable`.

Fix the panic in the pack, then run `metor pack dev`.

## M0501 Systems latched off

One or more systems panicked during the run. A system that panics is latched off: the coordinator stops stepping it for the rest of the run. The message names each one, and their last log lines say why.

Fix the cause in the system's pack. metor exits with status 3.

## M0502 Targets did not exit cleanly

In a multi-target run, each target runs as its own child process. One or more of them failed. Each target's own error is printed above, prefixed with its name.

Fix those errors. To run one target alone, pass `--target`. metor exits with status 4.

```
metor run --target fsw
```

## M0503 Interrupted

The run was stopped with SIGINT, usually Ctrl-C. The summary already says so, so no diagnostic is printed. metor exits with status 130.

To stop a run on its own, pass `--cycles` or `--for`.
