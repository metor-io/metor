# Saving, reopening, and sharing recordings

Investigated 2026-09-06 against the current working tree, including the new
session cleanup and DB flush implementation. This is a design report; no
application behavior was changed during this investigation.

**Recommendation:** combine optional persistent recording at startup with
**Save snapshot…** during a session. Keep temporary sessions as the default.
Start by opening snapshots through a temporary extracted/imported DB; add
direct tar mmap access if recording size makes extraction a problem.

| Option | User experience | Implementation difficulty |
| --- | --- | --- |
| 1. Move the live DB | Familiar first Save: choose a name, then recording continues there. Sharing the still-changing directory needs a separate snapshot. | Medium for same-filesystem macOS/Linux relocation, roughly a week. High for reliable cross-volume migration, several weeks. |
| 2. Choose location before recording | Clear “Record to…” option; data is retained without a later Save. An optional choice avoids interrupting quick exploration. | Low, roughly 2–4 days for new recordings. Reopening is separate work. |
| 3. Save a snapshot | Save produces a complete file now; recording continues. Best fit for emailing, uploading, and comparing recordings. Show the saved cutoff because later samples are not included. | Medium, roughly 1–2 weeks for consistent export; another 3–5 days for a basic single-session Open/import flow. |
| Direct mmap of snapshot tar | Open large recordings without extracting another copy. | High, roughly 1–3 additional weeks for the read-only storage integration. |

These are code-review estimates for one engineer, including focused tests,
not measured schedules. Shared work overlaps; multi-document UI and broad
platform testing can increase them.

## 1. Moving while writing

