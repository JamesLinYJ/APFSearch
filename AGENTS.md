# Contributor guidance

APFSearch (All-Purpose File Search) is a macOS file search application under active development. Target macOS 14 or later on Apple Silicon and Intel. Use this file as the shared guidance for coding agents; `CLAUDE.md` is a relative symlink to it.

## Architecture

- `core/`: idiomatic Rust, Edition 2024, for APFS metadata traversal, FSEvents, SQLite persistence, immutable search snapshots, query evaluation, and the C ABI. Keep `unsafe` small, document its invariants, and encapsulate it behind safe APIs.
- `macos/`: Swift with AppKit for the main interface, SwiftUI for settings, and a versioned XPC interface shared by the app and CLI. Keep search semantics in the shared service/core.
- `Resources/`: String Catalogs with stable semantic keys. English is the development and fallback language; preserve native Bundle localization and regional formatting.
- `tests/`: isolated fixtures and integration harnesses. Separate fixture results from installed-app, foreground UI, full-volume, and performance validation.
- `scripts/` and `build.sh`: universal ARM64/Intel builds, bundle metadata, localization maintenance, and builds. `ApplicationIdentity.swift` is the source of truth for neutral application identifiers.

## Implementation standards

Prefer root-cause fixes and measured algorithms. Preserve query snapshot consistency, every hard-link directory entry, and explicit coverage reporting. Do not follow directory symlinks during traversal. SQLite is authoritative; derived caches must be safe to discard and rebuild. Commit event progress with the reconciled metadata it describes.

Use descriptive English identifiers and neutral package names. Product branding is APFSearch; internal modules use APFSearch or names that describe their responsibility. Do not embed a developer's name, home directory, signing certificate, team identifier, or machine configuration.

Use native controls with deliberate layout and interaction. Preserve selection, scroll position, input focus, and window size during background updates. Respect Reduce Motion. Avoid periodic table replacement, unrequested resizing, and localization implemented by replacing text at runtime.

Keep disk activity bounded. Coalesce filesystem events, batch writes, and exclude the index's own output from feedback loops. Do not use repeated full-disk scans or generate million-file fixtures as routine checks. Content extraction and hashing must be cancellable and must not download cloud placeholders implicitly. Never use root scanning, kernel extensions, or disabled SIP as a shortcut.

## Validation

Run checks appropriate to the changed layer from the repository root:

```sh
cargo fmt --manifest-path core/Cargo.toml --check
PCRE2_SYS_STATIC=1 MACOSX_DEPLOYMENT_TARGET=14.0 cargo test --locked --manifest-path core/Cargo.toml
PCRE2_SYS_STATIC=1 MACOSX_DEPLOYMENT_TARGET=14.0 cargo clippy --locked --manifest-path core/Cargo.toml --all-targets -- -D warnings
python3 tests/check_localization.py
python3 tests/check_localization_protocol.py
python3 tests/check_application_identity.py
APFSEARCH_COMPILE_ONLY=1 ./build.sh
```

`tests/run.sh` exercises Rust and Swift service/content/file fixtures. `tests/run_search_window_tests.py` exercises AppKit behavior; visible-window checks require a usable WindowServer. Signed XPC integration requires an explicitly configured signing identity. Check each harness's arguments before running it; do not point destructive fixtures at user data or mutate the installed app for a routine test.

Do not weaken XPC client validation to bypass a signing problem. Keep release signing configuration external to the repository. Report what actually passed and what remains unverified. A successful build or fixture benchmark is not proof of complete Everything compatibility or whole-machine performance.

## Privacy and repository hygiene

Commit source, reusable tests, documentation, and required dependency notices only. Keep filesystem indexes, databases, query history, runtime captures, personal file lists, signing material, local configuration, generated binaries, and machine-specific validation reports out of Git. Use synthetic paths and fixtures in examples. Audit the actual staged tree and commit metadata before publishing; use a GitHub noreply address for commits when needed.

Preserve the MIT license and third-party notices. Do not publish credentials or remove authentication checks in the name of portability. Do not overwrite user data or force-push a shared branch without explicit authorization.

## Delivery status

APFSearch uses stable releases starting at 1.0.0; it is not a completed Everything clone. Keep the README's limitations accurate. Document any behavior change, relevant validation, and remaining risks without claiming unmeasured results.
