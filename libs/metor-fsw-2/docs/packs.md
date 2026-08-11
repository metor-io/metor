# Packs and dynamic loading

A pack groups system entries in one Rust crate. The crate can link the pack
into a host, export it as one shared library, or load it in a worker.

A pack makes a set of related systems available as one loadable and
distributable unit. A target can choose systems from that unit without
statically linking every possible system into the host.

Each load calls the crate's `pack()` function once. The returned `Pack` holds
entry descriptions and constructors.

A pack crate depends on `metor-fsw-2-core`, not on the host. Everything a pack
entry touches — ports, frames, messages, health, the ABI — is in that crate,
and keeping the host out is what lets a pack build for a target the host
cannot, such as `wasm32-unknown-unknown`.

## Writing a pack

A pack can mix function-based systems, struct-based systems, and async tasks:

```rust
use metor_fsw_2_core::{Pack, system};

pub fn pack() -> Pack {
    Pack::new()
        .system("Plant", system(plant::execute).init(PlantState::new))
        .system_type::<NavSystem>("Nav")
        .task("safe_mode", safe_mode)
}

metor_fsw_2_core::export_pack!(pack);
```

`system` registers a function-based cyclic system. An `init` function makes
its state from params.

`system_type` registers a type that implements the system traits, often through
`#[system]`.

`task` registers an async function. The coordinator polls it once per cycle. A
finished task reports `Done` and is not polled again.

Use the feature form when the same crate also ships an `rlib`:

```rust
metor_fsw_2_core::export_pack!(pack, feature = "export");
```

This keeps the fixed C symbol names out of a host that links the `rlib`.

## One driver shape

All entry styles become a `Driver` with three calls:

```text
init
step(now) -> Running | Done
shutdown
```

A cyclic entry always returns `Running`. A task can return `Done` with its
outcome.

The coordinator uses the same driver shape for a static entry, a loaded entry,
and a slot occupant.

## Create and bind

Entry construction has two stages.

The create stage decodes params and builds user state. It does not claim ring
roles. It can fail before the graph starts.

The bind stage receives rings in descriptor order, binds the ports, and makes
the driver. Init then runs on the bound driver.

The split lets the host size and create all rings before any entry binds them.

## Params and descriptions

A pack entry carries:

- its name
- its system descriptor
- its params schema
- optional default params as postcard bytes
- a reloadable flag
- its create function

Descriptions come from types. Describing a pack does not create user state.

A config value overlays declared defaults, then becomes postcard bytes. A
caller that supplies explicit postcard bytes supplies the full params value.

## Reloadable entries

Most constructors can create more than one instance. Slots need this property
because each Load or Reset can create a new instance.

An entry that takes a value through `.state(value)` can create one instance.
The first create moves the value. The pack marks the entry as not reloadable,
and the resolver rejects it as a slot occupant.

## Pack-shared state

`Pack::shared_state` declares state that several entries in one static pack can
use. A target state declaration supplies its params.

```rust
pub fn pack() -> Pack {
    let mut pack = Pack::new();
    pack.shared_state("Bus", open_bus);

    pack
        .system_type_shared::<Reader, Bus>("Reader", |p, bus| Reader::new(p).attach(bus))
        .system_type_shared::<Writer, Bus>("Writer", |p, bus| Writer::new(p).attach(bus))
}
```

The state's lifecycle starts before the first attached system init and ends
after the last attached system shutdown. Attached entries can instantiate once
and cannot fill slots. Each worker calls `pack()` in its own process, so pack
state does not cross a process boundary.

## ABI v10

The current pack ABI version is 10. The host checks the version before any
other call.

The call order is:

```text
fsw_abi_version
fsw_pack_open
fsw_pack_describe

  fsw_pack_create
  fsw_pack_bind_init
  fsw_pack_execute       repeated once per cycle
  fsw_pack_shutdown
  fsw_pack_destroy

fsw_pack_close
```

The indented calls repeat for each instance. `open` and `close` cover the whole
pack load.

`fsw_pack_describe` returns a postcard `PackManifest`. Each item carries its
`SystemDescriptor`, params schema, defaults, and reloadable flag. Entry order
sets the index used by `fsw_pack_create`.

Version 10 descriptors also carry schemas for postcard message ports. The
downlink uses them when it announces message types.

## ABI safety rules

Rust values do not cross the ABI by value. The boundary uses byte ranges,
integers, callbacks, opaque pointers, and `repr(C)` ring handles.

The pack catches panics at each exported call. A panic becomes a null pointer,
a nonzero result, or `FswStatus::Panicked`. No unwind crosses C.

Each side frees its own data. The pack drops pack and instance boxes inside the
same shared library that made them. The host copies manifest bytes through a
callback.

The host treats execute status as an untrusted `u32`. Unknown values become
`Panicked`.

## Port binding

The host sends input and output ring handles in descriptor order. The pack
binds them in that same order.

A loaded system cannot use a host registry capability. The loader rejects an
entry that declares one. Port schemas, params schemas, and message schemas do
cross the manifest.

## Opening a pack

`DlPack::open` performs these steps:

1. Load the shared library.
2. Check `fsw_abi_version`.
3. Call `fsw_pack_open` once.
4. Read and decode the manifest.
5. Resolve the per-instance functions.

The resolver caches one `DlPack` per artifact id. Many system specs can select
entries from that open pack. Each system instance still gets its own create and
bind calls.

If a pack has one entry, wiring may omit its type. If it has more than one, the
system or slot must name the entry.

## Loaded instance states

`fsw_pack_execute` returns one of three states:

- `Running`: call it again next cycle
- `Done`: the task finished
- `Panicked`: stop the instance

A fixed cyclic system treats stray `Done` as running. A task in a slot latches
`Done` and never calls execute again.

An in-process panic destroys the foreign state at once. This releases its input
reader slots so stopped code cannot block upstream writers.

## Teardown order

The host drops each instance before it closes the pack. It closes the pack
before it unloads the shared library. It frees ring storage last.

```text
destroy instances -> close pack -> unload library -> free rings
```

This order keeps every function pointer, state pointer, and ring view valid for
as long as code can use it.

## Manifest sidecars

The build tools can write raw manifest bytes beside a library:

```text
libadcs_pack.so
libadcs_pack.so.manifest
```

Pack module generation and bundle checks prefer the sidecar. They do not need to load
the library.

Generated Python modules record a SHA-256 hash of the manifest. Resolve rejects
a module whose hash does not match the built or shipped pack.
