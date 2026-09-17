# Ring buffer simplification plan

Implementation completed through the construction, read-path, module, and
documentation changes below. Public API and version-6 layout are preserved.
The optional writer-scan and reclamation-check removals were left out because
they need separate protocol reasoning. Validation results are recorded at the end.

The largest gains are in construction, record access, and documentation. The
shared-memory protocol is comparatively small. Simplify the code around that
protocol before changing its synchronization.

This plan preserves the public API, version-6 region format, 16-byte payload
alignment, and existing features. No feature removal is proposed. Changes that
would remove a feature or narrow supported behavior need a separate decision.

The review covered all six Rust source files, the verification guides, and the
parent crate's ring callers. In particular, `src/port.rs` in the parent crate
uses both `drain` and `try_latest_bytes`.

1. **Establish the behavior that must survive.**

   Preserve heap, caller-owned, and mmap storage; cross-process writer claims
   and owner reclamation; independent readers and backpressure; copying,
   borrowing, async, latest, and drain reads; optional notification support;
   and the Linux/macOS wait primitives.

   Preserve the less obvious behavior too: new readers start at the live edge;
   a blocked write may publish padding; latest reads keep their record pinned;
   drain slices can outlive the iterator; and drain consumption is applied on
   the next mutable operation on the view. Retain the existing errors and
   creation-time panic behavior during this refactor.

   First fix test feature gates. Some mmap tests only check `not(miri)`, and
   async tests reference `stellarator` without checking `notify`. This prevents
   the tests from building with default features disabled.

2. **Use one geometry value throughout construction.**

   `checked_layout` returns an unnamed tuple, `init_region` takes that tuple's
   fields separately, and `Inner` construction repeats those fields in the
   heap, mmap, and attach paths (`src/lib.rs:895–1054, 1216–1394`).

   Extend the existing private `Geometry` type to include the total region
   size. Both config layout and header validation should produce it. Use it
   to initialize the header and construct `Inner` through one helper. Keep
   one authoritative copy of the geometry in `Inner`; derive the mask from
   capacity instead of independently assembling both in every constructor.

   Config layout and external-header validation remain distinct operations.
   Attachment currently accepts valid gaps between sections and extra bytes
   after the data region. Comparing an attached header to the canonical
   creation layout would reject currently supported regions.

   `create_raw` should initialize and retain its backing directly. It currently
   initializes the region and then calls `attach_raw`, reconstructing a backing
   and validating the header it just wrote. The checked geometry is sufficient
   for construction; round-trip tests will still check compatibility with
   attachment. Preserve the length returned by `region()` for each backing.

3. **Validate external input once at the boundary.**

   `attach_raw` and its private `attach` helper both check base alignment
   (`src/lib.rs:1016–1047`). Put attachment alignment and header validation in
   one shared entry point, also used by `config_of`. Keep the alignment check
   before the first typed region access and preserve error precedence.

   `config_of` currently attaches a complete `RingBuffer`, allocating an `Arc`,
   only to extract two fields (`src/lib.rs:1196`). Read the validated geometry
   directly. This removes allocation and handle construction from metadata
   inspection without changing validation.

   Keep the checks for magic, version, architecture, capacity, offsets,
   overflow, overlap, alignment, and truncation. They validate external bytes;
   they are not redundant with config validation. Likewise, keep the record
   length check before conversion to `usize` and payload borrowing.

4. **Make the read APIs share the borrowing path.**

   Implement `try_read_into` using `try_read`, followed by `buf.clear()` and
   `buf.extend_from_slice(&grant)`. This removes `copy_payload`, its raw copy,
   and the zero-fill currently performed immediately before copying
   (`src/lib.rs:863–875, 1530–1541`). Grant drop already performs consumption.
   Leave `buf` unchanged on an empty read or an error. Document that callers
   must reserve enough capacity during initialization to avoid later allocation.

   In `read`, retain the `Located` returned by the successful lookup and build
   a grant from it. Currently the async path discards that value, calls
   `try_read`, repeats settlement and lookup, then uses `expect`
   (`src/lib.rs:1566–1583`). Borrow the relevant fields while waiting instead
   of cloning `Arc<Inner>` on each wait iteration. Preserve predicate checks
   after arming and the loop after a wake.

   Share a small grant-construction helper between ordinary, async, and latest
   reads. Give `ReadGrant` a reference to the reader's cursor, a release
   position, and the payload slice, instead of `Inner` plus a slot index.
   Drop then performs a single store through that cursor reference. Name the
   release position to reflect both behaviors: ordinary reads release through
   the end of the record; latest reads keep the start pinned.
   Both references must remain tied to the view's exclusive borrow so the
   smaller grant preserves the same lifetime and pinning guarantees.

   Use clear private names such as `start`, `payload_len`, and `frame_len`
   instead of `r`, `len`, and `rec` where they describe stored record metadata.
   Keep lookup responsible for publication, wrap handling, and bounds checks;
   avoid introducing a generic reader framework.

