# IO in guests: implementation plan (chunk G)

Implements "Guests" of `18-io.md`: a dylib or wasm system takes `Cap<K>`
and allocates through `IoInit` like a host system. The buffer table is
already one arena with its directory at the start (A). That arena crosses
`metor_fsw_create` as one region, shared by a dylib and copied for a wasm
guest. Both sides read and write the directory in place; neither keeps a
second entry list. The side that runs the ctor, the pack, freezes and
rewinds the arena. The host only reads it.

Five tasks, in order. Each ends with:

- `cargo test -p metor-fsw-3 --no-fail-fast`;
- `cargo clippy --all-targets` clean;
- `cargo build -p echo-pack --target wasm32-unknown-unknown`;
- the `python/metor-config/tests` suite;
- one commit.

G1 and G3 also run `cargo +nightly miri test -p metor-fsw-3
--no-default-features --lib io::region` and the Kani proofs they add. G5 builds and tests `examples/adcs-fsw3`.

Order: G1 adds the region form of the buffer table, with no ABI change. G2
bumps the ABI and shares the region with a dylib. G3 copies it for wasm.
G4 checks that a guest's `Reset` and `Load` quiesce like a host
system's. G5 is the gate.

## Depends on

From A (`18a-io-stellarator-impl.md`, "Interfaces other chunks use"):
- `BufTable` (shared by `Rc`, `&self` methods): one arena, a directory
  `{ slots, used, top }` plus entries `{ offset, cap, len, held }` at its
  start, `freeze`, `arena`. `BufRef { table, slot }`, `TableId`,
  `Tables`, `RawBuf`, `BufError` (with `NoSpace`), in fsw-3's core
  (`src/io/buf.rs`).
- `Reactor::{new, source, push, submit, reap, cancel_all, in_flight}`.
  The reactor resolves every `BufRef` through `Tables::raw`, and one that
  does not resolve completes `IoError::BadBuf` without reaching the `Os`.

From B: `Io`, `IoInit` (infallible `alloc`, `IoMemory` at `finish`),
`Table`, `MAX_OPS`, `fn_system::bind`, `Ctor::make(params, &IoInit)`, and
`SystemDef::grants`.

From C: `MakeCx.io`, `IoShared` with `member`, `begin`, `idle`, `end`,
`Member`, `Quiesce`, the handle map `Handles`, `BufTable::rewind` (run
where the ctor runs), the adapter's `quiescing` state and deferred
`Load`, `io_submit` (which freezes members' tables and refuses a
member-less input with `NoIoMember`), and `io_complete`.

## G1. The region form of the table

Files: `src/io/buf.rs`, `src/io/tests/buf.rs`, `src/io/shared.rs`, new
`src/io/region.rs`.

1. **The table.** A's arena already holds its own directory, so the
   region is the arena. G adds a third memory form and two methods:

   ```rust
   enum Arena {
       Making(Vec<u8>),
       Frozen(NonNull<[u8]>),
       /// Memory another party owns or this side mirrors; see `Region`.
       Region(Region),
   }
   impl BufTable {
       /// The frozen arena, for `metor_fsw_create`. `None` before `freeze`.
       pub fn region(&self) -> Option<NonNull<[u8]>>;
       /// A table over an arena laid out elsewhere. Checks the directory: a
       /// hostile one gives `Err`, never an entry outside the region.
       pub unsafe fn from_region(region: Region) -> Result<BufTable, Corrupt>;
       /// The entry for a `BufRef` read from an untrusted `Sqe`: `None` when it
       /// is out of the region or names no entry.
       pub fn checked(&self, at: BufRef) -> Option<NonNull<[u8]>>;
   }
   ```

   A table from `from_region` is frozen: its size never changes. Its
   `raw` reads the directory in place and uses `checked`, so the reactor
   fails a bad `BufRef` with `BadBuf`. It never caches entries, so a
   directory the other side changed is read as it is now.
2. B's `Table` gains `region(&self)` and a second constructor,
   `Table::attach(region: Region)`, over `from_region`.
3. ```rust
   /// An arena's memory when it is not this table's own `Vec` or box.
   pub(crate) enum Region {
       /// Memory another party owns; `owner` keeps it alive (a dylib).
       Shared { base: NonNull<u8>, len: usize, owner: RawOwner },
       /// Memory this side owns, mirroring a wasm guest's arena.
       Mirror { bytes: Box<[u8]> },
   }
   ```

   `RawOwner` is the ring's (`ring/src/lib.rs`), so a table and a ring
   are kept alive the same way.
