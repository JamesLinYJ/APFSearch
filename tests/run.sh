#!/bin/bash
set -euo pipefail
APFSEARCH_PROJECT="$(cd "$(dirname "$0")/.." && pwd)"
APFSEARCH_TEST_WORK="${APFSEARCH_TEST_DIR:-$(mktemp -d /private/tmp/APFSearch-tests.XXXXXX)}"
export PCRE2_SYS_STATIC=1
MACOSX_DEPLOYMENT_TARGET="$(python3 "$APFSEARCH_PROJECT/scripts/build_configuration.py" minimum_macos_version)"
export MACOSX_DEPLOYMENT_TARGET
export CARGO_ENCODED_RUSTFLAGS="$(python3 "$APFSEARCH_PROJECT/scripts/build_configuration.py" rust_flags)"
APFSEARCH_SWIFT_TARGET="$(python3 "$APFSEARCH_PROJECT/scripts/build_configuration.py" swift_target)"
mkdir -p "$APFSEARCH_TEST_WORK" "$APFSEARCH_PROJECT/validation"
if [[ "${APFSEARCH_SWIFT_ONLY:-0}" != "1" ]]; then
  cargo test --locked --manifest-path "$APFSEARCH_PROJECT/core/Cargo.toml" -- --test-threads=1
fi
cargo build --locked --release --manifest-path "$APFSEARCH_PROJECT/core/Cargo.toml"
APFSEARCH_SOURCES=("$APFSEARCH_PROJECT/macos/ApplicationIdentity.swift" "$APFSEARCH_PROJECT/macos/SearchProtocol.swift" "$APFSEARCH_PROJECT/macos/Localization.swift" "$APFSEARCH_PROJECT/macos/SearchService.swift" "$APFSEARCH_PROJECT/macos/ContentIndexer.swift" "$APFSEARCH_PROJECT/macos/FileOperations.swift")
APFSEARCH_LINK=("$APFSEARCH_PROJECT/core/target/release/libapfsearch_core.a" -framework AppKit -framework PDFKit -framework AVFoundation -framework ImageIO -framework Security -framework DiskArbitration -framework CoreServices -framework CoreFoundation -lc++)
for APFSEARCH_TEST_NAME in ContentAndFileTests ServiceTests; do
  swiftc -module-cache-path "$APFSEARCH_TEST_WORK/ModuleCache" -D TEST_BUILD -swift-version 5 -O -target "$APFSEARCH_SWIFT_TARGET" "${APFSEARCH_SOURCES[@]}" "$APFSEARCH_PROJECT/tests/$APFSEARCH_TEST_NAME.swift" "${APFSEARCH_LINK[@]}" -o "$APFSEARCH_TEST_WORK/$APFSEARCH_TEST_NAME"
  python3 "$APFSEARCH_PROJECT/tests/test_bundle.py" "$APFSEARCH_TEST_WORK/$APFSEARCH_TEST_NAME" "$APFSEARCH_TEST_WORK/$APFSEARCH_TEST_NAME.app"
done
"$APFSEARCH_TEST_WORK/ContentAndFileTests.app/Contents/MacOS/ContentAndFileTests" "$APFSEARCH_TEST_WORK/content-file-fixture" -AppleLanguages '(zh-Hans)' > "$APFSEARCH_PROJECT/validation/content-and-files.json"
"$APFSEARCH_TEST_WORK/ServiceTests.app/Contents/MacOS/ServiceTests" "$APFSEARCH_PROJECT" "$APFSEARCH_TEST_WORK/content-file-fixture" "$APFSEARCH_TEST_WORK/service" -AppleLanguages '(zh-Hans)' > "$APFSEARCH_TEST_WORK/service-output.json"
python3 "$APFSEARCH_PROJECT/tests/check_localization.py" --report "$APFSEARCH_PROJECT/validation/localization.json"
python3 "$APFSEARCH_PROJECT/tests/check_localization_protocol.py"
printf 'Tests passed. Fixtures and executables: %s\n' "$APFSEARCH_TEST_WORK"
