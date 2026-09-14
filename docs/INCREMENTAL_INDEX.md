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

This does **not** make total publication independent of index size. Visibility filtering, order arrays, and changed path ranks can still contain O(n) work; untouched file strings and unchanged derived structures remain shared where the existing snapshot implementation permits it.

## Copy-on-write entry blocks

Full SQLite recovery streams rows from one ordered SELECT directly into their final reference-counted record allocations. It no longer retains a complete `Vec<IndexedFile>` while allocating those objects. The cursor provides a consistent SQLite read view and is exhausted before normalization and secondary-index construction, so that CPU work does not extend the read transaction. Decoding errors abort construction before a snapshot can be published. Both startup recovery and a required full publication use this path, with the same SELECT and no additional scans or writes.

The entry table stores stable slots in shared blocks of 4,096 record references. Cloning a snapshot copies the block directory rather than incrementing a reference count for every file. Replacing an entry copies only its shared block; further replacements in that block reuse the private copy. Appending at a block boundary leaves every existing block shared. Readers retain their original immutable snapshot and slot identities.

For n entries, block capacity B, and k affected blocks, entry-table cloning and mutation require O(n/B + kB) reference operations. This is not constant-time publication: other indexes and order arrays have their own costs. Numeric columns already use separate copy-on-write blocks; the entry table complements that layout without duplicating text or numerical data. The flat serialized representation and format version 1 are unchanged.

`core/examples/query_candidates_benchmark.rs` constructs up to one million synthetic records entirely in memory. Run the same source in release mode against the baseline and candidate revisions. It measures candidate filtering, full matching, and a single size change against a retained snapshot, and compares every query result against an exhaustive matcher. It creates no fixture files or filesystem scans.

```sh
PCRE2_SYS_STATIC=1 MACOSX_DEPLOYMENT_TARGET=14.0 cargo run --locked --release --manifest-path core/Cargo.toml --example query_candidates_benchmark -- 1000000
```

The matcher measurement intentionally verifies even exact candidate sets; production queries may skip that work. Update timing excludes dropping the resulting snapshot. These measurements exclude SQLite transactions, service publication, result caching, sorting, XPC, and UI delivery. Keep machine-specific measurements outside the repository and do not equate this benchmark with end-to-end latency acceptance.

## Reusable query evaluation

Ordinary case-insensitive name expressions also use a separate hot name-reference column. It shares the records' already normalized strings instead of copying text, and stores references in the same block-level copy-on-write container as the entry table. Initial construction and cache restoration derive the column from records. Renames replace the affected reference, appends extend the column, and deletion remains governed by the snapshot's live mask. Metadata-only changes leave name blocks shared. The additional reference storage is approximately 16 bytes per slot on 64-bit targets, plus the block directory; it is not an extra text pool or persisted format.

The name expression compiler borrows the parser's literal matchers and preserves its AND/OR/NOT tree. It accepts only expressions whose complete behavior depends on the normalized name column; other fields, case-sensitive modes, patterns, content and relationship predicates use the general evaluator. Exact bitmap predicates remain first, so short queries do not lose their existing posting-index path. Candidate trigrams still narrow longer names before the column is read. Tests compare the resulting slot sets with record-based evaluation across Unicode normalization, exclusions, rename, append, deletion, retained snapshots and prepared-cache restoration.

Queries compute content and relationship dependencies once before iterating candidate rows. `QueryEvaluator` borrows the resolved expression immutably, so its execution properties cannot become stale while rows are evaluated. The evaluator retains the existing unknown-content and unknown-relationship behavior, short-circuit order, and per-file content normalization. Exclusions use the same evaluator. This removes repeated expression-tree inspection without changing predicates or introducing another result cache.

Candidate sets are intersected with the pinned snapshot's live bitmap before row evaluation. This replaces a membership lookup per candidate with one bitmap intersection; tombstoned entries remain excluded even when historical postings still contain their slots. An unfiltered query iterates the live bitmap directly.

Matched slots feed a streaming range builder. Consecutive slots become a single inclusive range insertion; isolated slots use direct insertion. Only one pending range is retained, so temporary storage does not grow with the result count. The builder preserves gaps, duplicates, container boundaries, and the maximum slot without an unchecked increment. Sparse input still requires one insertion per match; dense runs require one insertion per run.