4. `IoShared::adopt_region(id, Region)` makes the system's member over an
   existing region. On the pack side, `member(id)` on a later make
   rewinds an adopted region as it does any table (C2); the host never
   rewinds a `Shared` or `Mirror` region.

Tests, in `io/tests/buf.rs` and `io/region.rs`:
- nominal: a table built by a ctor, frozen, and attached through
  `Shared` sees the same entries and bytes; a `set_len` on one side is
  read by the other at once, since neither caches the directory;
- off-nominal: a pack-side make into an adopted region that asks for
  more than its capacity fails with B's `IoMemory`, naming the bytes
  needed and available; `rewind` on the pack side then the same
  `alloc`s give the same entries again, and the host's view sees them;
- failure: a directory with an entry past the region's end fails
  `attach`; `checked` of a `BufRef` naming no entry is `None`;
- Miri: the attached table is dropped before and after its owner's last
  release;
- Kani `checked_bufref_in_region`: for any directory bytes and any
  `BufRef`, `checked` returns `None` or a slice inside the region.

## G2. ABI 10, and the dylib shares the region

Files: `src/pack/{mod,def,raw,tests}.rs`, `src/dl/{pack,codec,mod,slot}.rs`,
`src/wasm/pack.rs`, `src/coordinator/error.rs`,
`src/coordinator/table.rs`, `python/metor-config/metor_config/_config.py`,
`tests/dl.rs`, `tests/golden/echo_pack.py`.

1. **The definition JSON** carries `SystemDef::grants` (B), so the host
   checks a guest's grants as it does a host system's. `Instance` and
   `DefCxOwned` do not change: a guest needs only its own `GrantDef`s,
   since its n-th grant is handle slot n.
2. **`metor_fsw_create`** gains one argument before `error`:

   ```rust
   pub unsafe extern "C" fn metor_fsw_create(
       ty: RawSlice, params: RawSlice, def: RawSlice,
       inputs: RawSlice, outputs: RawSlice,
       table: *mut RawRing,          // in: the host's region, or empty; out: the region in use
       error: *mut RawSlice,
   ) -> *mut c_void
   ```

   `RawRing` already is a region plus an optional owner
   (`pack/raw.rs:73-78`), so the table travels in one. An empty region
   (`len == 0`) means "make one".
3. **Pack side** (`pack::make`, `pack/mod.rs:265-309`). The pack runs
   the ctor, so it owns the arena's layout:
   - an empty `table` lets the system's first make build a growable
     arena. After `entry.make` the pack freezes it, and only then writes
     it back through `table`, with an owner whose callbacks hold the
     pack's `Rc` to it. A system with no IO writes back an empty region;
   - a non-empty `table` is attached through `adopt_region` on the pack's
     own `IoShared`, and rewound before the ctor runs (C2). Asking for
     more than its capacity fails the make with `IoMemory`.
   - The library is never unloaded (`dl/pack.rs:38-40`), so the owner's
     function pointers stay valid.
4. **Host side** (`DlPack::create`, `dl/pack.rs:185-229`): when the
   def has a `Host` grant, it calls `IoShared::member(cx.id)` and passes
   the member's region, or an empty one on the first make. After a
   non-null create, a non-empty returned region becomes the member's
   table (`Region::Shared`), already frozen. C's `io_submit` reads it from
   the member as it does a host system's, and no longer refuses the
   system with `NoIoMember`. The host never rewinds it.
5. **`DlSlot` joins the io system** (`dl/slot.rs:127-160, 241-256`). The
   slot's make calls `IoShared::member(cx.id)` when its def has a `Host`
   grant and keeps the `Rc<Member>`. `DlSlot::create` passes the member's
   region to `pack.create`, and stores the region it gets back in the
   member. `same_ports` also compares the candidates' grants, so every
   candidate takes the same caps. A `Load` reaches `create` only after C2's
   quiesce, so the old instance has no op in flight and has been dropped
   (`dl/slot.rs:289-306`).
6. **`ABI_VERSION`** 9 to 10 (`pack/mod.rs:26`), `ABI_VERSION = 10` in
   `_config.py`. The Rust test that keeps the two in sync passes.
7. **No guest models.** A dylib or wasm system whose definition has an
   `End::Device` grant fails its make with `ParamError::GuestDeviceSide
   { system }`. A `DeviceSide` resolves drivers' buffers through the
   target's `Tables`, which a guest cannot reach. The check sits where
   both adapters decode the definition (`dl/pack.rs`, `wasm/pack.rs`).

