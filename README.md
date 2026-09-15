<p align="center">
  <img src="Resources/Brand/AppIcon.svg" width="144" height="144" alt="APFSearch icon">
</p>
<h1 align="center">APFSearch</h1>
<p align="center"><strong>Local file search, built for macOS.</strong><br>All-Purpose File Search</p>
<p align="center"><code>macOS 14+</code> &nbsp; <code>Apple Silicon + Intel</code> &nbsp; <code>Swift + Rust</code> &nbsp; <a href="LICENSE">MIT</a></p>
<p align="center"><strong>English</strong> · <a href="README.zh-CN.md">简体中文</a></p>
<p align="center"><a href="#features">Features</a> · <a href="#download">Download</a> · <a href="#getting-started">Getting started</a> · <a href="#search">Search</a> · <a href="#documentation">Documentation</a></p>

---

APFSearch brings filename, path, and metadata search to an AppKit interface with a shared command-line client. It builds its own APFS-aware index independently of Spotlight.

> **In development.** This is not a complete Everything replacement and is not affiliated with voidtools. Full-volume correctness, real-world latency, advanced query compatibility, and minimum-OS hardware compatibility still need validation. See [current limitations](#current-limitations).

## Features

| Find | Work |
| :--- | :--- |
| **Flexible queries**<br>Combine names, paths, extensions, sizes, dates, Boolean expressions, and regular expressions. | **Familiar controls**<br>Native result tables, tabs, keyboard shortcuts, Quick Look, and Finder actions. |
| **Independent indexing**<br>APFS enumeration, incremental file-change monitoring, and visible coverage gaps. | **Reviewable file actions**<br>Rename, copy, move, and trash files with conflict previews and operation history. |
| **Beyond filenames**<br>On-demand content extraction, image and media properties, duplicate checks, and offline file lists. | **Your language**<br>English, Simplified and Traditional Chinese, Japanese, Korean, Russian, Spanish, and Portuguese through native String Catalogs. |

## Download

**[v0.1.4 pre-release](https://github.com/JamesLinYJ/APFSearch/releases/tag/v0.1.4)** · macOS 14 or later

| Your Mac | Download |
| :--- | :--- |
| Universal (both architectures) | [DMG](https://github.com/JamesLinYJ/APFSearch/releases/download/v0.1.4/APFSearch-0.1.4-Universal.dmg) · [ZIP](https://github.com/JamesLinYJ/APFSearch/releases/download/v0.1.4/APFSearch-0.1.4-Universal.zip) |
| Apple Silicon (M series) | [DMG](https://github.com/JamesLinYJ/APFSearch/releases/download/v0.1.4/APFSearch-0.1.4-AppleSilicon.dmg) · [ZIP](https://github.com/JamesLinYJ/APFSearch/releases/download/v0.1.4/APFSearch-0.1.4-AppleSilicon.zip) |
| Intel Mac | [DMG](https://github.com/JamesLinYJ/APFSearch/releases/download/v0.1.4/APFSearch-0.1.4-Intel.dmg) · [ZIP](https://github.com/JamesLinYJ/APFSearch/releases/download/v0.1.4/APFSearch-0.1.4-Intel.zip) |

Choose Universal if you are unsure. Open the DMG and drag APFSearch into Applications, or use the ZIP for manual deployment. Every contained app is Developer ID signed and carries an Apple notarization ticket; the DMG containers are not separately notarized. [SHA-256 checksums](https://github.com/JamesLinYJ/APFSearch/releases/download/v0.1.4/SHA256SUMS.txt).

This is a development preview. Intel code has been exercised under Rosetta; physical Intel hardware and macOS 14 still need validation.

The 0.1.4 Preview reduces index memory with shared path prefixes and compact integer columns. A five-round, 5.69-million-entry comparison used 49.5% less resident memory than the preceding compact candidate; this is not a comparison with 0.1.3 or a whole-machine guarantee. See the [measurements and remaining limits](docs/SHARED_PATH_LAYOUT.md#acceptance-results). Existing SQLite data and settings are retained; the derived search cache is regenerated once without rescanning files.

## Getting started

### Build from source

Build from source with a **stable Rust toolchain** and **Xcode** containing the macOS SDK, Swift compiler, and `xcstringstool`. The default build is universal (`arm64` + `x86_64`) for macOS 14 or later.

```sh
# Install both macOS compilation targets once.
rustup target add aarch64-apple-darwin x86_64-apple-darwin

# Compile the app, service, and CLI without signing or installing.
APFSEARCH_COMPILE_ONLY=1 ./build.sh
```

Output: `APFSearch-build/APFSearch.app` under the current user's system temporary directory; the build prints the full path. Set `APFSEARCH_BUILD_DIR` to use another directory. Build and release paths must belong to the current user, with trusted ancestors and no writable-by-others output entries or symlinks. Unsafe existing paths are refused; compiler caches remain reusable. SQLite and PCRE2 are linked statically; Cargo uses the pinned dependencies in `core/Cargo.lock`.

The app, indexer, and CLI each contain both architectures. For a faster local development build, set `APFSEARCH_ARCHITECTURES=arm64` or `APFSEARCH_ARCHITECTURES=x86_64`. The deployment target is shared by the Rust build, Swift compiler, bundle metadata, and test bundles through `scripts/build_configuration.py`. Select a particular Xcode with `DEVELOPER_DIR` if you have multiple installations.

**To run the authenticated service**, build with your own Apple Developer signing identity:

```sh
APFSEARCH_SIGN_IDENTITY='Developer ID Application: Your Name (TEAMID)' ./build.sh
```

The app, CLI, and service must use the same trusted team signature. Unsigned or ad-hoc builds are compilation artifacts; they cannot use the production XPC service. Signing configuration is kept outside the repository.

1. Copy the signed `APFSearch.app` to `/Applications` and open it.
2. Choose the folders or local APFS volumes to index.
3. If prompted, allow background activity in **System Settings → General → Login Items**. Grant **Full Disk Access** only when your chosen scope needs it.

Indexing does not require root, a kernel extension, or disabled SIP. Cloud placeholders are not downloaded automatically. English is the fallback UI language; number and date formatting follows your region independently.

## Search

| Query | Finds |
| :--- | :--- |
| `invoice` | Names containing “invoice” |
| `ext:pdf;docx` | PDF and Word files |
| `size:>10mb dm:today` | Files larger than 10 MB modified today |
| `path:Documents` | Matches in the file path |
| `"annual report" \| invoice` | Either the phrase or “invoice” |
| `regex:^report[0-9]+` | Regular-expression matches |
| `content:"keyword"` | Text in supported file contents; may take longer |

Spaces combine terms with **AND**, `|` means **OR**, and `!` excludes a term. Content extraction supports text/code, PDF, and Office Open XML. Unsupported formats and unreadable files are reported explicitly.

| Shortcut | Action |
| :--- | :--- |
| `Space` on a result | Quick Look |
| `Return` | Open |
| `⌘⇧C` | Copy path |
| `⌘D` | Bookmark the current search |
| `⇧` + column click | Add a sort column |

The CLI uses the same search service:

```sh
/Applications/APFSearch.app/Contents/MacOS/apfsearch-cli status
/Applications/APFSearch.app/Contents/MacOS/apfsearch-cli search 'ext:pdf size:>1mb'
```

<details>
<summary><strong>How the index works</strong></summary>

| Layer | Responsibility |
| :--- | :--- |
| Swift / AppKit | Search window, result table, menus, Quick Look, and file actions |
| SwiftUI | Preferences, filters, bookmarks, and index scope |
| Rust | APFS traversal, event reconciliation, query parsing, filtering, and sorting |
| SQLite WAL | Authoritative metadata, preferences, content, and event progress |
| Derived cache | Rebuildable search columns, postings, ordering, and immutable snapshots |

APFS enumeration uses `getattrlistbulk`. FSEvents starts before the first traversal and triggers reconciliation against the filesystem. Hard links retain every directory entry; directory symlinks are not followed. Inaccessible locations appear in the coverage report.

Substring candidates, Roaring bitmaps, numeric columns, Unicode normalization and case folding, and PCRE2 support query evaluation. Each query binds to one snapshot generation. Content extraction and hashing run separately and support cancellation.

Bundle identifiers are `org.apfsearch.app`, `org.apfsearch.indexer`, and `org.apfsearch.cli`; the Rust package is `apfsearch-core`.

This preview starts with format-1 preferences and a fresh index in `~/Library/Application Support/APFSearch/v1`. Earlier development settings and indexes are neither imported, migrated, nor deleted.

</details>

## Current limitations

- **Compatibility:** Everything 1.5 syntax, property functions, duplicate handling, and advanced bulk operations are not fully equivalent. Unsupported functions should report errors.
- **Startup and performance:** Cache-journal overflow or invalid history can require SQLite recovery and a slower startup. Some snapshot and ordering work still scales with index size; whole-machine latency and disk activity need continuing measurement.
- **File actions:** A batch is limited to 100,000 entries. Quick Look and drag initiation require loaded rows. Unresolved selections never silently turn into partial file operations.
- **Duplicates:** Identical contents do not establish an APFS clone relationship or reclaimable space. Hard links and independently stored identical files are distinguished where known.
- **Acceptance:** Full authorized-scope enumeration parity, crash/event-loss recovery under real workloads, and final installed-app UI behavior are not fully accepted. The original performance targets have not all been verified. Compiling for macOS 14 and running Intel code under Rosetta do not replace testing on a physical Intel Mac or macOS 14.
- **Distribution:** The default source build is for compilation checks. Distribution requires signing and notarizing the actual deliverable; see the [distribution guide](docs/DISTRIBUTION.md).

Runtime data lives in `~/Library/Application Support/APFSearch/v1`. Before uninstalling, unregister the background service and disable login launch. Keep the data directory if you want to preserve the index and settings.

## Documentation

| Guide | Contents |
| :--- | :--- |
| [Core interface](core/README.md) | Search engine requests and data model |
| [Incremental indexing](docs/INCREMENTAL_INDEX.md) | Cache recovery and update publication |
| [Workflow details](docs/WORKFLOWS.md) | File-operation review, query leases, and update lifecycle |
| [Localization](docs/LOCALIZATION.md) | Stable keys, native language selection, and checks |
| [Artwork](Resources/Brand/README.md) | Editable SVG icon, monochrome mark, and ICNS generation |
| [Distribution](docs/DISTRIBUTION.md) | Xcode account upload, notarization, and deliverable verification |
| [Contributor guidance](AGENTS.md) | Architecture, validation, privacy, and coding standards |

<details>
<summary><strong>Run validation</strong></summary>

```sh
cargo fmt --manifest-path core/Cargo.toml --check
PCRE2_SYS_STATIC=1 MACOSX_DEPLOYMENT_TARGET=14.0 cargo test --locked --manifest-path core/Cargo.toml
./tests/run.sh
python3 tests/run_search_window_tests.py --report validation/search-window.json
python3 tests/run_feature_tests.py
python3 tests/check_localization.py
```

Tests use isolated fixtures. GUI tests require WindowServer; controller tests do not establish foreground animation or input-method behavior. Large synthetic and live-index benchmarks are opt-in. Local validation reports may contain paths and are excluded from Git.

CI builds and tests committed source directly, without source-generating patches or automatic source commits. Keep local indexes, logs, file lists, and signing material out of commits. `CLAUDE.md` links to `AGENTS.md`.

</details>

## License

Original code and artwork use the [MIT License](LICENSE). Required copyright and permission notices must be retained; the software is provided without warranty. Dependencies retain their own licenses, listed in [THIRD_PARTY_NOTICES.txt](THIRD_PARTY_NOTICES.txt).
