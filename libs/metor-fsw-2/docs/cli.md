# Command-line guide

The `metor-fsw` command builds, packages, and runs targets. It also builds
and publishes pack distributions.

The command line gives local development, CI, and target runs one way to call
the same build and load code. Use it when working with `target.py`, target
bundles, or pack wheels.

See [Packaging](packaging.md) for the artifacts these commands create and the
checks that make them safe to load.

## Common workflows

Develop a pack, then run a source target:

```sh
metor-fsw pack dev systems/adcs-systems
metor-fsw run target.py
```

Build a cross-compiled target bundle:

```sh
metor-fsw package target.py \
  -o dist/target.metor \
  --target aarch64-unknown-linux-gnu \
  --release
```

Build a pack wheel:

```sh
metor-fsw pack build systems/adcs-systems
```

## Target commands

The target commands are:

```text
metor-fsw build
metor-fsw package
metor-fsw run
```

With no target path, commands that need source look for `target.py` in the
current directory.

### Build a source target

`build` evaluates the Python target and provides each artifact:

```sh
metor-fsw build target.py
metor-fsw build target.py --release
metor-fsw build target.py --target aarch64-unknown-linux-gnu
```

A local crate artifact runs Cargo. An installed pack selects the library for
the target triple from its package. Build writes a manifest sidecar beside a
new library unless `--no-manifest-sidecar` is set.

### Run a target

Run source directly:

```sh
metor-fsw run target.py
```

Source runs refresh direct path-source packs and build artifacts by default.
`--no-build` skips both steps and locates existing libraries instead.

Run a packaged target without Python or Cargo:

```sh
metor-fsw run dist/target.bundle
metor-fsw run dist/target.metor
```

Useful run flags include:

```text
--release
--wall
--sim-dt SECS
--cycle-rate HZ
--cycles N
--serve ADDR
```

These flags override target settings. A run exits with an error if a system
hard-stops.

### Package a target

Write a directory bundle or a single `.metor` file:

```sh
metor-fsw package target.py -o dist/target.bundle
metor-fsw package target.py -o dist/target.metor
metor-fsw package target.py -o dist/target.metor \
  --target aarch64-unknown-linux-gnu --release
```

Check whether a bundle's copied source still emits the same wiring IR:

```sh
metor-fsw package --check-ir dist/target.metor
```

Use this check in CI to find config drift or input that changes between runs.

## Pack commands

The pack commands are:

```text
metor-fsw pack dev
metor-fsw pack build
```

They read pack settings from the pack project's `pyproject.toml`.

### Develop a pack

Run this after changing pack types, params, or ports:

```sh
metor-fsw pack dev .
```

The command builds the host library, describes the pack, and writes the local
`.metor/<module>` payload. The `metor_build` editable backend also runs this
during `uv sync`.

Before a source target command runs, the CLI refreshes each direct editable
pack dependency. Cargo makes an unchanged refresh cheap.

### Build a pack wheel

Build the host triple into a wheel under `dist/`:

```sh
metor-fsw pack build .
metor-fsw pack build . --wheel-out out/
```

Pack wheel builds always use release mode and strip their libraries. The
`metor_build` backend runs this command for `uv build`. Hand the wheel to
`uv publish` to release it.