5. **Remove indirection where it hides simple data access.**

   Prefer direct access such as `inner.control().committed` and
   `inner.slot(index).cursor` over separate one-line accessors for every atomic
   (`src/lib.rs:676–729`). Keep the two accessors that establish typed region
   references and their local safety explanations. Remove `Inner::reserve`
   if the free arithmetic helper reads just as clearly at its call site.

   Compute the frame length once per write and pass the computed end position
   into publication, instead of calculating it again in `commit`. Keep the
   writer's padding fallback visibly separate from publishing a payload.

   After these reductions, move storage ownership into a private `backing`
   module and layout/header handling into a private `region` module if that
   makes the remaining protocol easier to read. Preserve public import paths.
   Keep writer and reader coordination together; a file per type would add
   navigation without simplifying the protocol. Retain the small `sync` shim
   and the existing public OS `wake` module.

6. **Rewrite all doc comments around observable behavior.**

   `lib.rs` has 1,764 lines, including 512 doc-comment lines. Replace the long
   crate introduction with a short description, one example, and the basic
   rules for backpressure and borrowed reads. Public items should explain
   their operation, result, and any important lifetime or failure condition.
   Private items usually need no doc comment when their names are sufficient.

   Apply this pass to every source file, including test and proof comments.
   Keep unsafe caller obligations under `# Safety`, panic conditions where
   applicable, and concise `SAFETY` explanations at unsafe operations. Add the
   required `PANIC Safety` explanation to invariant panics that remain.

   For example, `try_read` can say:

   > Borrow the next unread record, or return `None` if caught up. Dropping
   > the grant consumes the record and allows the writer to reuse its space.

   Fix these specific inaccuracies as part of the rewrite:

   - The crate introduction and `AttachError::ArchMismatch` mention pointer
     width, but `arch_tag` deliberately does not encode it.
   - `try_read` promises a writer wakeup on grant drop; no such wake occurs.
   - `WakeSource` describes space-freed notifications, but the ring only
     notifies after publishing data or padding. `committed` includes padding.
   - `locate_from` has an unrelated copy-operation safety comment attached to it.
   - Creation requires capacity of at least 16 and representable geometry;
     its current panic description is incomplete.
   - Raw-region lifetimes must cover all ring handles, clones, views, writers,
     and their borrows. External mutation restrictions must be explicit.
   - `data_ptr` must permit a one-past pointer for an empty payload after the
     final record header; that pointer is valid for a zero-length slice, but
     not for dereferencing. Its current contract only permits interior offsets.
   - `WakeSink` says it waits until ready, but `NoWake` returns immediately;
     document that waits may return before readiness and callers recheck.
   - The Miri guide still describes 8-byte heap alignment and an unconditional
     `stellarator` dependency. The implementation uses 16-byte alignment and
     optional notification support.

   Put the short region layout and synchronization argument in one
   `DESIGN.md`: publication, registration, wrap gaps, pinning, and reclamation.
   Link to it instead of repeating those arguments in API docs. Condense the
   three verification guides to commands, actual coverage, and limitations;
   remove historical debugging stories and unsupported claims of complete
   proof coverage.

7. **Keep synchronization checks unless their removal has a specific proof.**

   These are necessary parts of the current design:

   | Mechanism | Why it remains |
   | --- | --- |
   | Registration retry and paired `SeqCst` fences | Prevent a writer from missing a reader that believes its bytes are pinned. |
   | Wrap marker before committed position | Prevent readers from interpreting gap bytes as a record. |
   | Padding-only publication and its wake | Let a caught-up reader skip padding so a large record can later fit. |
   | Fresh reader scan after padding publication | Account for progress and registrations after that publication. |
   | `data_end` | Keep latest reads pinned when padding is the only new publication. |
   | Drain's pending cursor and settlement | Keep all yielded slices pinned even after the iterator is dropped. |
   | Owner CAS during reclaim | Prevent competing reclaimers from freeing a reused slot. |
   | Heap ownership through a raw allocation pointer | Preserve the current allocation/provenance argument. |

   Two smaller removals can be evaluated after the core refactor. The first
   two writer scans could share a snapshot, but doing so can cause extra
   `WouldBlock` results when readers advance between them. That is a poor
   trade for this simplification. The preliminary cursor load in reclaim may
   be unnecessary for a valid dead, nonzero owner tag; remove it only after
   checking the full claim/drop/reclaim interleavings and rerunning the reuse
   models. Keep the owner CAS and cursor-release ordering.

