# Snapshot sharing checkpoint

This candidate changes snapshot ownership and the copy cost of incremental
updates. It does not change SQLite, the public query protocol, indexed scope or
the meaning of a query. Performance acceptance is separate from correctness;
the local comparison report records failures as well as improvements.

## Ownership and layout

- Text compaction copies live byte ranges and remaps references directly. It
  does not construct a block of full `IndexedFile` values and repack them.
- Newly constructed text blocks pack directory prefixes in reverse byte order.
  A parent or shorter prefix borrows a contiguous range of its descendant's
  bytes; normalized name bytes retain their leading contiguous region. Sharing
  requires exact bytes, preserving case, normalization and imported metadata.
  Updates reuse the path prefix for its parent when equal, with an independent
  value for unrelated imported parents. Query readers still use direct ranges;
  there is no reconstruction or decoding on access. Existing valid caches keep
  their original layout until rebuilt; this changes packing, not the schema.
- Posting maps have 256 stable shared shards. A batch clones only the shards
  it modifies; existing Roaring containers remain the bitmap implementation.
- Incremental postings apply the difference between old and new term sets.
  Common bytes, pairs, trigrams and extensions keep their existing bitmap;
  unchanged directory/symlink membership also stays shared. Pair/trigram
  buffers are reused within the batch and released before other index work.
  This avoids cloning large bitmaps merely to remove and reinsert the same
  slot, and preserves unchanged posting sections across cache publications.
- Name and path orders use shared leaves of at most 4,096 slots. Rank access
  uses cumulative counts; ordered scans iterate slices. Updates copy affected
  leaves, split oversized leaves and merge adjacent underfilled leaves.
- Restored orders are validated as exact permutations of the live slots. Dense
  slot sets use a direct-address bit mask: each slot must consume one set bit,
  and the total must equal the live cardinality. Sparse sets consume a cloned
  Roaring bitmap instead. This removes the separate membership lookup and
  growing seen-set without probabilistic checks or deferred validation. The
  dense representation is selected only when its bytes do not exceed two per
  live slot; the scratch storage is released before restoring postings.
- Column validation follows physical sharing too: aliases of an already
  validated immutable reference range reuse that proof. Every distinct column
  is still checked, including UTF-8 boundaries and the path-separator split.
  Path bounds and separator validation share one pass instead of rereading
  the same references in successive loops.
- Natural-path equivalence uses ordered neighbors instead of a separate
  full-sized path-rank array. Raw-path and ID tie breaking remain unchanged.
- Large retired snapshots have a bounded background reclamation queue. They
  remain counted while queued, and reclamation uses the shared CPU work budget.
  Backpressure bounds retained storage; it is not a timer-driven purge.
- Cancelling a window request does not retire the displayed result lease. A
  successful replacement takes ownership before the old lease is released;
  closing the window also invalidates late responses. An offline service must
  retain every returned snapshot token, including tokens issued by a query.

The derived cache uses the `APFMAP04` / `APFSEC04` format. Immutable sections
are validated before publication and reused while their owners remain alive.
An obsolete or damaged cache is rebuilt from SQLite, not by scanning files.
No production decoder for the previous derived-cache format is retained.

## Diagnostics and acceptance tools

Ordinary status requests do not traverse the index. An explicit
`{"op":"status","diagnostics":"memory"}` request reports allocation identities
deduplicated across current, historical and queued snapshots, lease counts,
payload/capacity categories and allocator counters where supported. These are
structural estimates, not a claim to account for every byte of process memory.
Mapped lengths, resident memory and physical footprint are distinct quantities.

The ignored `prefix_pool_profile` test reconstructs one metadata block at a
time from an explicitly supplied immutable cache, compares every output field,
and reports aggregate text bytes. It neither writes a cache nor scans files.
The ignored `name_dictionary_profile` and `posting_representation_profile`
tests evaluate prospective index representations. Their payload measurements
do not constitute an integrated implementation or a process memory benchmark.

`scripts/compare_shared_layout.py` alternates immutable baseline and candidate
test binaries on matching cache corpora. It checks corpus and result-order
digests and reports every query's uncached and warm P95. `--cases` selects an
explicitly labelled diagnostic subset; it cannot replace the full suite.

`scripts/compare_cache_restore.py` isolates cache recovery with test-only phase
timings and CPU, memory, fault and disk counters. Use corpora whose metadata and
query-result fingerprints were already compared. It retains every alternating
sample and does not substitute for query, event or UI acceptance.

`scripts/compare_snapshot_updates.py` uses a consistent SQLite backup and APFS
clones. It applies ten rename/restore cycles to real metadata rows, holds an
export snapshot across updates, changes window leases and verifies reopening.
Disk counters include close/reopen, rather than treating WAL length as writes.
Setting `APFSEARCH_ACCEPTANCE_INVENTORY=1` enables a separate per-round attribution
run; its allocation-heavy inventory is outside timed sections and should not be
mixed into normal benchmark measurements.

`tests/run_shared_runtime.py` uses an authenticated, separately signed fixture
service and production AppKit controller. Its GUI configuration/report paths
should be in a private temporary directory, so the harness does not need access
to protected user folders. It checks actual display availability, query result
stability, history isolation and service cleanup. Latency ends at AppKit display
submission, not physical keyboard input or compositor presentation. Its idle
fixture has no live filesystem event backlog and does not establish full-volume
idle behavior.

Raw indexes, query text, process captures and machine-specific comparison reports
remain outside the repository. A reduced footprint alone is not sufficient:
assess RSS, page faults, CPU, latency and actual disk activity together.
