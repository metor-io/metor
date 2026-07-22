# Packaging

Packaging turns pack and mission source into artifacts that can run without
the source tree that built them.

A pack package makes a set of systems available to mission authors. A mission
bundle fixes one mission's wiring and native libraries for a target. Together
they support pack reuse, repeatable mission builds, and deployment to hosts
that do not have Python, Cargo, or the original repository.

See the [command-line guide](cli.md) for the commands that build, inspect, run,
and publish these artifacts.

## The two package types

Metor FSW produces two main artifact types:

| Artifact | Used by | Contains |
| --- | --- | --- |
| Pack wheel | Mission authors | Typed Python module, native libraries, pack manifests |
| Mission bundle | Mission host | Frozen wiring IR, selected native libraries, build metadata |

A pack wheel can serve many missions. A mission bundle serves one resolved
mission.

For example, a team may publish `adcs-pack` once. Several missions can import
its `Plant` and `Nav` system types. Each mission can then produce its own
`.metor` bundle with different params, edges, and target libraries.

## Pack project metadata

A pack crate is also a Python project. Its `pyproject.toml` names the Python
distribution and the native pack payload.

```toml
[project]
name = "adcs-pack"
version = "0.1.0"
requires-python = ">=3.11"

[build-system]
requires = ["metor-build"]
build-backend = "metor_build"

[tool.metor.pack]
id = "adcs"
crate = "adcs-systems"
lib = "adcs_systems"
module = "adcs_pack"
targets = [
  "aarch64-unknown-linux-gnu",
  "x86_64-unknown-linux-gnu",
  "aarch64-apple-darwin",
]
```

The project name and version identify the distribution. The pack table links
that distribution to a Cargo package, library stem, Python module, and target
set.

The Cargo package and library stem default from `Cargo.toml`. The module
defaults to a normalized form of the distribution name. The artifact id
defaults to the module name.

## Pack manifests

Each pack library describes its entries, params, ports, and message schemas
through ABI v10. The description is stored as postcard bytes in a manifest
sidecar beside the library:

```text
libadcs_systems.so
libadcs_systems.so.manifest
```

The manifest lets tools inspect a pack without loading target code into the
current process. Stub generation, wiring checks, and cross-target builds all
use the same description.

A pack manifest must not change by target. The Rust types, params, and ports
form one contract even when the native code has several builds. A multi-target
wheel or CI matrix fails if its manifest sidecars differ.

## Editable pack layout

Local development writes the same module shape that an installed wheel uses:

```text
.metor/
  adcs_pack/
    __init__.py
    py.typed
    _libs/
      aarch64-apple-darwin/
        libadcs_systems.dylib
        libadcs_systems.dylib.manifest
```

`__init__.py` contains typed system classes, port markers, params, artifact
data, and the manifest hash. A mission imports those classes to record system
specs in its wiring IR.

Using the installed layout during development keeps imports and artifact
selection the same in both cases. A mission can switch between an editable
path source and an indexed wheel without changing `mission.py`.

## Pack wheel layout

A published pack uses one `py3-none-any` wheel for all configured targets. The
wheel is not tied to one Python platform because its Python code selects a
native payload by Rust target triple.

```text
adcs_pack/
  __init__.py
  py.typed
  _libs/
    aarch64-unknown-linux-gnu/
      libadcs_systems.so
      libadcs_systems.so.manifest
    x86_64-unknown-linux-gnu/
      libadcs_systems.so
      libadcs_systems.so.manifest
    aarch64-apple-darwin/
      libadcs_systems.dylib
      libadcs_systems.dylib.manifest
```

At provision time, the host selects `_libs/<target-triple>/<library>`. An
unsupported target fails with the targets that the wheel does provide.

Pack wheels use release, stripped libraries. Builders may use Cargo,
`cargo-zigbuild`, or a project command that leaves the target library in the
normal Cargo target directory.

The wheel records an exact `metor-fsw-abi` requirement. It also records a
compatible minor range for `metor-config` unless the project already declares
those requirements. These pins stop an environment from combining a pack with
an incompatible host or recorder.

## Mission bundles

A mission bundle fixes the wiring IR and every native library that the mission
needs. It can be a directory or a single `.metor` file.

```text
mission.bundle/
  meta.json
  wiring.json
  mission.py
  libadcs_systems.so
  libadcs_systems.so.manifest
```

The `.metor` form contains the same members in an uncompressed tar archive.
The archive uses a fixed member order and clears variable tar metadata.

`wiring.json` holds wiring IR v4 with build paths removed. It is the mission
definition that the target will resolve and run.

`mission.py` records provenance. Bundle load never evaluates it. A CI check
may evaluate the copy later and compare its output with the frozen IR.

`meta.json` records:

- pack ABI version
- target triple when known
- debug or release profile
- package time
- SHA-256 of `wiring.json`
- each pack's source and distribution
- each native library hash
- each pack manifest hash

## Bundle validation

Bundle load checks the ABI version, target triple, wiring digest, library
names, and manifest hashes before it loads native code. Resolve then checks the
IR version, params, ports, and graph.

These checks give failures clear causes. A host can report a wrong target,
stale pack interface, changed library, or changed wiring before the mission
starts.

The library hash checks that the copied native file did not change. The
manifest hash checks that the pack interface still matches the interface used
when the mission was built. They protect different parts of the package.

## Repeatable output

Artifact paths do not form part of mission identity. Packaging removes local
build paths from the IR before it writes `wiring.json`.

Generated pack modules use stable ordering and contain no build timestamp.
Wheel entries use a fixed order and timestamp. `.metor` archive entries use a
fixed order and cleared metadata.

The bundle records a digest of the exact `wiring.json` bytes. A config-stability
check can evaluate the copied `mission.py`, remove location-only differences,
and compare the result with those frozen bytes.

This does not prove that two native builds have the same machine code. It does
make changes in mission structure, pack interfaces, and packaged files visible
to CI and the target loader.

## Cross-target builds

A target library cannot always run on the build host. For a cross build, the
build flow also makes a host copy of the pack and reads its manifest. It writes
that description beside the target library.

This works because the pack interface must stay the same across targets. A
fresh target sidecar that differs from the host description fails the build.

CI may build each native target on a separate runner and assemble the wheel
later. Assembly requires byte-identical manifests from every runner. This
detects source, feature, or tool skew before publication.
