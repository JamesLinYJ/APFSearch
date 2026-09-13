# APFSearch

APFSearch (All-Purpose File Search) is a local file search application for macOS 15 and later on Apple Silicon. Its AppKit interface and command-line client share a versioned XPC service backed by a Rust indexing and query engine. Filename indexing is independent of Spotlight.

This is a development version. It is not a complete Everything replacement and is not affiliated with voidtools. Full-volume correctness, installed-app latency, advanced query compatibility, and signed distribution still need further validation.

## Architecture

| Component | Responsibility |
| --- | --- |
| Swift / AppKit | Search window, result table, native menus, Quick Look, and file actions |
| SwiftUI | Preferences, filters, bookmarks, and indexing scope |
| Rust | APFS traversal, event reconciliation, query parsing, filtering, sorting, and immutable snapshots |
| SQLite WAL | Authoritative metadata, preferences, content, and event progress |
| Derived cache | Rebuildable prepared search columns, postings, and ordering data |

APFS enumeration uses `getattrlistbulk`. FSEvents starts before the initial traversal; events trigger reconciliation with the current filesystem. Hard links retain their individual directory entries, directory symlinks are not followed, and inaccessible locations are reported as uncovered. Indexing does not require root, a kernel extension, or disabled SIP.

Search uses substring candidates, Roaring bitmaps, numeric columns, Unicode normalization and case folding, and PCRE2 regular expressions. Each query binds to a snapshot generation. Content extraction and hashing run separately from filename search and support cancellation.

See [the core interface](core/README.md) and [localization conventions](docs/LOCALIZATION.md).

## Build

Install a Rust stable toolchain and select an Xcode toolchain containing the macOS SDK, Swift compiler, and `xcstringstool`. The build targets ARM64 and macOS 15. SQLite and PCRE2 are linked statically; Cargo resolves the pinned dependencies in `core/Cargo.lock`.

To compile without signing or installing:

```sh
FILESEARCH_COMPILE_ONLY=1 ./build.sh
```

The default output is `/private/tmp/FileSearch-build/APFSearch.app`. Set `FILESEARCH_BUILD_DIR` to choose another build directory. Without a signing identity, the build produces a compilation artifact; it does not validate the authenticated XPC connection used by the installed application.

For a signed build, supply your own Apple Developer signing identity:

```sh
FILESEARCH_SIGN_IDENTITY='Developer ID Application: Your Name (TEAMID)' ./build.sh
```

Signing configuration is not included in this repository. The app, CLI, and service must be signed consistently. The service derives its team from its own validated Apple signature, then accepts only the designated client identifiers from that team. Unsigned or invalidly signed services refuse production XPC connections.

The neutral bundle identifiers are `local.filesearch.app`, `local.filesearch.indexer`, and `local.filesearch.cli`. The Rust package is `filesearch-core`.

## Use

After building and signing, copy the app to `/Applications` and open it. The application registers a user-level indexer with `SMAppService`. macOS may require approval under System Settings → General → Login Items. Choose the directories to index; grant Full Disk Access through System Settings if coverage of protected directories is needed.

Example queries:

```text
invoice
ext:pdf;docx
size:>10mb
dm:today
path:Documents
"annual report" | invoice
regex:^report[0-9]+
```

Space combines terms with AND, `|` means OR, and `!` excludes a term. Space while a result is selected opens Quick Look; Return opens the selected file. Context menus provide reveal, copy path, rename, copy, move, and trash actions.

The CLI uses the same XPC interface:

```sh
/Applications/APFSearch.app/Contents/MacOS/filesearch-cli status
/Applications/APFSearch.app/Contents/MacOS/filesearch-cli search 'ext:pdf size:>1mb'
```

`content:` can extract candidate text from supported text/code, PDF, and Office Open XML files. Image and media properties use system frameworks. Unsupported formats, inaccessible files, and cloud placeholders are reported; on-demand search does not implicitly download placeholders. File lists can be imported for offline queries or exported in pages.

English, Simplified Chinese, and Traditional Chinese use native String Catalogs and Foundation language selection. English is the fallback language. Number, date, and unit formatting follows the user's region settings.

## Tests

```sh
cargo fmt --manifest-path core/Cargo.toml --check
PCRE2_SYS_STATIC=1 MACOSX_DEPLOYMENT_TARGET=15.0 cargo test --locked --manifest-path core/Cargo.toml
./tests/run.sh
python3 tests/run_search_window_tests.py --report validation/search-window.json
```

Tests use isolated fixtures. GUI tests require WindowServer; controller-level checks do not establish foreground animation or input-method behavior. Tests and benchmarks can produce local validation reports containing paths, so reports are excluded from Git. Large synthetic and live-index benchmarks are opt-in and should not be run as routine checks.

## Current limitations

- Everything 1.5 syntax, property functions, duplicate handling, and advanced bulk operations are not fully equivalent. Unsupported functions should report errors.
- The prepared-cache delta journal is bounded. Overflow, incompatible caches, or invalid history can require SQLite recovery and a slower startup.
- Incremental publication retains shared data, but some snapshot and ordering work still scales with index size. Whole-machine latency and disk activity need continuing measurement.
- Cross-page bulk file operations are incomplete. Unresolved selections must not silently cause partial operations.
- APFS clone relationships are not established by content equality. Hard links and independently stored identical content are reported separately where known.
- Full authorized-scope enumeration parity, crash/event-loss recovery under real workloads, and final installed-app UI behavior are not fully accepted. The repository contains no claim that all original performance targets have passed.
- Builds are not notarized releases. Signing and distribution remain the builder's responsibility.

Runtime data is stored under `~/Library/Application Support/FileSearch`. It is not part of the repository. Unregister the background service and disable login launch before removing an installation; retain the data directory if you want to preserve its index and settings.

## Contributing

Read [AGENTS.md](AGENTS.md) for architecture, validation, privacy, and implementation guidance. `CLAUDE.md` links to the same file. Use synthetic examples and keep local indexes, runtime logs, file lists, and signing material out of commits.

Licensed under MIT. See [LICENSE](LICENSE) and [THIRD_PARTY_NOTICES.txt](THIRD_PARTY_NOTICES.txt).
