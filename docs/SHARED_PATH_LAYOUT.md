# Shared paths and compact query layout

This report compares the shared-path implementation with the preserved compact
column candidate. The earlier column refactor is described in
[COMPACT_INDEX_STAGE.md](COMPACT_INDEX_STAGE.md). The shared-path checkpoint is included in the 0.1.4 Preview.
This is a staged delivery, not acceptance of every optimization target.

## Implementation

Metadata remains in immutable blocks of 4,096 stable slots. Each block interns
directory prefixes and path suffixes, independently of the display name. Raw,
case-folded and search-normalized text retain their respective byte semantics;
identical fragments and identical reference arrays share storage. New entries
reuse the block's existing fragments. The temporary interning map is discarded
before publishing a snapshot.

Integer columns choose a constant, base plus unsigned 8/16/32-bit deltas, or
uncompressed 64-bit values. Signed order is represented exactly. Numeric queries
translate their existing floating predicate boundaries into exact integer rank
intervals once, then scan the selected narrow representation once per block.
This preserves the existing query language's rounding behavior above 2^53.

Filename matching obtains the name-reference column and text region once per
block. Ordinary path matching checks each distinct directory prefix once, then
checks suffixes and the final slash boundary where needed. Case-sensitive and
diacritic modes preserve their normalization behavior. Full path regexes still
receive one contiguous subject through a reusable query-owned buffer. Natural
sorting compares segmented bytes without permanently expanding all paths.
Result pages construct complete paths and owned JSON only at the output boundary.

The derived cache uses `APFMAP03` manifests and `APFSEC03` secondary-index roots.
Metadata columns map directly. Secondary postings are partitioned into 256 stable
shards; sorting arrays are persisted in blocks of 4,096 slots. Sorting arrays and
Roaring containers still become owned memory during restoration. This is not a
claim that the entire search index is zero-copy or memory-mapped.

Publication reuses unchanged sections and replaces files atomically. Checksums,
reference bounds, integer bounds, UTF-8, path split invariants and slot membership
are validated before exposing a snapshot. Reuse checks file identity and change
timestamps, including secondary dependencies, so a same-length corrupted
replacement is repaired. Old readers retain valid mapped inodes and pin their
dependent sections. SQLite, settings and the public query/XPC protocol are
unchanged. An obsolete derived cache is rebuilt from SQLite, without scanning
the filesystem or maintaining an old cache decoder.

CPU admission now includes computation on requesting threads. Saturated requests
wait on task completion or cancellation. Background builders yield at block
boundaries when foreground work is waiting. Metadata restoration has at most two
file-reading workers. Retained text and numeric scratch buffers share the
existing derived-cache byte budget; active and peak capacities are recorded
separately and idle buffers are released under memory pressure. These counters
account for those buffers, not every allocation inside dependencies.

## Reproduction

Preserve the pre-change source tree, cache and release test executable before
building the candidate. Both executables must contain the same acceptance
harness and use separate, immutable cache fixtures of the same corpus:

```sh
PCRE2_SYS_STATIC=1 MACOSX_DEPLOYMENT_TARGET=14.0 \
  cargo test --locked --release --manifest-path core/Cargo.toml --lib --no-run
python3 scripts/compare_shared_layout.py \
  --baseline-binary /private/benchmark/baseline-tests \
  --baseline-cache /private/benchmark/baseline.snapshot.bin \
  --candidate-binary /private/benchmark/candidate-tests \
  --candidate-cache /private/benchmark/candidate.snapshot.bin \
  --output /private/benchmark/new-comparison --rounds 5 --repetitions 25
```

The comparison alternates AB/BA, validates canonical corpus and result-page
fingerprints, and pools 125 samples per class and cache mode for P95. It does not
use a combined average to hide a slower query class. Each process restores the
cache normally; the OS file cache is not forcibly purged. This is repeat-launch
restoration, not a power-on cold-storage measurement. No large file tree is
generated. The test-only converter accepts an immutable benchmark archive; it
is not a production legacy-format decoder. First conversion is separate from
steady-state measurements.

`tests/build_isolated_service.py` builds an authenticated signed service at a
unique fixture endpoint without bootstrapping it. Combine it with
`tests/build_runtime_window_benchmark.py --service-name ...` to exercise the
production client and controller. Its timing boundary includes input coalescing,
real XPC, decoding, AppKit layout and display submission; it excludes physical
keyboard/IME input and compositor presentation. The large offline cache fixture
prepared by `compact_acceptance_tests::prepare_cached_service_fixture` deliberately
has an empty SQLite row table. It is valid only for cache-backed query/UI timing,
not for testing SQLite recovery, content search or file mutations.

For separate small real filesystem workloads, run the ignored
`resource_workload_tests::reconciliation_resource_workload` on both executables.
It creates 512 tiny hard-link directory entries, verifies file identities and
metadata against the filesystem, and measures three identical update batches.
Build `scripts/process_resources.c` with `xcrun clang -O2 -Wall -Wextra -Werror`
to sample aggregate Darwin counters for the isolated service. Check process
start ticks before calculating deltas; a recycled PID is not the same process.

