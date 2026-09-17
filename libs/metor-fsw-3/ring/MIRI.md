# Miri

Run the unit tests with the interpreter:

```sh
cargo +nightly miri test -p metor-fsw-3-ring --lib --no-default-features
```

Exercise 32-bit pointer and length arithmetic with:

```sh
cargo +nightly miri test -p metor-fsw-3-ring --lib --no-default-features \
  --target i686-unknown-linux-gnu
```

For stricter aliasing checks or additional thread schedules:

```sh
MIRIFLAGS="-Zmiri-tree-borrows" \
  cargo +nightly miri test -p metor-fsw-3-ring --lib --no-default-features
MIRIFLAGS="-Zmiri-many-seeds=0..16 -Zmiri-preemption-rate=0.1" \
  cargo +nightly miri test -p metor-fsw-3-ring --lib --no-default-features concurrent
```

The tests exercise heap and caller-owned storage, borrowed and copied reads,
wrap gaps, latest pins, drains, claims, reclamation, and concurrent registration.
They include empty payloads at the region boundary and copying into reserved
storage. A scripted wake sink exercises async reads after spurious returns and
padding publication without an executor.

Heap storage is 16-byte aligned, including on 32-bit targets. Miri checks the
raw allocation ownership and pointer accesses on the paths each test executes.
Runs with additional seeds sample more schedules; they do not prove the absence
of races in every execution.

Mmap, OS wake syscalls, and `stellarator` executor tests are excluded under
`cfg(miri)`. The optional `notify` dependency is disabled by the commands above.
A successful run does not cover cross-process mapping or wake behavior.

See [DESIGN.md](DESIGN.md) for the memory and synchronization argument.