Numeric-column initialization uses direct known-bit insertion. The current Roaring implementation's checked append scans for the maximum within dense containers on every call, which is unnecessary when writing an explicitly addressed slot. No unchecked library entry point or fork is used. Compare the actual range builder and library insertion strategies on dense, sparse, and clustered synthetic results with:

```sh
PCRE2_SYS_STATIC=1 MACOSX_DEPLOYMENT_TARGET=14.0 cargo run --locked --release --manifest-path core/Cargo.toml --example bitmap_build_benchmark
```

The benchmark rotates method order and compares complete bitmaps after every sample. Range tracking has a small per-match branch cost on isolated results; measure the full query as well as bitmap construction before attributing an application speedup to it.

The candidate benchmark compares the reusable evaluator with the row API, alternating their execution order and checking both against exhaustive matching. For a first-page measurement through the actual core engine, run:

```sh
PCRE2_SYS_STATIC=1 MACOSX_DEPLOYMENT_TARGET=14.0 cargo test --locked --release --manifest-path core/Cargo.toml --lib uncached_first_page_latency -- --ignored --nocapture
```

This opt-in harness creates one million synthetic metadata records in memory and an isolated empty SQLite database. Each sample publishes a changed snapshot, then calls the engine for a 200-row page and encodes the response as JSON. It verifies the generation, complete match count, and ordered first page against exhaustive matching. The 30 samples per query cannot reuse the preceding snapshot's matching/page caches. The reported duration includes the core query and response encoding, but excludes snapshot construction, XPC transport, and AppKit rendering. `APFSEARCH_PROFILE_ROWS` can reduce the fixture size for diagnostics.

## Regression coverage

Tests use small synthetic metadata stores and in-memory journals, never a generated filesystem tree. They cover more-than-2,000-change replay, deletion, denied paths, hard-link entries, extracted properties, ordinary-journal consumption, no-op observations, read-only cache recovery, format rejection, counter deduplication/rollback/overflow/reset, query-plan primary-key lookups, refusal of truncated deltas, and exhaustive small sorted merges. A sparse 33-change comparison-count test exercises the old algorithm cliff without a wall-clock threshold.

The macOS validation workflow runs formatting, Rust tests, Clippy, repository consistency checks, and the complete unsigned app/service/CLI build. Signed XPC behavior, foreground UI/IME behavior, whole-volume acceptance, physical disk-write measurements, notarization, and installed-app P95 latency are separate acceptance tasks.

## Preference publication and query latency

Preference reads use the committed in-memory value loaded at engine startup. Identical updates return that value without acquiring the index-store lock or issuing SQL. This keeps offline-list query setup independent of a background cache checkpoint: synchronizing unchanged macros and exclusions must not wait for serializing a large index.

Changed preference batches validate all keys before writing, serialize through the store lock, and commit in one SQLite transaction before publishing the new in-memory value. Each writer merges against the latest committed preferences, preserving concurrent changes to other keys. Failed batches publish nothing and roll back all their SQL changes. Tests hold the database lock across reads and identical updates, inject a mid-batch SQL failure, check persistence after reopening, and exercise concurrent disjoint updates.

## Bounded CPU parallelism

Large name-column candidate sets use a single process-wide Rayon pool. The pool is initialized only when a query needs parallel work; its capacity is half the available logical CPUs rounded up, capped at eight. Two-core and single-core environments use the calling thread directly. This is a conservative hardware bound, not an assumption that every core has equal performance.

Each query requests one partition per 32,768 candidates, capped by the available pool budget. Candidate counts, rather than ID spans, determine work size. Bitmap rank/select partitions assign disjoint, balanced candidate ranges without materializing a list of slots or copying names. Each task borrows the same immutable snapshot and checks cancellation while matching; the final bitmap union is independent of task completion order. Sorting and pagination retain their existing snapshot-bound behavior.

A nonblocking permit accounts for the partitions reserved by simultaneous queries. Insufficient capacity for at least two partitions falls back to the serial path instead of adding a waiting query to the pool. Rayon owns worker reuse and work stealing; the application only supplies query admission and partitioning. Initialization failure also falls back to serial evaluation. No parallel worker performs file reads or SQLite writes. Transient bitmap results and thread stacks are additional memory costs; there is no per-worker copy of the index.

An opt-in matcher experiment compares serial evaluation and reused pools of two, four and eight workers over dense and sparse candidate sets:

