# APFSearch core

Rust owns metadata persistence, Everything-style query evaluation and immutable search snapshots. `filesystem.rs` reads metadata, directories and mounted volumes; `file_events.rs` owns file-change notifications and their callback lifetimes. `scanner.rs` schedules bounded traversal and reconciles events. The AppKit app, XPC service and CLI use the same JSON C ABI; there is no separate CLI query implementation.

## Build and verification

```sh
PCRE2_SYS_STATIC=1 MACOSX_DEPLOYMENT_TARGET=15.0 cargo build --release
PCRE2_SYS_STATIC=1 MACOSX_DEPLOYMENT_TARGET=15.0 cargo test -- --test-threads=1
```

PCRE2 must be linked statically for a redistributable application. `.cargo/config.toml` supplies these environment defaults when Cargo is run from this directory; the outer build script also sets them explicitly. Artifacts are `libapfsearch_core.a` and `libapfsearch_core.dylib`. ICU4X case mapping, Unicode NFC normalization, SQLite and Roaring bitmaps are compiled into the core.

## ABI and requests

- `apfsearch_engine_open(const char *databasePath)` returns an opaque handle or null.
- `apfsearch_engine_call(handle, const char *json)` returns allocated UTF-8 JSON.
- `apfsearch_engine_free_string(char *)` frees every response once.
- `apfsearch_engine_close(handle)` signals background work to stop and releases the caller's handle. The caller must prevent new/overlapping calls during close.

Calls are thread safe. Every response contains `success` and `protocol_version: 2`; errors contain a readable `error`. JSON strings are owned by the caller until explicitly freed.

| Operation | Parameters and behavior |
| --- | --- |
| `scan` | `roots: [absolute paths]`, `watch: true` by default. Starts the watcher before scanning. A new request cancels/joins the previous worker, replaces scope, and starts again. `watch:false, wait:true` provides deterministic synchronous fixture indexing. |
| `status` | Online `count`, current `generation`, `scanning`, `state`, configured `roots`, inaccessible `uncovered` paths, and errors. |
| `query` | `text`, `offset`, `limit` (maximum 10,000), optional `generation`, `request_id`, and `sort:[{field,ascending}]`. Returns rows, full count, generation, timings and coverage warnings. |
| `retain_snapshot` | Optional `generation`. Returns a `snapshot_lease` that pins metadata, macros, exclusions and relative-date evaluation time. |
| `renew_snapshot` / `release_snapshot` | Renew or release `snapshot_lease`. A maximum of eight active leases is retained; each expires after five minutes without access. |
| `query_plan` | Returns `requires_content`, `requires_properties`, and `requires_extraction` after macro and exclusion expansion. |
| `content_candidates` | Same query fields; content and extracted-property predicates use three-valued evaluation to return conservative metadata candidates, including `NOT content:`. Native extraction can process these rows first. |
| `put_content` | `path`, `text`, `properties`, optional `defer_publish:true` and `expected:{file_id,device_id,size,modified_ns,changed_ns}`. Native extractors supply this expected identity and timestamps; a changed source is rejected. Text is limited to 32 MiB per extracted file. |
| `publish_content` | Publish one snapshot after a content extraction batch. |
| `cancel` | `request_id`; matching queries/duplicate jobs observe cancellation. `request_id:"scan"` cancels the current scanner. |
| `duplicates` | `mode: "content", "name", or "size"`, optional generation/request_id. Content uses size groups then cancellable BLAKE3 reads; hardlinks are separately identified. APFS clone relationships are explicitly unknown. |
| `preferences` | Read preferences, or `action:"set", values:{...}` / `set:{...}`. Supported keys: macros, bookmarks, exclusions, history, settings. |
| `import_file_list` | `rows` containing at least `path`; optional name,size,modified,created,is_dir. Use a separate database. This creates an offline-only index with ordinary query semantics and no local file reads. |
| `volumes` / `volumes_changed` | Returns current mounted volume information. The active event stream already subscribes to Disk Arbitration and reconciles changes. |
| `stop` | Stop the scanner and watcher. Persisted watched roots automatically resume when the database is reopened. |