## Acceptance results

Measured on Apple Silicon using 5,692,629 identical live entries, five alternating
baseline/candidate process pairs, and 25 uncached plus 25 warm repetitions per
query class in each process. Canonical metadata and every tested page/order
fingerprint matched. The baseline is the **preceding compact-column candidate**,
not the published 0.1.3. The following are local desktop observations, not a
controlled hardware lab result: ordinary OS services remained active, and the
first baseline briefly overlapped small Swift validation compilations.
Exploratory, interrupted comparisons are excluded.

| Measure | Compact baseline | Shared-path candidate |
| --- | ---: | ---: |
| Median resident memory after the same queries | 3,026.23 MiB | 1,528.03 MiB |
| Median repeat-launch cache restoration | 3,737.65 ms | 3,188.86 ms |
| Median resident peak through restoration | 3,446.97 MiB | 1,521.48 MiB |
| Median physical footprint after queries | 591.60 MiB | 591.33 MiB |
| Median internal pages after queries | 589.72 MiB | 590.19 MiB |
| Median external/mapped pages after queries | 2,436.52 MiB | 937.84 MiB |

Resident memory decreased **49.5%** and repeat-launch restoration took **14.7%
less time**. Most of the memory change is fewer clean mapped pages; the private
physical-footprint counter did not fall by 49.5%. The comparison includes those
mapped pages and does not count them as free memory. Candidate compression was
zero in these runs; baseline had up to 1.53 MiB after queries. Per-process swap
was not independently measured; pre-existing system swap is not attributed to
this application.

### Per-class query P95

Values are milliseconds; each cell contains baseline → candidate. Uncached means
rebuild the disposable query products against an already restored index, not a
cold filesystem cache. Warm means reuse those products. All requests return at
most 200 rows. Asterisks identify misses of the original percentage-only gates
(5% daily, 15% complex path regex/deep paging).

| Query class | Uncached P95, ms | Warm P95, ms |
| --- | ---: | ---: |
| Empty query | 0.181292 → 0.224208 * | 0.131167 → 0.125875 |
| One-character filename | 0.279750 → 0.236000 | 0.134791 → 0.119583 |
| Application filename | 0.374625 → 0.345209 | 0.141667 → 0.120750 |
| Common filename | 1.783000 → 1.546875 | 0.177375 → 0.159833 |
| Chinese filename | 0.026125 → 0.021125 | 0.013750 → 0.011833 |
| Accented filename | 1.106667 → 0.904125 | 0.155125 → 0.144166 |
| Case-sensitive filename | 0.110625 → 0.110083 | 0.044291 → 0.050083 * |
| Ordinary path | 92.883708 → 56.941708 | 0.158250 → 0.150875 |
| Filename regex | 180.716333 → 168.189792 | 0.223042 → 0.227500 |
| Size filter | 2.554333 → 2.249208 | 0.165667 → 0.150292 |
| Modified-date interval | 1.038584 → 0.738292 | 0.136375 → 0.130250 |
| Extension filter | 0.514833 → 0.541625 * | 0.138708 → 0.123250 |
| Exclusion | 0.399667 → 0.327750 | 0.133958 → 0.126250 |
| Boolean OR | 3.996167 → 3.856250 | 0.143042 → 0.132250 |
| Deep page (offset 10,000) | 0.271417 → 0.254083 | 0.126375 → 0.115209 |
| Size/date multi-column order | 0.296542 → 0.330917 * | 0.129292 → 0.121334 |
| Path across prefix boundary | 83.702917 → 32.750333 | 0.169959 → 0.142583 |
| Chinese path | 87.956541 → 84.911333 | 0.056875 → 0.050292 |
| Case-sensitive path | 838.590417 → 142.400042 | 0.166542 → 0.149041 |
| Anchored full-path regex | 912.046541 → 967.019709 | 0.204708 → 0.199750 |
| Backreference full-path regex, deep page | 2081.305416 → 1972.615167 | 0.234833 → 0.310709 * |

The five percentage-gate misses add approximately 43, 6, 27, 34 and 76
microseconds respectively. They are accepted for this Preview checkpoint as
small absolute differences, **not relabeled as passing the original gates**.
The uncached anchored full-path regex adds about 55 ms (+6.0%), within its 15%
allowance. Regex remains a potentially long-running, cancellable operation.

### Signed service and native interaction

The signed fixture used the production authenticated XPC client and AppKit
search controller, eight daily query classes, three warmups and 30 measured
samples each. Input-to-layout/display-submission P95 ranged **1.04–6.47 ms**;
all requests succeeded and query history remained unchanged. This includes
coalescing, XPC, decoding and AppKit work, but excludes physical input/IME and
compositor scanout. It is the offline cache fixture described above, not an
installed, actively indexing whole-volume database. A separate native controller
regression run passed **84/84** checks, including selection, scrolling and window
behavior. The isolated launchd service was unloaded and its process exit verified.