```sh
PCRE2_SYS_STATIC=1 MACOSX_DEPLOYMENT_TARGET=14.0 cargo test --locked --release --manifest-path core/Cargo.toml --lib parallel_name_matching_latency -- --ignored --nocapture
```

This measures dispatch, matching and bitmap merging, excluding one-time pool initialization and the rest of the application. The uncached first-page benchmark above exercises the production admission policy together with candidate planning, sorting and JSON encoding. Tests also cover sparse partition boundaries, cancellation, exclusions, Unicode, Boolean expressions and simultaneous budget reservations. Installed-app responsiveness, memory pressure and competing indexing workloads require separate runtime validation.

## Reuse boundaries

The core already delegates substring search to `memchr`, regular expressions to PCRE2, case folding to ICU, Unicode normalization to `unicode-normalization`, compressed sets to `roaring`, atomic snapshot publication to `arc-swap`, mapped files to `memmap2`, hashing to BLAKE3, and persistence to SQLite through `rusqlite`. Rayon now owns CPU worker reuse and scheduling. Adding another implementation of these primitives would duplicate existing dependencies.

Keep application-specific behavior explicit. The APFS traversal layer must preserve bulk metadata reads, directory-entry identity and coverage semantics. Snapshot storage must preserve stable slots and block-level copy-on-write. Sorting uses the standard library within bounded runs plus a cancellable merge, because replacing it wholesale with an uninterruptible parallel sort would lose the cancellation contract. The tiny bounded query caches currently retain at most 8 match sets and 32 pages; adopting a general concurrent cache is not justified solely to replace a few `VecDeque` operations. Reconsider these choices when measured workload or requirements justify a different abstraction.

## Compact cache string restoration

Prepared-cache restoration copies validated UTF-8 into independently owned,
roughly 1 MiB character blocks. `SharedText` holds an immutable block plus a
checked range; records and the hot name column share these blocks. This removes
per-string allocations and the temporary offset-to-string hash table from cache
restoration. Strings crossing block boundaries own their bytes separately.
Dropping the arena releases unreferenced blocks; a retained result does not pin
all character data. Cache files remain disposable and use the same format.

On 64-bit targets a reference occupies 24 bytes and stores an `Arc<str>` directly.
An experimental 16-byte reference through `Arc<String>` saved slightly more
space but slowed ordinary name queries through another pointer indirection; it
was replaced. UTF-8 and range checks remain safe Rust, with tests for split
multibyte characters, overflow, mapping removal and independent block release.
Snapshot generations, entry slots and copy-on-write publication remain intact.

On macOS, the following read-only experiment reports phase timings and kernel physical-memory/I/O counters while decoding an explicitly supplied disposable cache:

```sh
APFSEARCH_PROFILE_CACHE=/path/to/disposable/index.snapshot.bin PCRE2_SYS_STATIC=1 MACOSX_DEPLOYMENT_TARGET=14.0 cargo test --locked --release --manifest-path core/Cargo.toml --lib prepared_cache_memory_profile -- --ignored --nocapture
```

It checks for zero process writes during decoding and exercises the complete production decoder. Measurements exclude SQLite/XPC and do not control OS page-cache warmth. Compare repeated fresh-process runs over the same immutable fixture, retaining separate timing and peak-memory results.

## Imported-index request preparation

Imported-index lookup and first-open restoration execute inside the existing user-initiated request queue, before querying or scheduling an export. They no longer run synchronously in the XPC request handler. Protocol checks and the read-only imported-operation allowlist still run before dispatch. The existing registry lock continues to prevent duplicate engine construction; this change does not claim that different imported-index opens are independent or that core initialization is cancellable.

`tests/XPCStartupBenchmark.swift` checks this boundary through one persistent authenticated connection. Run it against a freshly restarted isolated fixture service with the list UUID, expected record count and report path. It first connects to the live index, then sends an imported-index status request and an unrelated live-index status request without waiting between them. It records both reply times and whether the unrelated reply precedes loading. Compile/sign it with the same production client/protocol sources and permitted CLI signing identifier as the other XPC harnesses. It does not create an index, scan a volume or measure AppKit rendering.