Rows contain path/name/extension, logical size, created/modified/changed seconds and modification/change nanoseconds, type, symlink flag, inode, parent inode, volume identity, filesystem flags, extracted properties and `content_indexed`. Offline imports are marked `offline:true` at response level. Directory size is unknown, so directory rows do not match `size:` predicates.

## Correctness boundaries

Each query sees one immutable metadata generation. Two prior snapshots remain available for ordinary pagination. Multi-page jobs supply `snapshot_lease` to retain their generation and query preferences across further publications. Active page reads renew the lease; long extraction batches also renew explicitly. Expired leases fail explicitly. Extracted text has a separate content revision; a changed revision invalidates an old content query while metadata-only pages remain readable.

SQLite is authoritative. Every metadata/content mutation marks the cache dirty and increments a revision in the same transaction. Cache publication rechecks that revision, so an intervening batch cannot make an old snapshot look current. Event cursors and reconciliation changes commit together. Inaccessible descendants remain in SQLite with `accessible=0` and are omitted from current results; successful rescans make them visible again. Coverage outside a delta's roots is retained.

Name trigrams use Roaring candidate sets followed by actual substring validation. One- and two-byte normalized name literals, extensions and file types have exact postings; supported boolean expressions combine these sets without visiting every file. Size, modification time and creation time use dense numerical columns with 4,096-slot blocks and range bounds. Range queries reject or accept complete blocks where possible and read only contiguous values in remaining blocks. Updates copy affected blocks while existing snapshots remain valid. These derived columns are reconstructed from already decoded cache records in memory, without requiring a cache rewrite for numeric-column reconstruction. Name and path ordering are indexed independently, and path equivalence groups retain secondary sort semantics. Default Unicode text matching uses ICU full case folding and ignores diacritics; explicit modifiers can retain accents or case. Natural sorting retains separate normalized names. PCRE2 matching has explicit backtracking/depth limits. Unsupported functions report errors. Content without an available extractor is unknown, not proof of absence: `!content:x` does not falsely include unreadable files. Direct text reads and hashing reject symlinks/cloud placeholders and require a regular file; uncached text reads have a 16 MiB bound.

The disposable V4 cache uses 212-byte metadata records, a shared UTF-8 pool, prepared Unicode columns, serialized postings, visibility and ordering vectors, and a BLAKE3 checksum. V3 caches remain readable without a forced upgrade rewrite. Checked memory-mapped loading restores owned strings and vectors; it avoids rebuilding Unicode folds, postings and sort orders. This is still an owned runtime representation. A separate bounded changed-ID journal can reconcile a previous prepared cache with newer SQLite rows even after ordinary snapshot publication has consumed its own journal. Missing, corrupt, incompatible or incomplete cache histories fall back to SQLite. Cache failures remain nonfatal to publishing searchable metadata. SQLite FTS5 stores extracted content, while final content matching uses candidate-local reads and exact substring validation. Ordinary publications consume SQLite's changed-ID set: stable slots, shared entries and copy-on-write postings preserve old readers while updating affected records. Full rebuilds remain necessary when a prepared snapshot cannot be recovered or when compaction is warranted. The million-record benchmark measures these costs separately. No claim of NTFS MFT/USN equivalence or complete Everything 1.5 function coverage is made.

## Bounded incremental recovery

SQLite schema 4 retains up to 65,536 distinct cache-change IDs with a transactionally maintained counter. Startup reads only these keys in one ordered outer join, preserving deletions and denied entries, and replays them without rewriting the binary cache. V3/V4 prepared-cache formats are unchanged. The ordinary publication budget remains separate from startup recovery. See [the design and validation boundaries](../docs/INCREMENTAL_INDEX.md).