The same signed service, after workload drain, was observed for **60.59 seconds**:
average CPU was below **0.001%**, with zero measured reads, writes, logical writes
or page-ins during that interval. It had no live filesystem backlog; this does
not establish whole-volume idle behavior under arbitrary application activity.

### Updates, disk activity and remaining limits

Five alternating small real-filesystem process pairs produced 15 update batches
per side over 512 hard-link paths. Identities and metadata were checked against
the filesystem. Median batch duration was **25.49 → 28.17 ms** (+2.68 ms); total
CPU was **527.66 → 549.01 ms** (+4.0%). This is accepted as a small checkpoint
cost. It does **not** meet a no-regression update-time gate or prove the earlier
50% whole-volume reconciliation CPU goal.

Measured logical writes over those batches were **8,711,320 → 8,274,040 bytes**.
OS-attributed physical write counters were **4,489,216 → 0 bytes** inside the
measurement intervals. Deferred writes and attribution prevent interpreting
that zero as zero SSD writes or a 100% improvement. Query phases reported zero
writes in all ten runs. Additional query-phase reads varied from 0–20 KiB on the
baseline and 0–772 KiB on the candidate, so strict steady-state read nonincrease
is **not established**. Restore reads/page-ins decreased with the smaller cache,
but neither disk counters nor WAL size prove device-level write amplification.

The first format-conversion timing overlapped a compiler and is not used as a
performance result. Conversion correctness passed, but an uncontended conversion
time/write report remains open. The format is derived-only: SQLite metadata,
settings and public protocol stay unchanged, with no required filesystem rescan.

Validation passed **314 Rust tests**, strict Clippy, **121 Swift service/content/
file checks**, localization in eight languages, protocol/identity checks and the
84 AppKit checks. Large-corpus fixture tests remain opt-in. Hardware/OS limits,
installed-service event freshness, full-volume reconciliation, long-export live
interaction and settled device I/O remain separate acceptance work. Secondary
sort/posting arrays are still owned in memory, and changed secondary shards are
serialized to identify reuse. Relevant-record-only reconciliation retries and
full event-scope refinement also remain open. This release is a measured
checkpoint, not completion of the broader Everything feature or resource plan.

## Parser safety follow-up

Release validation exposed an existing unoptimized Intel stack overflow when
rejecting deeply nested expressions. The preceding parser reproduced it; the
shared-path change did not modify that parser. A recursion-depth limit alone
does not bound stack bytes across architectures and optimization levels.

Parsing now uses one loop and owned suspended expression frames. Groups and
macros transfer token iterators instead of cloning token strings. Simple queries
need no suspended-frame allocation. The existing 128-level nesting, 16-level
macro and 8,192-expression work limits remain intact, including error behavior
and Everything's OR-before-AND precedence. Matching and index layout are unchanged.

The frozen recursive parser is retained only as a test oracle. More than 4,000
generated valid/malformed expressions compare full syntax trees and errors.
Both ARM64 and Intel debug suites pass **316 tests** without a stack override;
the new regression deliberately uses a **256 KiB** thread stack and checks deep
groups, negations, macro limits and sibling scopes. Strict Clippy also passes.

CI separately exposed a timing-dependent cache-publication test: it polled for
a short-lived temporary file to guess when to commit a concurrent update. The
test now uses synchronous channel handshakes before and after publication, with
the real cache writer and SQLite checkpoint path. It verifies that the newer
delta remains dirty and replays correctly, with a tiny fixture and no timing
assumption. Production publication behavior is unchanged.

The five-round layout measurements above precede this parser follow-up. Use the
opt-in `query::parser_tests::parser_latency_profile` to compare parsing alone;
it alternates 40 batches of 200 parses per implementation and reports raw batch
averages, not individual-query P95. Final signed-window verification and package
checks are recorded with the [0.1.4 release](https://github.com/JamesLinYJ/APFSearch/releases/tag/v0.1.4).

In the release-mode parser-only run, median batch-average time changed as follows
(nanoseconds per parse). These do not imply the same percentage change in total
file-search latency; query matching and native transport have their own costs.

| Parser case | Recursive reference | Explicit frames |
| --- | ---: | ---: |
| Empty input | 6.0 | 8.1 |
| Filename | 270.3 | 217.6 |
| Chinese filename | 298.0 | 259.1 |
| Ordinary path | 435.8 | 382.7 |
| Size condition | 440.2 | 395.4 |
| Date condition | 667.0 | 641.9 |
| Extension | 352.4 | 322.6 |
| Boolean combination | 713.1 | 662.5 |
| Scoped macro | 1,415.6 | 1,328.4 |
| Full-path regex | 4,577.2 | 4,544.2 |
| Nested groups | 916.8 | 886.9 |
| Repeated negation | 411.5 | 361.0 |