Tests:
- `pack/tests.rs`: `create` with an empty region over an echo-pack-shaped
  table returns a region holding the ctor's buffer; a second `create`
  given that region allocates into it; a system with no IO returns an
  empty region; a definition without `grants` decodes.
- `tests/dl.rs`: a pack built against ABI 9 is refused with
  `AbiMismatch { found: 9, expected: 10 }`; the existing dylib tests
  pass.
- `tests/golden/echo_pack.py` is re-blessed for `abi_version=10`.
- `dl/slot.rs`: nominal, a slot whose candidates take a `Cap<Uart>` is
  an io member, and its second candidate's create is given the first's
  region; off-nominal, a slot whose candidates take no cap is not a
  member; failure, candidates whose grants differ fail `same_ports`.
- `tests/dl.rs` and `wasm/tests.rs`: a fixture system with a
  `DeviceSide<Uart>` loads as a host system and builds; as a dylib and as
  a wasm module its make fails with `GuestDeviceSide` naming it; a guest
  system with only a `Cap<Uart>` still builds.

## G3. The wasm guest's region is copied

Files: `src/wasm/{abi,pack,step,tests}.rs`, new `src/wasm/io.rs`,
`src/mirror.rs` (`readers` argument).

1. **Create.** `Args::table(region: Option<&[u8]>) -> Result<u32,
   PackError>` writes a 20-byte `RawRing` (`WASM32_RAW_RING`) into guest
   memory. On a remake the host copies its mirror into newly allocated
   guest memory first, so the guest attaches to it. The returned region
   is read back with `Guest::read`, and copied into the host's
   `Region::Mirror`. On a remake it is copied into the existing mirror,
   which keeps its address; the guest's arena is frozen, so the lengths
   match.
2. **Remakes** happen only after C2's quiesce, so no op of the old guest
   is in flight. The guest rewinds its copy and runs the ctor. The host
   never rewinds the mirror; it takes the guest's directory as returned.
3. **`IoCopy`**, in `wasm/io.rs`, fixed at create. The guest's copies of
   `io_sq` and `io_cq` need a second reader. `mirror_config`
   (`mirror.rs:241-249`) hardcodes `max_readers: 1`; it gains a `readers`
   argument. Every existing call passes 1, and the two io copies pass 2.

   ```rust
   pub(super) struct IoCopy {
       region: u32,                         // the guest's table offset
       sq: View<NoWake>,                    // a second reader on the guest's io_sq copy
       cq: View<NoWake>,                    // a second reader on the guest's io_cq copy
       reads: Box<[Option<(Token, BufRef)>]>, // MAX_OPS entries
   }
   ```

   With the second reader, `IoCopy` reads the records without touching
   `Mirror`.
4. **Around each step** in `WasmStep::execute` (`wasm/step.rs:43-59`):
   1. `drain_inputs` copies completions into the guest's `io_cq` copy;
   2. `IoCopy::completions`: for each new `Cqe` whose token is in
      `reads`, copy `Len(n)` bytes from the host mirror's entry into the
      guest's entry, and clear the token;
   3. `guest.execute`;
   4. `IoCopy::submissions`: for each new `Sqe` carrying a `BufRef`,
      check it against the guest's directory with `checked`. For a
      write, send, or open path, copy the entry's `len` from the guest's
      directory into the host mirror's directory, then copy that many
      bytes. Record a read or recv in `reads`. A bad `BufRef` is left
      for the reactor to fail;
   5. `drain_outputs` copies the `Sqe`s to the real `io_sq`.

   No step allocates. A full `reads` cannot happen, because the guest's
   queue holds at most `MAX_OPS` ops.
5. A corrupt directory read during 2 or 4 ends the step like a corrupt
   ring: `Failed("corrupt table")` with a `corrupt` log line.

Tests:
- `wasm/tests.rs`, over echo-pack's `gps` as a module with a host-side
  fake reactor writing `Cqe`s:
  - nominal: the PMTK write's bytes, and its `len`, reach the host
    mirror before the `Sqe` reaches the real ring; a read's bytes reach
    the guest before the step that polls it;
  - off-nominal: a read completing with `Len(0)` copies nothing; a remake
    after a quiesced `Reset` gives the new guest the mirror's bytes, the
    guest rewinds them, and the mirror keeps its address; a write whose
    guest `len` shrank since the last write sends only the new length;
  - `mirror.rs`: a mirror built with `readers = 2` takes two views, and
    one built with 1 refuses the second, as today;
  - failure: a guest that writes a `BufRef` past its region gets
    `BadBuf` from the reactor and the host copies nothing; a guest that
    overwrites its own directory ends `Failed`;
  - allocation: 100 cycles after create allocate nothing (counting
    allocator, as `tests/wasm_alloc.rs`).