**Possible, especially on the same filesystem.** Renaming does not inherently
invalidate open files: Linux documents that file descriptors survive rename;
Apple documents the same-filesystem restriction. A temporary macOS probe here
successfully wrote through an existing mmap after renaming its parent directory.
This establishes the OS behavior, not correctness of a live metor DB move.
([Linux rename](https://man7.org/linux/man-pages/man2/rename.2.html),
[Apple rename](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/rename.2.html))

The problem in our code is path ownership. `DB.path`, cloned `TimeSeries.path`,
and `MsgLog.path` still point to the old directory. Node rollover, metadata,
sealing, hydration, and backfill subsequently use those paths. A safe move
needs a shared storage-root abstraction and a short barrier around filesystem
operations while the directory is renamed and the root is changed. Existing
mapped payloads can stay mapped. A Unix symlink at the old location could
bridge paths in a prototype, but adds alias lifetime and portability issues.

Across filesystems, the chunk system becomes useful: seal/rotate active heads,
direct new chunks to the destination, copy immutable old chunks, then commit
the new location. That requires per-chunk location tracking, failure recovery,
and coordination with eviction/hydration. Message logs also need a migration
protocol; they do not currently expose the time-series seal/store machinery.
It is a storage migration feature, not a directory rename with a copy fallback.
Windows has additional move restrictions, including same-drive directory moves.
([Microsoft MoveFileEx](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-movefileexa))

## 2. Choosing the location at startup

Offer **Temporary session** by default and **Record to…** before connecting;
a CLI `--record-to PATH` would serve scripted launches. Create a new bundle
directly at the selected location and mark it persistent immediately. Display
its name and recording status. Cancellation returns to temporary mode.

The DB already accepts a creation path, and `PanelApp::new(db)` preserves
caller-owned storage. The UI change must happen before DB construction:
today `main.rs` creates the temporary DB before `PanelApp::run()` opens its
connection picker. Adding a location field after connecting would require
option 1 or 3 anyway. Refuse an existing recording path and use an explicit
Open/resume operation rather than calling `DB::create` on existing data.

This is the cheapest useful improvement and avoids migration entirely.
It does not cover the common case “I just saw something interesting; save it,”
so it complements snapshots rather than replacing them.

## 3. Saving a snapshot now

Use **Save snapshot…** initially; repeated Save can replace the same snapshot
atomically. Show “Saved through …” while incoming data continues. The source
session stays active, and an unsaved temporary source still cleans up on quit.

The main work is a **snapshot barrier**. The new `DB::flush()` assumes writers
have stopped; it is not a live, globally consistent snapshot API. Add reversible
pause/drain hooks instead of cancelling whole runtimes. Coordinate ingress,
dynamic producers, LoD, and hydration/backfill; drain accepted WAL data;
capture metadata and pin the selected nodes. Capture matching committed lengths
for sample index/data and message timestamps/offsets/payloads. Then resume writers
and serialize those fixed prefixes in the background, or rotate active heads
at the barrier. Rebuild snapshot headers with the captured lengths. A DB state
lock alone does not cover every writer. Define the cutoff by accepted data at
the barrier, since source timestamps can arrive late or use different clocks.

Export **committed bytes only**. Time-series files reserve about 32 MiB each;
message log files use `AppendLog::create`, which reserves about 8 GiB each.
In a small local probe, a 32 MiB sparse file occupied 16 KiB, but an ordinary
Python tar export occupied about 32 MiB. Sparse-aware tar tools exist, but a
compact recording format avoids depending on sparse reconstruction.

`SealedNode` already exposes compact index/data payloads and checksums.
Extend that idea to active prefixes and messages. The existing
`DB::save_archive` exports component tables as Arrow/Parquet/CSV; it does not
package the complete DB, messages, and workspace for reopening.

### Tar and mmap

`tar-core` is a plausible parser. The reviewed docs identify version 0.1.0 and
describe parsing borrowed byte slices, including memory-mapped archives. It
does not supply our DB reader, ownership model, or recording index.
([tar-core](https://docs.rs/tar-core/latest/tar_core/),
[parser](https://docs.rs/tar-core/latest/tar_core/parse/index.html))

For an **uncompressed tar**, map the file read-only, build an entry-offset index,
and expose bounded payload slices backed by a shared mapping. Preserve mapping
lifetimes and validate sizes, offsets, and typed-data alignment. Mapping the
whole file avoids treating each tar member offset as an OS mapping boundary.
`memmap2` supplies read-only mapping support.
([memmap2](https://docs.rs/memmap2/latest/memmap2/struct.MmapOptions.html))

The larger change is that `AppendLog::open` currently opens writable files,
assumes its header starts at the mapping base, and couples reads to mutable
append state. Arrow buffers also retain that mapping directly. Introduce a
read-only log view over either a file or an archive range, with writable heads
kept separate. A `TarStore` alone is insufficient: today's `NodeStore::get`
copies into `NodeStaging` rather than exposing mapped bytes.

Whole-archive compression, such as `.tar.zst`, requires decompression before
the DB can read its payloads; it loses this direct mmap benefit. Start with
compact uncompressed entries. Compression can be an explicit sharing export
or use independently compressed chunks with a cache later.

## Reopening and sharing

Use a versioned manifest, component/message schemas and metadata, data chunks,
checksums, and workspace layout. Specify byte order and format compatibility;
validate archive paths and payload bounds on import. Keep credentials and connection preferences
out of shared recordings. For remote-backed history, either materialize the
requested data before declaring the snapshot self-contained or report missing
coverage explicitly. A local directory may contain remote-only manifest spans.

**Open** should enter offline inspection at the recording's saved time range.
Opening must not mutate the original artifact; calculated data and layout edits belong
in a temporary working copy or writable overlay. `DB::open` currently starts
persistence/lifecycle tasks and can remove stale staging directories, so it is
not a read-only viewer. Extraction/import into a managed temporary directory
is the simplest initial solution and can reuse the existing DB implementation.

All windows currently share one DB through GPUI globals. An arbitrary second
recording cannot safely be opened by passing another DB to one window. Start
with one recording per process, or replace the entire active session; independent
recordings in separate windows require session-scoped registries and services.
Expose Open through the picker/menu and CLI first; double-click support also
needs platform file-type registration. Distinguish directory bundles and tar
snapshots by their type and validated manifest, even if both use `.metor`.

Implement option 2 first for persistent live recording, then option 3 with
Open/import for ordinary saving and sharing. Keep the existing shutdown and
flush work; replace the save-on-exit UI once immediate snapshots are ready.
Defer live migration and direct tar mmap until their specific benefits justify
the larger storage changes.

Code reviewed: [session storage](../../src/session.rs),
[application lifecycle](../../src/app.rs), [workspace](../../src/workspace.rs),
[DB lifecycle](../../../db/src/lib.rs), [append logs](../../../db/src/append_log.rs),
[time series](../../../db/src/time_series_2.rs), [messages](../../../db/src/msg_log_2.rs),
[node stores](../../../db/src/store/mod.rs), [table export](../../../db/src/arrow/mod.rs).
