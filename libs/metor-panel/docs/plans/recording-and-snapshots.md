# Persistent recording and snapshot save/open

Status: implemented, 2026-09-06. Implements options 2 and 3 from the
[options report](db-save-options.md).

## Startup and ownership

The connection picker is the single startup screen. It shows favorites,
discovered systems, connection options, and manual address entry alongside
**Temporary**, **Record to…**, and **Open recording…**.

Temporary storage is selected by default. Record to selects a new `.metor`
directory; selecting a path creates no database. Connect reserves the chosen
location, creates the DB, starts services, and connects the selected target.
Discovery and connection configuration work before a DB exists. Cancelling a
path dialog preserves the previous selection, and existing destinations are
never overwritten. Only the recording parent folder is remembered.

`--temporary` and `--record-to PATH` select storage explicitly at launch.
`--open PATH` imports an existing artifact. Embeddings retain `PanelApp::new(db)`
for caller-owned databases, alongside `choose_session`, `temporary`,
`record_to`, and `open_recording`.

Temporary working storage is removed on quit after producers and exporters
stop. Persistent directories retain their data, including queued writes drained
at shutdown, and hold an exclusive ownership lock until finalization completes.
Shutdown failures preserve temporary data for recovery. Caller-owned DBs are
never moved or deleted.

## Snapshot capture

`DB::snapshot()` captures independent committed prefixes. There is no global
checkpoint, writer pause, or live WAL flush. Different time series may end a few
samples apart. Data queued for persistence is included in a later snapshot.

For each node, capture the timestamp index's committed length once and derive
the matching value or message descriptor/payload boundaries. Timestamp indexes
commit last, so every included record is complete even when capture overlaps an
append. Copy headers and metadata; pin existing mappings while streaming their
immutable committed prefixes. The existing structural lock protects the node
and manifest selection against hydration or purge; ingestion continues.

The capture timestamp and duration describe the capture operation, not a single
atomic cutoff across streams. Known unavailable remote ranges are recorded and
shown before saving; snapshots contain locally available history only.

## Save and archive format

Save exports immediately while recording continues. First Save and Save As ask
for a `.metor` filename; later Save reuses it. Save never replaces a recording
directory or the source of an imported recording. One export runs per session,
with progress, cancellation, and completion status.

V1 files are compact, uncompressed UStar archives written with `tar-core`:

```text
db/                 native committed log prefixes, schemas, metadata, manifests
workspace.json      optional layout and selected time range
manifest.json       version, byte order, capture information, gaps, entry hashes
```

Unused sparse capacity is excluded. The final manifest inventories every other
entry by path, byte length, and SHA-256. Directory recordings use a separate
versioned `metor-recording` envelope with their live DB under `db/`.

Export streams into a sibling temporary file, syncs it, checks that the selected
destination has not changed, and publishes it with an atomic rename. Failed or
cancelled exports preserve the previous artifact. Quit joins the exporter before
source cleanup; the Quit command offers waiting or cancellation.

## Open and sharing

Open validates and imports a tar, closed directory recording, or legacy raw DB
bundle into a temporary working copy. It checks format versions, canonical
paths, regular entry types, duplicates, checksums, metadata/allocation bounds,
native headers, record lengths, and seals before opening the DB. Active directory
recordings are rejected; Save snapshot provides a shareable live capture.

Offline sessions restore layout and temporal state without connecting or serving.
Analysis changes affect only the working copy. Closing cleans up that copy, and
Save As can export local changes. Opening from an active panel launches another
process with structured `--open` arguments; embeddings can override the launcher.
Layout restoration resolves the tile tree through its window registration,
avoiding re-borrowing an AppRoot already being updated.

## Verification and follow-ups

Automated checks cover concurrent capture, rollover, pinned nodes through
purge/hydration, compact sample/message round trips, workspace inclusion,
recording ownership, legacy imports, malformed archives, repeat saves,
cancellation, quit cleanup, deferred storage selection, and the shared startup
flow. Regression coverage exercises layout restore from an active root update.
Test preferences and connection history use isolated temporary directories.

Direct mmap of tar entries, compression, live relocation, automatic download of
remote history, resuming recordings in place, multiple DBs in one process, and
OS file association remain follow-ups. Native dialogs and a separate-volume
recording still benefit from manual verification.
