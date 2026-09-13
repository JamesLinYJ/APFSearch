# Bounded incremental indexing

SQLite remains authoritative. This change does not scan filesystem roots, read file contents, change event-cursor commits, disable durability settings, or alter the prepared-cache file format.

## Changed-ID journal

The cache journal retains at most 65,536 **distinct** persistent IDs. A primary-key conflict is a no-op, so repeated filesystem notifications do not consume its capacity. A singleton counter is maintained by insert/delete triggers in the same transaction as the metadata and journal rows. Its overflow trigger runs only when the counter exceeds the bound: it marks history incomplete and clears the disposable journal. Subsequent metadata changes stop adding cache IDs until an actual checkpoint establishes a new baseline.

Ordinary inserts no longer execute `COUNT(*)` or scan the existing journal. The counter starts at zero when an empty format-1 index is created, not on every startup. Delete/checkpoint and overflow remain linear in the bounded number of journal IDs. Counter updates touch one SQLite row; they do not introduce per-file transactions or fsync calls. Existing transaction batching, WAL, and synchronization settings are unchanged. This is not a zero-write or SSD-lifetime guarantee.

Format 1 opens do not perform schema writes. Only an empty database or an APFSearch format-1 database is accepted; older development indexes are never migrated or rewritten. The application uses a fresh versioned data directory.

## Recovery and delta reads

Both publication and cache recovery read a bounded journal as the driving table of an ordered outer join. Each ID performs a primary-key lookup in `files` and an indexed content lookup. Deleted or inaccessible entries become tombstones. There is no separate ID vector followed by a full decoded-row vector and hash map. A limit-plus-one sentinel rejects an oversized delta instead of returning a silently truncated update.

Cache replay is pinned to one SQLite read transaction and has the journal's hard bound, independent of the ordinary publication budget. More than 2,000 changes can therefore recover a small prepared cache as well as a large one. Replay does not checkpoint SQLite or rewrite the binary cache. Missing, corrupt, overflowed, or incompatible histories still use authoritative SQLite recovery.

The ID bound does not constitute a fixed byte limit: path lengths and extracted properties vary. Large batches can still allocate substantial metadata, and subsequent compaction remains a full derived-index operation.

## Sorted-slot updates

Changed live slots are sorted once. Unchanged slots are copied into the output vector, then galloping searches determine insertion positions through their sorted keys. Reverse bulk moves insert the whole batch; each old slot is moved at most once after the initial copy. Existing items precede replacements with equal comparator keys.

For `n` unchanged slots and `k` replacements, the merge uses O(k) insertion-offset workspace and O(n + k) integer movement. Galloping comparisons across the intervening runs are O(k log(1 + n/k) + k), in addition to sorting the replacements. This removes repeated O(n) `Vec::insert` shifts and the previous 32/33-change algorithm cliff without allocating a second O(n) slot array.

This does **not** make total publication independent of index size. The entry-pointer vector, visibility filtering, order arrays, and changed path ranks still contain O(n) work; untouched file strings and unchanged derived structures remain shared where the existing snapshot implementation permits it.

## Regression coverage

Tests use small synthetic metadata stores and in-memory journals, never a generated filesystem tree. They cover more-than-2,000-change replay, deletion, denied paths, hard-link entries, extracted properties, ordinary-journal consumption, no-op observations, read-only cache recovery, format rejection, counter deduplication/rollback/overflow/reset, query-plan primary-key lookups, refusal of truncated deltas, and exhaustive small sorted merges. A sparse 33-change comparison-count test exercises the old algorithm cliff without a wall-clock threshold.

The macOS validation workflow runs formatting, Rust tests, Clippy, repository consistency checks, and the complete unsigned app/service/CLI build. Signed XPC behavior, foreground UI/IME behavior, whole-volume acceptance, physical disk-write measurements, notarization, and installed-app P95 latency are separate acceptance tasks.