- Kani `copy_stays_in_bounds`: for any guest region length, directory,
  and `BufRef`, `IoCopy`'s copy ranges are inside both regions.

## G4. A guest's `Reset` and `Load` quiesce

Files: `src/channel/adapter/tests.rs`, `tests/dl.rs`, `src/wasm/tests.rs`.

Guests follow C2's rule. On `Reset` the channel adapter drops the guest
instance, cancels its ops, and remakes it once the member is idle. On
`Load`, including a `DlSlot` swap, it cancels the ops and defers
`inner.load(name)` until idle. The new instance is given the same region,
and the pack rewinds it before the ctor runs. No guest code changes; this
task pins it.

A `Load` of a candidate that needs more IO memory than the arena's
capacity fails with `IoMemory`: the first candidate's make sets the
capacity. Size the first candidate to cover the others.

Tests, over a dylib and a wasm `gps`:
- nominal: `Reset` with nothing in flight remakes on the next cycle into
  the same region, at the same entries;
- off-nominal: a read in flight at `Reset` delays the remake until its
  cancel lands, and is never delivered to the new instance; the same for
  a `Load`, which emits `Loaded` only once the cancel lands;
- failure: a new instance whose ctor asks for more than the region holds
  fails its make with `IoMemory`, and the adapter reports `Failed`.

## G5. The gate

Files: new `tests/io_guest.rs`, `tests/fixtures/echo-pack`,
`examples/adcs-fsw3/systems/*` (rebuilt only).

1. `tests/io_guest.rs` runs C8's pty test three times: echo-pack's `gps`
   built in (as a host table entry), as a dylib, and as a wasm module.
   Each run:
   - completes `until_fix`;
   - writes `PMTK_RATE_10HZ` to the pty master once;
   - publishes the same `fix` records, compared field by field across the
     three runs.
2. The dylib run also runs under `metor run --cycles 300`, which loads
   echo-pack as a dylib, exits 0, and reports no failed system.
3. A `gps` in a `DlSlot` loads, swaps to a second copy, and swaps back.
   With a read in flight, each swap emits `Loaded` only after that read's
   cancel lands, a cycle or more after the `Load`. Each swap reuses the
   slot's region at the same address. A third candidate whose ctor asks
   for more than the region holds fails its `Load` with `IoMemory`
   naming both sizes, and the slot reports `Failed`.
4. `examples/adcs-fsw3` rebuilds its packs at ABI 10 through `metor pack
   dev`, and its tests pass. Its target file does not change.

## Exit criteria

- G5 passes on Linux and macOS.
- Every existing fsw-3, Python, and ADCS test passes.
- The Kani proofs in G1 and G3 pass.

## Versions

- `ABI_VERSION` 9 to 10, Rust and Python. Every pack is rebuilt.
- `CONFIG_VERSION` stays 2. Chunk F bumps it.

## Not in this chunk

- Async systems taking `Cap<K>` through a reactor of their group's own,
  and the group reactors themselves (deferred, `TODO.md` §8).
- Model systems in guests. A `DeviceSide` resolves drivers' buffers
  through the target's `Tables` on the coordinator thread, which a guest
  cannot reach. Out of scope for v1; G2 refuses them.

## Open questions

1. **Who sizes the region.** A region must exist before a ring-like
   handoff, but a ctor's needs are known only after it runs. This plan
   lets the guest's first make lay out and freeze the arena and hand it
   back, and later makes reuse it within that capacity. So a `DlSlot`'s
   first candidate sets the capacity for all of them. The alternative, a
   byte budget on `SystemConfig`, is not in the design.
2. **`create`'s signature.** The table rides in a `RawRing`, which is a
   region plus an owner. A type of its own would read better at the cost
   of a second layout check for wasm32.

## Risks to check early

- G2: a dylib's table is read by the host's reactor on the coordinator
  thread, and by the kernel under io_uring. The region is plain memory and
  every entry is owned by one side at a time, but the owner callbacks
  must be thread-safe, as `RingExport`'s are.
- G3: the second reader slot on the guest's `io_sq` copy. If a guest
  writes its own ring's reader cursors, the copy still sees only
  records the ring protocol accepts (`14-wasm-impl.md` T1).