The launch agent uses launchd's `Adaptive` process classification. A permanently
`Background` process is intended for work not directly requested by the user and
also throttles first-open restoration while a client is waiting. Adaptive
classification lets the system account for XPC activity and return to background
limits when appropriate. It does not change scan concurrency, write batching,
cache validation, or client authentication, and does not force permanent
interactive priority.

In a same-binary, alternating fresh-service experiment over one million offline
fixture entries, Background first-open replies took 2,373 and 2,310 ms; Adaptive
replies took 699, 689 and 680 ms. All returned the expected count. Direct core
engine opening took 704 ms. This isolates a scheduling penalty rather than a
new search algorithm; page-cache warmth was uncontrolled, and no full-volume
scan or installed-app replacement was performed. Idle energy and concurrent
live-index I/O still require separate validation.

`core/examples/startup_benchmark.rs` separates store/cache restoration from full
engine opening. It requires an existing explicitly offline, non-watched fixture;
engine mode performs normal journal housekeeping. Run cache and engine modes in
separate processes and compare with authenticated XPC measurements rather than
attributing the entire reply latency to cache decoding.

Cache decoding also borrows transient properties JSON directly from the mapping
and pre-sizes owned paths using checked component lengths. The restored snapshot
owns its strings and survives mapping removal. In three alternating million-row
decoder runs, median restoration fell from 816 to 711 ms with essentially
unchanged peak physical memory; decoding performed no process writes. These are
core-only results, not whole-application startup measurements.

A follow-up experiment replaced shared-string lookup plus insertion with a
single `hashbrown::HashTable::entry` operation, retaining randomized hashing and
complete offset/length keys. Three alternating fresh-process decoder pairs did
not establish a benefit: baseline times were 780/697/708 ms and candidate times
685/736/738 ms, with essentially unchanged peak physical memory and zero decode
write deltas. The candidate was reverted to the previously measured source.
Fewer source-level hash operations alone are not sufficient evidence of faster
cache restoration; future storage changes should reduce allocations and total
data reconstruction, and be checked through both decoder and XPC measurements.

## Shared CPU execution and cache verification

`cpu_executor` owns one hardware-sized Rayon pool for name filtering and large
cache checksum verification. BLAKE3's maintained Rayon implementation verifies
the same full payload and produces the same digest; no bytes or integrity checks
are omitted. Payloads below 8 MiB remain serial. Recursive library work must
reserve the entire pool budget atomically; if queries already hold workers,
verification falls back to serial without waiting or consuming the remaining
worker budget. This admission limit bounds pool participation, not every CPU
thread in the application: concurrent small requests still run on their callers.

Three alternating fresh-process runs over the same million-row cache reduced
median checksum time from 184 to 27 ms and total restoration from 713 to 557 ms.
Peak physical memory increased by approximately 0.47 MiB; all six runs had zero
process disk-write and logical-write deltas during decoding. These measurements
used the mapped fixture with uncontrolled OS cache warmth; cold-storage behavior
and the final signed-service build still need measurement. No file-content scan,
cache-format change or new persisted data is involved.

The ignored `prepared_cache_string_layout_profile` inventories owned string
capacities, shared-reference byte counts and distinct Arc allocations from the
same disposable fixture. It is intentionally separate from timing and memory
profiles because its temporary allocation-identity set changes peak memory.


The block implementation's measured cache restoration median was 372 ms versus
550 ms for the preceding per-string implementation, with median peak physical
memory approximately 801 versus 1,035 MiB. These are three alternating fresh
core-process runs on the same million-record synthetic cache, including full
checksum verification, not installed-app or cold-storage results. Broad query
latency is measured separately with 30 uncached samples per query; improvements
in restoration alone do not establish a search-speed improvement. An earlier
comparison accidentally selected a stale Cargo artifact and is excluded;
retained comparison reports identify the exact executable paths and hashes.

Signed-service validation then compared the preceding owned-path/string-table
service with the block-storage plus parallel-checksum service. Both used the
same Adaptive launchd classification, authenticated clients, existing million-row
offline fixture and unchanged AppKit harness. Across three fresh-service opens
per build, median first status latency was 748 versus 428 ms (individual baseline
runs: 1,343/748/722 ms; candidate: 493/428/384 ms). OS page-cache warmth remained
uncontrolled. Loaded service physical memory was approximately 1,038 versus
803 MiB in the window comparison.

