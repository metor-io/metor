# WebAssembly packs

A pack can be a WebAssembly module instead of a shared library. The module
speaks the same pack ABI over the same entry points, in the same order. Only
the boundary changes: a library is loaded into the host's address space and
hands back raw pointers, while a module runs under an interpreter whose
pointers are offsets into a linear memory the host can read but the guest
cannot escape.

Two properties come out of that, and they are the reason the backing exists.
A poll is bounded, because execution is metered in fuel and a guest that will
not stop is cut off and reported rather than stalling the cycle. A fault is
contained, because an out-of-bounds access traps the guest and leaves the
host untouched.

Both Rust and Python produce wasm packs. `metor-fsw-2-core` compiles for
`wasm32-unknown-unknown`, so a pack crate can build as a module with no
source change. A `@system` in a target file compiles to one at build time
(see [Python systems](python-systems.md)).

## A module imports nothing

A pack module has no imports: no host functions and no system interface. It
exports the pack entry points and its memory. There is no clock inside, and
nothing needs one, because the host publishes each cycle's timestamp before
bind and times every step itself for `system_status`.

This is what makes a module safe to move around. It can be built on any
machine, carried in a bundle unchanged for every target triple, and reviewed
as a closed artifact.

## Rings inside guest memory

A guest cannot follow a pointer into the host, so its ring regions live inside
its own linear memory. The host asks the guest's allocator for each region,
has the guest format a ring into it, and passes the offsets through bind
exactly as the library path passes host addresses.

The coordinator keeps its own host rings for every port. Once per cycle the
host pumps records across the boundary: new records from each host input ring
into the matching guest ring before the step, and from each guest output ring
back out after it. A snapshot port carries only the newest record; a log port
carries every pending one. A guest can corrupt only its own copy, and the host
validates each record at the copy. Records the host cannot deliver are counted
and reported as `wasm_boundary_dropped`.

The host holds ring handles over guest memory for the occupant's whole life,
so that memory must not move. The store lets memory grow up to the target's
ceiling while the module loads and binds, then pins it. A later attempt to
grow traps and stops the guest.

## Fuel and memory

Each step runs under a fuel grant, `wasm_fuel_per_poll` on the target's
coordinator. Loading and binding cost far more than a step and run under a
separate fixed setup budget, so tightening the per-poll grant never breaks
setup. Running out of fuel is a trap, and a trap is a clean stop.

`wasm_memory_limit_bytes` caps the guest's linear memory. A module whose
initial memory exceeds it is rejected at load.

## Where a module can run

A wasm entry can occupy a runtime slot or stand in a fixed position of the
cyclic chain. A slot occupant gets the control input and status output the
mount appends, and the slot runner drives it through the same
`execute → status` shape as a library or process occupant. A wired entry gets
the descriptor's own ports and nothing else, and steps in registration order
like any static system.

A slot occupant can be reloaded from fresh bytes. The new module's entry must
have the same manifest identity as the one the target resolved; a changed
descriptor is rejected before binding.

## Faults

Any failure inside the guest is terminal for that entry and invisible to the
rest of the target. A trap, an exhausted fuel grant, a moved memory, or a
corrupt record read all fold to the same stopped state a library occupant
reaches when its panic is caught. For a slot occupant that is `Stopped` on
the slot status; for a wired entry the coordinator logs `system_stopped` and
reports it in its status. A stopped guest is never re-entered, but its bridge
keeps carrying inputs so upstream producers never back up on it.

Nothing the guest returns is trusted as a value. A status word outside the
known set reads as a panic, a manifest that fails to decode is an error, and
every trap becomes an error the host handles.