8. **Verify each behavioral change, then review the final diff.**

   Keep existing coverage. Add focused cases for copying into preallocated
   buffers, empty and corrupt reads leaving the buffer unchanged, empty
   payloads at the ring boundary, direct metadata inspection rejecting invalid
   regions, and async reads through padding and spurious wakes. Include a
   padded external layout so constructor consolidation cannot narrow attach
   behavior. Cover ordinary and latest grant release positions and retained
   drain slices after the grant representation changes.

   Run library tests and doctests with default features, no default features,
   mmap only, and notify only. Run the parent crate's port tests for drain,
   latest, and alignment compatibility. Check formatting and rustdoc links.
   Exercise the supported target configurations when their toolchains are
   available, especially 32-bit layout and wasm without default features.

   Run Miri for borrowing, copying, raw attachment, grant/drop, and drain
   changes. Run the geometry and record-bound Kani harnesses when those paths
   change. Run Loom for registration, padding, pinning, claims, and reclamation
   after the shared code is refactored; report scheduling bounds and tool
   limitations rather than describing a bounded run as a complete proof.

   Finish with an independent review focused on slice lifetimes, accepted
   region layouts, error behavior, and accidental protocol changes. Keep
   documentation, construction, read-path changes, and module moves in
   separate reviewable changes.

Two correctness concerns should stay visible during implementation. The writer
stores a 64-bit payload length while readers retain only its low 32 bits, so
payloads above `u32::MAX` need a representability decision before writing.
Also, the explanation that a violated `fits` precondition can only reject or
panic is too strong: with overflow checks disabled, the addition can wrap.
For example, `committed = 0`, `slowest = 16`, and `need = 16` evaluate to a
successful fit. The protocol must establish the precondition; the current Kani
harness treats checked overflow as acceptable and does not establish that
release-build claim. Neither issue justifies weakening existing checks. Treat
any resulting behavior change as a separate correctness change with focused
tests, rather than hiding it in a mechanical cleanup.

Review baseline: all 60 default-feature library tests passed on the current
host. The library also passed `cargo check` without default features. The
no-default-features test build failed because of the missing test feature
gates described above. Miri, Kani, Loom, and cross-target checks were reviewed
but not run for this planning task.

Implementation results:

- Shared geometry now drives initialization and attachment. `config_of` reads
  metadata without allocating, and raw creation no longer reparses its header.
- Copying and async reads share grant construction; the raw copy and repeated
  async lookup are gone. Grants store their cursor reference directly.
- Private `backing` and `region` modules separate storage and layout from the
  protocol. Production Rust source decreased from 2,056 to 1,538 lines,
  including comments and docs across all production modules.
- Doc comments and verification guides were rewritten. `DESIGN.md` records
  the synchronization argument. The inaccurate `fits` fallback claim is gone.
- A separate length-admission fix rejects payloads above `u32::MAX` before
  calculating their frame size. Numeric boundary tests avoid large allocations.
- Independent review caught and corrected an async `Send` regression; a
  compile-time regression test preserves support for compatible custom sinks.

Validation after implementation:

| Check | Result |
| --- | --- |
| Default features | 68 library tests and 1 doctest passed |
| No default features | 62 library tests and 1 doctest passed |
| mmap only / notify only | 65 library tests and 1 doctest passed for each |
| Parent crate's port tests | 21 passed |
| Miri, aarch64 macOS | 57 passed |
| Miri, i686 Linux | 56 passed |
| Kani, full sequential suite | All 19 harnesses passed |
| Loom, at most 3 preemptions | 10 models passed, plus 5 OS wake tests |
| wasm32, no default features | Library check passed |
| Clippy, crate only | Passed with warnings denied |
| Rustdoc | Built with warnings denied |
| Formatting and whitespace | Passed |

Dependency-wide Clippy encountered existing documentation warnings in
`maitake-sync`; the crate-only run used `--no-deps`. Loom and Kani results apply
within their documented bounds; Miri covers the executed schedules.