Eight warmed query cases, 30 samples each, retained correct results and unchanged
history. Programmatic input through the real XPC service and AppKit display had
P95 around 24–25 ms for both builds; service disk reads, disk writes and logical
writes did not increase during either window run. Two-second disconnected idle
observations also showed zero CPU-time and I/O deltas. These are bounded offline
fixture observations, not long-term energy, full-volume, uncached-query or physical
keyboard/compositor validation. Separate uncached core queries still show timing
variation and remain part of the performance acceptance work. Test jobs were
stopped afterward; the installed application was not replaced.

## Candidate selection and repeated predicates

Name-substring candidate generation collects distinct required byte trigrams,
checks for absent postings before allocating a result, and starts intersection
from the lowest-cardinality posting. Selecting this starting set is linear in
the number of distinct trigrams; it does not sort or change match semantics.
Remaining postings still intersect the result, followed by normal visibility
and exact string validation. Cancellation remains checked during collection and
intersection. No persistent index or disk-access policy changes.

The name-expression compiler also removes repeated identical normalized text
predicates within an AND or OR node. It compares matcher bytes, not user-facing
spellings, and collapses a single remaining child. It does not combine different
fields, case modes, NOT nodes or unsupported expressions. Boolean/Unicode oracle
tests exercise these cases before and after rename, deletion and restoration.

In two alternating million-record uncached core-query comparisons (30 samples
per query), a rare long filename's median fell from 0.051–0.056 to 0.033–0.035 ms;
five identical AND predicates fell from 3.52–3.58 to 1.29–1.34 ms. Ordinary single
and distinct-term queries stayed close to baseline. The repeated-predicate case
specifically measures algebraic simplification; it is not a claim that all
multi-condition searches accelerate by that factor. Timings include core query
and JSON encoding, excluding XPC and AppKit.

The rebuilt, isolated signed ARM64 service was also compared through actual
authenticated XPC and the production AppKit controller. All 24 distinct first
queries matched the baseline's first-page digest, row count and total; history
was unchanged. The service recorded zero disk-read, disk-write and logical-write
deltas during both query runs. Repeated-predicate queries after the first broad
result display took about 30–38 ms versus 43–51 ms before the change. These were
single first-query observations per text, not a statistically robust P95.

The first broad result display took 104 ms in the candidate versus 99 ms in the
baseline, so this run did not satisfy the 100 ms target for every case. Separate
timing-only diagnostics localized most of the extra work to applying the table
row count, rather than query evaluation or transport. Native table initialization
remains a performance investigation; no result cap, hidden pre-query, or changed
display-completion boundary was introduced to conceal it. Neither these offline
fixtures nor the diagnostic hooks establish installed-app or full-volume latency.

Validation of this source revision passed 276 Rust tests, Clippy with warnings
denied, formatting, Swift service/content/file fixtures and localization checks.
The compile-only universal build also passed for ARM64 and Intel. Intel runtime
performance has not been measured; the universal artifact was not installed or
published.

`prepared_cache_incremental_update_profile` separately restores a disposable
cache and applies 64 successive changes only in memory while retaining the
original snapshot. It checks unaffected record sharing, unchanged normalized
string sharing, old/new renamed-query results and zero write deltas. On the
million-row fixture, 56 metadata changes had median 0.049 ms and eight renames
median 3.59 ms. Physical memory rose from about 801 to 866 MiB with both the old
and updated snapshots retained. This does not measure durable commits, FSEvents
visibility latency or long-running memory reclamation.

## Avoiding duplicate work when result tables grow

When a query increases the row count, `noteNumberOfRowsChanged` lets AppKit create
new visible cells from the already-updated data source. The controller now reloads
only changed surviving rows; it no longer immediately invalidates those newly
created cells. This preserves the full row count, lazy paging, native row views
and the existing display-completion measurement. Shrinking results and animated
insertions retain their existing update paths.

All 65 AppKit interaction checks passed, including a growth case that verifies
both surviving and newly visible cell text. Three alternating fresh-process
comparisons used the same signed service, million-record fixture and 24 query
texts. The first broad query's window latency was 78–91 ms before and 73–85 ms
after; medians were 85 and 79 ms. The small sample and overlapping ranges do not
establish a robust P95 improvement. All first-page digests, row counts and totals
matched, history remained unchanged, and service disk-read/write/logical-write
deltas were zero. This removes verified duplicate view work, but does not prove
that native table initialization is fully optimized or that all installed-app
queries satisfy the latency target.

