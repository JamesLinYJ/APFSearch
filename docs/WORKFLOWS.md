# Workflow implementation notes

These notes describe the current implementation, not a claim of full Everything compatibility. See the [project overview](../README.md) for setup and limitations.

## Current core architecture and resource boundaries

SQLite is authoritative. The in-memory `SearchSnapshot` is immutable after publication and is selected by an `Arc` pin or an explicit lease. Stable entry slots keep historical postings safe across deletion and replacement; incremental publication shares unchanged entry blocks and derived storage where copy-on-write permits. The disposable prepared cache is an acceleration file only: a missing, stale, corrupt, or incompatible cache must fall back to SQLite recovery.

The current prepared-cache format is a segmented `APFMAP04` manifest plus immutable section files. The `APFSEC04` marker identifies the secondary-index root section descriptor/payload; it is not a marker required in every section file. The manifest and each section are written to a private temporary file, flushed and synced, then atomically renamed; the containing directory is synced after manifest publication. Older `APFIDX01` and `APFMAP03` wording belongs to earlier cache implementations and does not describe the current on-disk format. See [Snapshot sharing](SNAPSHOT_SHARING.md) for the current ownership and validation boundaries.

Query work uses one process-wide CPU admission budget. The Rayon pool uses half the available logical CPUs, rounded up and capped at eight; foreground queries acquire one permit and background index/cache work checkpoints its permit. The budget limits pool participation and admission, not all transient allocations, SQLite read latency, thread stacks, mapped pages, or allocator slack.

Derived query products use an eight-shard cache with an entry cap and a machine-size-derived byte estimate. The estimate covers cache products and retained scratch buffers, while active scratch, result-selection vectors, snapshot records, postings, leases, and mapped-file residency remain separate accounting domains. A cache budget is therefore a soft acceleration-cache bound, not a process-memory or physical-footprint limit. Memory diagnostics must report the scope of each measurement and must not infer a multi-gigabyte cause without runtime evidence.

Window leases are capped at 128 and operation leases at eight; the default lease lifetime is five minutes and active reads renew it. These are count/time limits rather than byte-weighted snapshot limits. A window or export that spans publications can retain the complete immutable generation it references, so callers must release leases when the operation or viewport ends.

Cache publication and full snapshot construction have a strict sequencing requirement. Long cache encoding/writing and derived-index construction must not wait for CPU permits while holding the shared `IndexStore` mutex. A full snapshot is built from a dedicated read-only SQLite connection inside one explicit read transaction, pinning the revision and content revision and streaming rows directly into final record storage; the shared store lock is released before CPU-heavy derived-index work. Cache encoding and atomic file publication likewise run after releasing the shared store lock, followed by a short `BEGIN IMMEDIATE` SQLite transaction that updates cache metadata only if the target revision is still current. A consistent snapshot may be slightly older than the newest database revision and remains publishable; the changed-ID journal and later reconciliation provide the catch-up path rather than requiring an immediate rebuild.

With SQLite WAL, the dedicated read transaction provides a consistent view while writers continue, but it can retain WAL frames until the read view closes. The transaction therefore remains bounded to row streaming and is rolled back before long derived-index work. The short cache checkpoint leaves `cache_changes` and `cache_dirty` untouched when the revision advanced during encoding; a stale atomically renamed manifest is rejected by its generation/revision validation, while the journal remains available for later reconciliation and SQLite remains authoritative.

## File actions, query leases, and updates

Bulk actions resolve selected rows against one retained snapshot before presenting a per-file review. Rename rules, destination conflicts, skipped files, and partial completion are shown explicitly. Operation history records intents and completed changes; hard-link aliases share verified identity updates so a batch and its undo do not mistake their own renames for external edits. Changed files still require review rather than automatic undo.

The review binds both source and destination directory identities. Execution and undo use open directories and verified file handles, so a changed ancestor cannot redirect an operation. Moves first capture the entry in private storage on the same volume, verify the captured identity, and publish exclusively. Native Trash receives only that protected path. A failed rollback leaves a recorded recovery path; it never overwrites a newly occupied source name. If no private workspace with trusted ancestry is available, the action fails before mutation. Regular-file copies attempt an APFS clone before descriptor-based copying.

All application-owned protocols and persistent formats start at version 1. Operation history accepts only explicit format-1 records with complete source and destination parent identities and paths; missing fields, unversioned records, and other versions cannot authorize undo. The earlier JSON journal is not loaded. The app starts with `APFSearch/v1` data and `APFSearch.v1.` preferences, without importing, converting, or deleting earlier development data. SQLite accepts only a blank database or the APFSearch format-1 application marker. The current prepared-cache format is described above; references to the earlier `APFIDX01` layout are historical.

Window queries share immutable snapshot data and have a separate budget of 128 leases, including replacement pages awaiting adoption. Eight additional leases remain available for concurrent exports and other operations. Closing windows, discarding replies, and finishing operations release their leases; abandoned leases expire after five minutes without renewal.

Configured updates keep one download or Installer session active at a time. Cancelled packages are removed immediately; opened packages remain available until Installer exits. If APFSearch exits first, a later launch or activation reclaims its abandoned download directories once Installer is no longer running.

## Index scope and file-list imports

A fresh root selection may resolve an intermediate directory alias once. Persisted roots and subsequent filesystem events retain that selected scope. Metadata access rejects intermediate symlinks; a final symlink remains its own searchable entry. Verified system aliases and System/Data firmlink mapping retain their normal behavior.

CSV file lists are decoded incrementally as UTF-8 and staged in bounded SQLite batches. The completed import replaces the list atomically and publishes one search snapshot. Cancellation, malformed input, or a limit failure leaves the previous list intact; a newly imported list is not registered until completion.

Import limits are 1 GiB of source CSV, 2,000,000 data records, 256 columns, 64 KiB per decoded field, 1 MiB per record, and 256 MiB of stored path/name/extension text. Swift-to-core batches hold at most 4,096 rows and 1 MiB of text. Exceeding a limit produces an explicit error instead of truncating the list. File lists must be regular files and must remain unchanged while being read.
