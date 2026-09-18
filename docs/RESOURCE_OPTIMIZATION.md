# Resource optimization status

This is the historical reconciliation/column-refactor checkpoint. The subsequent
path layout, cache format and scheduling work is documented in
[Shared paths and compact query layout](SHARED_PATH_LAYOUT.md).

The subsequent candidate for shared posting directories, ordered leaves and
snapshot ownership is described in [Snapshot sharing](SNAPSHOT_SHARING.md).
Its validation status does not supersede the measurement boundaries below.

This work preserves the SQLite schema and public query protocol. The changes
below are implementation progress, not evidence that the full resource targets
have been met on a six-million-entry installation.

The completed checkpoint and final comparison for this stage are recorded in
[Compact index stage report](COMPACT_INDEX_STAGE.md).

## Metadata reconciliation

Alias discovery reads the database before collecting filesystem observations.
The filesystem verifier runs outside the store mutex and write transaction.
A changed index revision discards the prepared observations and retries; the
atomic commit consumes only the prepared alias closure. Cancellation before
commit leaves the batch uncommitted. Event progress is not advanced when
preparation fails.

Verification groups sibling paths and reuses a parent descriptor. Parent and
mount identity are rechecked before accepting observations. The booted sealed
system volume remains eligible; other mounted snapshots remain excluded.
The verified-object cache evicts individual objects instead of clearing all
entries at capacity, and unknown link counts are not reusable successes.

Mount checks read the current kernel table using a reusable per-thread buffer.
Parsed policies and firmlink mappings are shared while the mount identity is
unchanged; there is no TTL or notification-only authorization shortcut.
`mount_checks` counts freshness checks and `mount_refreshes` counts actual policy
materializations. Scanner delivery merges records already in the receive queue,
up to the existing 512-record bound, and flushes when the queue is empty. It does
not wait for a timer. Directory enumeration uses at most two workers.

Transient-change requeueing and a complete event-scope audit still need further
work and workload validation.

## Disposable query caches

Pages, matching bitmaps, optional sort arrays and directory aggregates share one process admission
budget: physical memory divided by 64, clamped to 64–256 MiB. Shared product
allocations are charged once while held by multiple cache owners. The existing
512-entry admission bound also remains in place.

The implementation reuses `hashlink`. Borrowed query keys perform hash lookup
without creating temporary strings or cloning sort vectors. Lookup and LRU
touch have expected constant table cost, in addition to hashing the key bytes.
Eight independent shards allow unrelated hits to proceed without the admission
mutex. Each shard maintains LRU order; eviction compares the oldest entries
across shards. Concurrent touches can change the globally oldest candidate, so
this is not a claim of a strictly serialized global LRU.

Insertions and owner disposal coordinate shared allocation accounting through
a separate mutex. Large retired products are dropped after unlocking. Memory
pressure clears disposable ownership; queries holding an `Arc` remain valid.
Snapshot disposal scans at most the bounded cache entries and removes that
owner's references.

`resources.cache_estimated_bytes` is the eviction model's estimate, **not RSS,
physical footprint or allocator-exact memory accounting**. It includes vector
capacities, key allocations and conservative bookkeeping/container allowances.
Allocator size classes, unused retained hash-table capacity and active results
held outside the cache are not measured by this field. Use OS process metrics
for memory acceptance. `cache_budget_bytes` is the model's admission limit.

Directory topology is owned by this budget rather than permanently retained in
each snapshot. A weak build result coalesces overlapping readers, including
trees too large for admission. Active queries keep their own strong reference.

## Compact snapshot storage

The primary table now stores immutable blocks of 4,096 rows. Queries borrow an
`EntryView`; complete `IndexedFile` objects are constructed only at I/O and
operation boundaries. Exact integer columns replace the duplicate numeric
arrays. Numeric range summaries remain rebuildable, with existing query
operand semantics unchanged. Boolean state uses bit flags, labels use block
dictionaries, and nonempty properties use a sparse table.

Strings have block ownership and range references. Normalized names occupy the
first contiguous region. Raw text, folded text and search text share only exact
byte matches. Names and parent paths can reference verified subranges of full
paths. Neither filename nor path queries rebuild paths or normalize stored
names per query. Unchanged columns and blocks remain shared across snapshots.
Repeated text replacement reclaims a block when dead bytes exceed live bytes.

The new disposable cache publishes an `APFMAP02` manifest referencing immutable,
checksummed sections. Columns and text are searched directly from mappings;
startup does not expand them into millions of owned objects. Each updated block
receives a new section. Unchanged blocks and unchanged secondary-index bundles
reuse their previous sections. Changed secondary indexes still serialize a
whole secondary-index bundle; finer posting-section reuse remains open.

Publication syncs new sections before atomically replacing the manifest. Reads
validate lengths, integer overflow, UTF-8, references, stable IDs, slots and
checksums. Active snapshots pin old sections; publication collects unreferenced
sections that are absent from the current manifest. Production does not decode
the old cache format. An incompatible or damaged cache recovers from existing
SQLite metadata, without requesting a filesystem rescan or a database migration.

The opt-in benchmark-fixture converter exists only in test builds. It produces
a new disposable corpus for comparisons and refuses to overwrite its output.

## Validation boundaries

Focused regressions check allocation-free borrowed hits and misses, shared
allocation accounting, LRU eviction, owner isolation, concurrent admission and
pressure clearing, and preservation of active results. A reconciliation test
checks that a filesystem callback can acquire the store mutex and that an index
revision conflict triggers a fresh observation pass.

The opt-in reconciliation workload uses 512 temporary hard-link entries rather
than millions of real files. CPU duration uses `getrusage` time values. Short
interval disk counters can remain zero before deferred writes complete; they
must not be interpreted as proof of zero physical writes.

An initial APFS comparison alternated baseline/candidate processes three times
per layout, with three update passes per process (nine samples per variant).
Against the preceding prepared-verification implementation, small-directory
workloads (128 directories, four links each) averaged 67.68 → 34.17 ms CPU and
55.15 → 27.34 ms wall time. The flat-directory workload averaged 12.49 → 12.26 ms
CPU and 12.54 → 12.42 ms wall time. These are fixture results, not whole-volume
recovery or query-tail-latency acceptance. Logical writes decreased substantially
for small directories; flat-directory logical writes were approximately unchanged
(180224 → 180679 bytes per pass). Settled physical-write acceptance remains open.

An intermediate column-layout candidate was compared with the preserved
baseline on the same 5,692,629 visible entries, alternating three runs each.
Every metadata fingerprint and all 16 result-page fingerprints matched. Median
core restoration was 2.53 s versus 0.99 s. These numbers precede segmented
publication and are not final installed-service measurements.

The intermediate candidate's physical-footprint counter was much lower, but
resident memory was about 3.41 GiB. Baseline resident measurements ranged from
3.66 to 4.49 GiB under system compression. Clean mapped pages must not be
treated as free memory; this does **not** establish the 50% memory target.
Several query classes exceeded the permitted 5% relative P95 regression even
though common core queries remained below 100 ms. No blanket latency acceptance
is claimed from this intermediate comparison.

Remaining work includes request-thread CPU admission, foreground scheduling,
relevant-record reconciliation conflicts, temporary-buffer accounting, mapped
base sort columns, finer secondary-section reuse, final same-corpus comparisons,
controlled idle measurements, settled physical-write accounting and signed
installed-service/UI validation. The full optimization plan remains incomplete.
No 50% CPU or 35% memory reduction is claimed by the focused tests.