## Sparse removal from prepared sort orders

Small changes no longer test every old sorted slot against the changed-slot
bitmap. The update searches old immutable keys in the old order, sorts their
positions, and copies the intervening unchanged runs. It then uses the existing
sorted-delta insertion routine. New keys are never used to search the old order;
deleted and reactivated slots remain subject to the same visibility rules.

The selection model accounts for the higher cost of natural-key comparisons and
random record/string accesses, relative to bitmap membership. The initial model
counted comparisons equally and regressed for thousands of changes; it was
replaced before acceptance with a conservative relative cost of 32. The ignored
`prepared_cache_order_update_profile` compares the previous linear algorithm and
current algorithm with alternating invocation order and identical outputs. On
the million-row fixture, six samples per variant gave median name-order update
times of 0.63/0.10 ms for one change, 3.22/0.22 ms for 16, 7.41/2.00 ms for 256,
and 7.71/6.03 ms for 1,024 (previous/current). Tested larger batches of 2,048,
4,096 and 16,384 use the linear path and stayed close to baseline. These are
in-memory order-maintenance measurements, not file-event visibility latency.

Three alternating full snapshot-update profiles, each with 64 changes and the
original snapshot retained, measured median rename update times of about
3.38 ms before and 1.99 ms after. Physical footprint after the sequences stayed
around 866 MiB in both variants; logical and disk write deltas were zero. The
new reference-sort test exercises sparse and dense changes, deletion,
reactivation, insertion and cached multi-column orders while retaining and
checking old snapshots. Contiguous order vectors still require O(n) output
copies; this change removes per-row membership work rather than claiming that
all incremental structures now update in logarithmic time.

The separate posting-directory profile found only 1,555 distinct trigrams in
this fixture and about 0.002 ms for its directory clone. That evidence did not
justify replacing the map ahead of the sorting work. It does not establish the
same cost for an index with a much larger filename vocabulary.

## Snapshot-owned metadata labels

Extensions, folded extensions and volume identifiers now share `Arc<str>` values
through a snapshot-owned label pool. Cache decoding interns borrowed validated
strings before allocating repeated values. Incremental publication reuses old
labels and copies the pool only when adding a new value; slot compaction retains
only labels referenced by retained records. There is no global interning table,
and the serialized strings and format version remain unchanged.

On the existing million-row fixture, record size fell from 352 to 328 bytes.
Three alternating cache-restoration measurements put physical footprint near
801 MiB before and 739 MiB after. A separate signed service comparison measured
803/742 MiB. All 24 window-query page digests, totals and row counts matched;
query history remained unchanged and service query disk-read, disk-write and
logical-write deltas were zero. Rust tests, Clippy, Swift fixtures and universal
compilation passed. These checks did not modify the installed application.

Performance acceptance remains open: some uncached synthetic queries and later
queries in the single signed service comparison were slower. The memory result
does not establish unchanged query latency. Cache restoration timing also varied
with process and OS cache state; it is not evidence of a loading-speed win.

An experiment moved repeated-text Boolean simplification from the name matcher
to the parsed query, so candidate planning could also avoid duplicate work.
Four alternating million-row first-page runs (30 samples per query per run)
did not demonstrate a speed gain: the repeated-five-term query remained around
1.3 ms, and other queries fluctuated. The experiment was removed, rather than
retaining extra query transformations without a demonstrated benefit.

The ignored `name_query_phase_latency` harness isolates candidate/visibility
calculation from name verification and checks results against row evaluation.
It excludes queries handled by exact postings, which bypass name verification
in production. It retains an additional shared name-reference column and does
not measure parsing, page materialization, JSON, XPC or UI; its timings cannot
be added to other benchmark results as an end-to-end estimate. This diagnostic
is intended to distinguish candidate planning costs from name traversal costs
before changing storage layout.

A second experiment accessed shared text as a bounds-checked byte slice directly,
avoiding string-slice UTF-8 boundary checks for byte matchers. Four alternating
process runs did not show a stable first-page benefit, despite similar isolated
verification timings. It was also removed. Neither experiment justifies a speed
claim; further work should use layout comparisons within the same process to
reduce interference from executable layout and scheduling differences.

