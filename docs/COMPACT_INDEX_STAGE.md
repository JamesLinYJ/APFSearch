# Compact index stage report

For the subsequent shared-path implementation and its separate comparison, see
[Shared paths and compact query layout](SHARED_PATH_LAYOUT.md).

This is a completed implementation checkpoint, not acceptance of the full
resource-optimization plan or a release recommendation.

## Delivered scope

- Immutable blocks of 4,096 entries with borrowed row views, exact integer
  columns, block-owned strings, dictionary labels and sparse properties.
- Copy-on-write updates of changed columns; old snapshot readers remain valid.
- Direct mapped loading of metadata columns and text, without reconstructing
  millions of complete file objects. Base sorting arrays remain owned.
- Atomic manifests and immutable cache sections. Unchanged blocks and unchanged
  secondary-index bundles are reused; old readers pin the sections they need.
- Directory topology participates in the existing process cache budget. Active
  readers can finish after cache ownership is released under memory pressure.
- Existing SQLite data, configuration and public protocol remain unchanged.
  Old derived caches are rebuilt from SQLite, without a filesystem rescan.

## Same-corpus comparison

The preserved baseline and this stage's candidate alternated three times on one
immutable existing corpus: 5,692,629 visible entries, 5,692,705 stable slots.
No large file tree was generated. Each of 16 cases had 25 uncached and 25 warm
queries per process. Canonical metadata fingerprints and all result-page
fingerprints matched across all six processes. Fingerprints verify the corpus
and tested pages, not every possible query or full result set.

| Metric | Baseline, runs 1 / 2 / 3 | Candidate, runs 1 / 2 / 3 |
|---|---|---|
| Restore, ms | 2473.71 / 2591.92 / 2438.70 | 3212.72 / 2851.77 / 2737.88 |
| Resident memory after queries, MiB | 4600.47 / 4600.14 / 4601.08 | 3026.19 / 3024.59 / 3026.16 |
| Physical-footprint counter, MiB | 4593.07 / 4592.74 / 4593.68 | 591.64 / 590.05 / 591.61 |
| Query-phase physical writes, bytes | 0 / 0 / 0 | 0 / 0 / 0 |
| Query-phase additional disk reads, bytes | 688128 / 32768 / 32768 | 0 / 0 / 16384 |
| Query-phase major faults | 62 / 4 / 2 | 1 / 1 / 2 |

Median resident memory decreased **34.2%**. The much larger decrease in the
physical-footprint counter excludes clean mapped pages and is not a valid
claim of equivalent memory savings. System compression/swap were not isolated;
this remains a core-process comparison, not a complete service memory account.

Median restore time increased **15.3%**. The section loader currently verifies
many individual files; its startup cost is a remaining optimization item.

Selected uncached-query P95 values below are the median of the three per-run
P95 measurements, not a pooled end-to-end percentile:

| Query class | Baseline, ms | Candidate, ms |
|---|---:|---:|
| Application filename substring | 0.2355 | 0.2557 |
| Broad filename substring | 1.5687 | 1.9674 |
| Path substring | 271.2556 | 93.6577 |
| Filename regular expression | 311.1126 | 164.1203 |
| Size predicate | 2.4708 | 2.5946 |
| Date predicate | 0.8815 | 0.7937 |
| Deep name-sorted page | 0.2890 | 0.2517 |
| Multiple sort columns | 0.3882 | 0.2700 |

Several classes regress more than 5%. The 50% memory reduction and uniform
query-regression targets are **not met**. Improvements in path and regex scans
do not compensate for those failed acceptance criteria. The measured query
phases had no additional physical writes; this does not establish settled
event-reconciliation write volume or SSD write amplification.

## Verification and artifacts

The ordinary Rust suite, strict Clippy, formatting, localization, application
identity checks and an ARM64 native build were run. New regressions cover
column ownership, integer boundaries, sparse properties, Unicode/string
sharing, corrupt mappings, truncated manifests, interrupted publication,
section reuse/collection and active readers surviving eviction.

A local Developer ID signed application is retained as a validation artifact.
Bundle and nested-executable signatures are checked separately. This stage does
not replace the installed application, publish GitHub assets, or claim installed
XPC/AppKit performance, controlled idle CPU or whole-volume recovery acceptance.

## Reproducing the core comparison

Build a release test executable separately for each preserved source tree. Run
only one benchmark process at a time, alternating baseline and candidate:

```sh
PCRE2_SYS_STATIC=1 MACOSX_DEPLOYMENT_TARGET=14.0 \
  cargo test --locked --release --manifest-path core/Cargo.toml --lib --no-run
APFSEARCH_ACCEPTANCE_CACHE=/path/to/immutable/disposable/cache \
  /path/to/release-test-executable --ignored --exact \
  compact_acceptance_tests::same_corpus_resource_profile --nocapture
```

The ignored `snapshot_cache::fixture_conversion::convert_immutable_benchmark_fixture`
test converts a v1 benchmark archive into a new candidate fixture using
`APFSEARCH_V1_FIXTURE` and `APFSEARCH_V2_FIXTURE`. It is not a production
compatibility decoder. First conversion and its writes are separate from the
steady-state measurements above. Never use a mutable live cache as input.

Request-thread admission, foreground scheduling, relevant-record conflict
requeueing, scratch-buffer accounting and finer secondary-index section reuse
are deferred to a later stage.
