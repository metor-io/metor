# Ring design

The ring has one writer and independently positioned readers. Positions count
bytes from initialization; a power-of-two capacity maps them to physical offsets.
A new reader starts at the current published position. The writer refuses to
reuse bytes at or after the slowest reader's cursor.

## Region layout

Version 6 stores an immutable region header, atomic control words, a reader
table, and a data region. Each reader slot stores a cursor and an owner tag.
Control words store the published position (`committed`), the latest wrap gap
start (`hwm`), and the writer owner tag. Creation uses a compact layout;
attachment also accepts validated gaps between sections and trailing storage.

The base and data region are 16-byte aligned. Each record has a 16-byte header
followed by its payload and padding to the next 16-byte boundary. Records never
cross the physical end of the data region. Empty payloads are valid, including
a zero-length slice immediately after the final header.

## Publication and registration

The writer checks reader cursors before writing record bytes, then publishes the
new position with a Release store. Readers Acquire-load that position before
reading bytes. A reader releases consumed space with a Release cursor store;
the writer's Acquire scan observes that release before reusing the bytes.

Registration claims a free slot at the live edge, executes a SeqCst fence, and
rechecks `committed`. If publication advanced, it updates its cursor and retries.
The writer executes a SeqCst fence between prior publication and scanning slots.
The paired fences prevent the writer from missing a claim while that reader
mistakenly treats an old position as pinned. Slot claim and release also use
Acquire/Release ordering to transfer the slot between owners.

## Wrap gaps and padding

When a record does not fit at the physical end, the writer leaves a gap and
starts it on the next lap. It stores the gap start in `hwm` before publishing
`committed`. Readers load `committed` before `hwm`, then skip a matching gap.
The gap marker can become visible before the new committed position, so lookup
also handles cursors temporarily ahead of the position it observes.

If the gap and record together do not fit behind the slowest reader, the writer
may publish just the gap and notify readers. A reader at the gap can then advance,
allowing a later write to fit. The writer rescans readers after this publication.
Padding counts toward `committed`, but `data_end` excludes a final padding-only
publication when identifying the newest record.

## Borrowed records

A view's cursor pins borrowed bytes. An ordinary read grant releases through the
record's end when dropped. A latest-read grant releases to the record's start,
so repeated latest reads can serve the same record. The view's exclusive borrow
keeps either cursor from moving while the slice is live.

A drain can yield several slices that outlive the iterator. It keeps the shared
cursor pinned and records progress locally. The next mutable operation on the
view applies that progress; its borrow prevents this while any yielded slice is
still in use.

## Ownership and reclamation

The writer claim is a CAS on a shared owner tag. Dropping the writer releases
that claim. Dropping a view clears its owner tag before freeing its cursor.
Reclamation is unsafe: the caller must establish that the owner has stopped,
all its handles are unusable, and its tag has not been reused. A reclaimer uses
an owner CAS before releasing a slot so competing reclaimers cannot free a new
owner's slot. A crash between claiming a cursor and storing its owner tag can
leave an unattributed slot that owner-based reclamation cannot recover.

Heap storage retains its allocation as a raw owning pointer until final drop.
All region pointers derive from that allocation; raw-backed regions instead
rely on the caller to keep the storage live for every handle and borrow.

## Verification limits

[Miri](MIRI.md) checks executed memory accesses, [Kani](KANI.md) checks bounded
sequential properties, and [Loom](LOOM.md) explores modeled atomic schedules.
None establishes every possible execution. In particular, `fits` requires an
ordered cursor snapshot; arithmetic outside its preconditions can wrap in a
release build. Payload lengths occupy the low 32 bits of the header word; the
writer rejects lengths above `u32::MAX` before computing a frame size.