The same-process layout harness now includes the production-style chunked
`SharedText` name column, alongside record dereferencing, borrowed references
and a contiguous text pool. With 30 alternating samples per layout on a million
synthetic names, chunked-column/contiguous-pool serial scan medians were
5.97/5.47 ms for `report`, 6.23/5.64 ms for `analysis`, 4.15/3.03 ms for the Chinese
literal and 3.67/2.92 ms for an absent literal. All matching-slot vectors agreed.
These are serial scans including slot collection, not production parallel query
latencies. The experimental text pool plus offsets allocated about 44.7 MB and
duplicated existing text, so it has not been added to production. The evidence
supports investigating compact shared ownership and locality, not accepting an
extra full name copy as a memory-neutral speed improvement.

## Bounded name packing during initial construction

Initial row construction now packs normalized search names in batches of 4,096
records before publishing them. The existing `TextArena` provides immutable,
UTF-8-validated blocks; the previous individual name allocations are released.
When folded and search names already share their text, both fields reuse the
packed slice. Diacritic-sensitive folded names remain distinct. Incremental
changes still preserve old snapshots and allocate replacement names normally;
this is not a full-snapshot repack on each update. Prepared cache restoration
already uses shared text blocks and is unchanged.

Two alternating baseline/candidate pairs on a million synthetic rows measured
RSS of approximately 740/714 MiB after construction. Construction took
2.16–2.26 seconds before and 2.07–2.19 seconds after; these samples are insufficient
for a robust startup-speed claim. Common uncached query medians remained close,
without a demonstrated broad speed gain. Full result counts and first-page IDs
were checked. This reduces retained allocations without adding a permanent copy
of all names or new disk operations. It is an initial in-memory construction
measurement, not full-volume scan or signed-window acceptance.

The memory inventory now counts `SharedText` owners rather than slice addresses;
distinct slices in one packed block must not be reported as separate allocations.

The same bounded construction step now also packs normalized paths. Name and
path columns use separate arenas; a shared helper preserves aliasing only when
the folded and search strings already share storage. Two alternating pairs
against name-only packing reduced initial-build RSS from about 714 MiB to
699–701 MiB. Unique shared-text owners fell from 1,001,245 to 1,490 while retained
text bytes stayed identical. Construction measured 2.24–2.25 seconds before and
2.26–2.28 seconds after; this is a memory improvement, not a claimed build-speed
improvement. Common first-page query timings stayed close, and all checked totals
and ordered page IDs matched. Full-volume and foreground acceptance remain open.

## Current signed-window regression check

The current core was linked into a private signed service with the same Swift
service sources and production client authentication. Three alternating pairs
against the pre-label-sharing service used the existing million-row imported
cache, without scanning a real volume. All 24 query page digests, totals and row
counts agreed across all six runs. Current-service loaded footprint was
741.6–741.8 MiB versus 803.0–803.2 MiB. Maximum observed query latency per current
run was 73.6–84.0 ms, versus 79.0–85.7 ms for baseline. The earlier slowdown did
not reproduce consistently here; three observations per query do not establish
a robust per-query P95 or rule out regressions in other workloads.

A separate current-service warm run tested eight query classes with 30 measured
samples each. P95 ranged from 23.3 to 25.2 ms; pages stayed stable and history
unchanged. Both experiments measured from a programmatic input notification
through the production 16 ms debounce, authenticated Mach XPC and AppKit display
submission. Physical keyboard input, IME composition and compositor presentation
were excluded. Warm-query results include normal query caching and must not be
represented as uncached matching time.

Service disk-read, disk-write and logical-write deltas were zero during every
measured query sequence. The private service was stopped afterward; the installed
app was not replaced. These checks use cache restoration, so the separate initial
name/path packing savings must not be added to their memory measurements.

## Search-name locality in prepared caches

The encoder now interns search names before writing per-record references. This
places the hot name text together in the existing globally deduplicated pool,
without adding a second text column or changing the format. Remaining strings
still use the same interning table. Checksum verification, atomic publication,
and cache rebuild scheduling are unchanged; existing caches are not proactively
rewritten for this layout change.

Encoding was separated from file publication so layout comparisons can encode
into an in-memory cursor. On the existing million-record fixture, both layouts
were exactly 391,385,179 bytes. Thirty alternating same-process samples per
layout measured candidate/name matching medians of 2.46/0.78 ms for `report`,
2.43/0.81 ms for repeated `report`, and 2.63/0.79 ms for `report | image`
(old/new). Full matching bitmaps agreed; absent and rare literals showed little
change. This isolates candidate planning and name verification, excluding page
materialization, XPC and UI. Encoding the new layout took about 1.30 seconds in
that run; without a paired encoder baseline this is not a cache-write speed claim.

