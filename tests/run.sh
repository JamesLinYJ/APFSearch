#!/bin/bash
set -euo pipefail
FILESEARCH_PROJECT="$(cd "$(dirname "$0")/.." && pwd)"
FILESEARCH_TEST_WORK="${FILESEARCH_TEST_DIR:-$(mktemp -d /private/tmp/FileSearch-tests.XXXXXX)}"
export PCRE2_SYS_STATIC=1
export MACOSX_DEPLOYMENT_TARGET=15.0
mkdir -p "$FILESEARCH_TEST_WORK" "$FILESEARCH_PROJECT/validation"
cargo test --locked --manifest-path "$FILESEARCH_PROJECT/core/Cargo.toml"
cargo build --locked --release --manifest-path "$FILESEARCH_PROJECT/core/Cargo.toml"
FILESEARCH_SOURCES=("$FILESEARCH_PROJECT/macos/ApplicationIdentity.swift" "$FILESEARCH_PROJECT/macos/LegacyDataMigration.swift" "$FILESEARCH_PROJECT/macos/SearchProtocol.swift" "$FILESEARCH_PROJECT/macos/Localization.swift" "$FILESEARCH_PROJECT/macos/SearchService.swift" "$FILESEARCH_PROJECT/macos/ContentIndexer.swift" "$FILESEARCH_PROJECT/macos/FileOperations.swift")
FILESEARCH_LINK=("$FILESEARCH_PROJECT/core/target/release/libfilesearch_core.a" -framework AppKit -framework PDFKit -framework AVFoundation -framework ImageIO -framework Security -framework DiskArbitration -framework CoreServices -framework CoreFoundation -lc++)
for FILESEARCH_TEST_NAME in ContentAndFileTests ServiceTests; do
  swiftc -module-cache-path "$FILESEARCH_TEST_WORK/ModuleCache" -D TEST_BUILD -swift-version 5 -O -target arm64-apple-macos15.0 "${FILESEARCH_SOURCES[@]}" "$FILESEARCH_PROJECT/tests/$FILESEARCH_TEST_NAME.swift" "${FILESEARCH_LINK[@]}" -o "$FILESEARCH_TEST_WORK/$FILESEARCH_TEST_NAME"
  python3 "$FILESEARCH_PROJECT/tests/test_bundle.py" "$FILESEARCH_TEST_WORK/$FILESEARCH_TEST_NAME" "$FILESEARCH_TEST_WORK/$FILESEARCH_TEST_NAME.app"
done
"$FILESEARCH_TEST_WORK/ContentAndFileTests.app/Contents/MacOS/ContentAndFileTests" "$FILESEARCH_TEST_WORK/content-file-fixture" -AppleLanguages '(zh-Hans)' > "$FILESEARCH_PROJECT/validation/content-and-files.json"
"$FILESEARCH_TEST_WORK/ServiceTests.app/Contents/MacOS/ServiceTests" "$FILESEARCH_PROJECT" "$FILESEARCH_TEST_WORK/content-file-fixture" "$FILESEARCH_TEST_WORK/service" -AppleLanguages '(zh-Hans)' > "$FILESEARCH_TEST_WORK/service-output.json"
python3 "$FILESEARCH_PROJECT/tests/check_legacy_migration.py"
python3 "$FILESEARCH_PROJECT/tests/check_localization.py" --report "$FILESEARCH_PROJECT/validation/localization.json"
python3 "$FILESEARCH_PROJECT/tests/check_localization_protocol.py"
printf 'Tests passed. Fixtures and executables: %s\n' "$FILESEARCH_TEST_WORK"