The comparison itself performed no disk writes. One new isolated derived cache
was then exported for signed-window testing, without scanning or rebuilding the
real index. Initial snapshot packing and cache pool ordering address different
construction paths; their individual speed figures must not be multiplied.

Three alternating signed-window pairs subsequently used the identical service
binary with the two immutable cache layouts. All 24 query page digests, counts
and totals agreed. For the repeated-name queries after first table growth,
median service times generally fell from about 7.5–8.5 ms to 4.3–6.0 ms, and
input-notification-to-AppKit-submission times from about 31–32 ms to 27–29.5 ms.
The first large page remained dominated by table setup (77.5/74.9 ms median).
These are three observations per query, not robust P95 estimates. Rare queries
are not claimed to improve by the same factor as broad name matching.

Loaded service footprint stayed near 741.6 MiB for both layouts. Query-stage
disk-read, disk-write and logical-write deltas were zero in all six runs; history
was unchanged. The original fixture cache was restored and the private service
stopped. The encoder change passed Rust regression tests, Clippy and universal
compilation. The installed app has not been replaced; the new layout is produced
on a normal subsequent cache publication, not by an extra rewrite on startup.

## Adaptive membership during ordered paging

A test-only ordered-paging experiment begins with Roaring membership checks. If a page
requires a long scan, it can convert the matching set to a temporary dense bit
mask and continue the same ordered stream. The conversion budget reflects the
number of mask words plus matching slots; it is independent of query text. The
two scan loops are separate, so each later row does not branch on representation.
The mask costs one bit per entry slot (125 KB for a million slots), is released
after the request, and does not add a permanent inverse-rank index. Allocation
failure falls back to the existing membership checks. Cancellation is checked
during both scans and mask construction; anchor resolution and sorting semantics
remain unchanged.

The same-process page-selection fixture compared a full ordered scan, candidate
partial sorting, inverse-rank selection and dense membership. Candidate partial
sorting was much slower on its broad matches (roughly 20–45 ms), so it was not
substituted for the ordered scan. The integrated adaptive method measured about
1.46/0.94 ms for the first `report` page and 2.13/1.08 ms for the Chinese query
(old/new). Early `txt` pages stayed near 0.01 ms; its deeper page was slightly
slower (0.28/0.32 ms). This is not a claim that conversion always wins. Sparse
ordinary requests still use the existing candidate-selection path in the query
planner. All measured pages matched the reference ordering and offsets.


Three alternating signed-window pairs did not establish an overall gain for the
adaptive paging experiment. Some service and window samples were slower despite
the isolated page-selection improvement. Results, history and disk-write checks
passed, but that is not a latency win. Production integration was removed; the
helper is compiled only for tests so further experiments can reproduce the result.
The previously verified prepared-cache name layout remains enabled. The original
fixture cache was restored and the private service stopped after measurement.

## Input scheduling without a fixed timer

The AppKit controller now coalesces text notifications in queued main-thread
work instead of waiting a fixed 16 ms. Each notification immediately invalidates
older replies and cancels outstanding requests. Queued work carries the query
sequence, is cancelled on explicit query execution or window closure, and checks
the native field editor for marked text before dispatch. Composition is not
searched until committed. This changes scheduling, not query semantics.

The 69-check AppKit suite includes synchronous input bursts, native field-editor
composition/commit, stale replies, scrolling, selection, and animation behavior.
A signed isolated million-record service was held constant while baseline and
candidate controllers alternated for three pairs. Each of eight query classes
had two warmups and 30 measured samples per run. Hot input-notification-to-AppKit
display-submission P95 changed from 23.1–24.6 ms to 6.2–9.1 ms across classes and
runs. All 1,536 pages, totals and row counts matched; history was unchanged.
Service physical read, physical write and logical write counters did not increase
during the query intervals. These measurements exclude physical keyboard input,
IME latency and compositor presentation; they do not establish full-volume or
installed-application performance. Removing a timer can issue more requests for
keystrokes arriving in different event-loop turns; cancellation remains in place,
and no new indexing or disk-writing path is introduced.
