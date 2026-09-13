#!/bin/bash
set -euo pipefail
FILESEARCH_ROOT="$(cd "$(dirname "$0")" && pwd)"
FILESEARCH_BUILD="${FILESEARCH_BUILD_DIR:-/private/tmp/FileSearch-build}"
FILESEARCH_EXECUTABLE="$(python3 "$FILESEARCH_ROOT/scripts/build_identity.py" applicationExecutable)"
FILESEARCH_SERVICE_EXECUTABLE="$(python3 "$FILESEARCH_ROOT/scripts/build_identity.py" serviceExecutable)"
FILESEARCH_CLI_EXECUTABLE="$(python3 "$FILESEARCH_ROOT/scripts/build_identity.py" cliExecutable)"
FILESEARCH_SERVICE_IDENTIFIER="$(python3 "$FILESEARCH_ROOT/scripts/build_identity.py" serviceIdentifier)"
FILESEARCH_CLI_IDENTIFIER="$(python3 "$FILESEARCH_ROOT/scripts/build_identity.py" cliIdentifier)"
FILESEARCH_APP="$FILESEARCH_BUILD/$FILESEARCH_EXECUTABLE.app"
FILESEARCH_IDENTITY="${FILESEARCH_SIGN_IDENTITY:-}"
if [[ "$FILESEARCH_IDENTITY" == "-" && "${FILESEARCH_COMPILE_ONLY:-0}" != "1" ]]; then
  printf 'Ad-hoc signing cannot authenticate the XPC service. Use FILESEARCH_COMPILE_ONLY=1 or a trusted Apple signing identity.\n' >&2
  exit 1
fi
mkdir -p "$FILESEARCH_APP/Contents/MacOS" "$FILESEARCH_APP/Contents/Resources" "$FILESEARCH_APP/Contents/Library/LaunchAgents"
export PCRE2_SYS_STATIC=1
export MACOSX_DEPLOYMENT_TARGET=15.0
cargo build --locked --manifest-path "$FILESEARCH_ROOT/core/Cargo.toml" --release
mkdir -p "$FILESEARCH_BUILD/ModuleCache"
FILESEARCH_FLAGS=(-module-cache-path "$FILESEARCH_BUILD/ModuleCache" -swift-version 5 -O -target arm64-apple-macos15.0)
FILESEARCH_LIB="$FILESEARCH_ROOT/core/target/release/libfilesearch_core.a"
swiftc "${FILESEARCH_FLAGS[@]}" "$FILESEARCH_ROOT/macos/ApplicationIdentity.swift" "$FILESEARCH_ROOT/macos/LegacyDataMigration.swift" "$FILESEARCH_ROOT/macos/SearchProtocol.swift" "$FILESEARCH_ROOT/macos/Localization.swift" "$FILESEARCH_ROOT/macos/SearchService.swift" "$FILESEARCH_ROOT/macos/ContentIndexer.swift" "$FILESEARCH_ROOT/macos/FileOperations.swift" "$FILESEARCH_LIB" -framework AppKit -framework PDFKit -framework AVFoundation -framework ImageIO -framework Security -framework DiskArbitration -framework CoreServices -framework CoreFoundation -lc++ -o "$FILESEARCH_APP/Contents/MacOS/$FILESEARCH_SERVICE_EXECUTABLE"
swiftc "${FILESEARCH_FLAGS[@]}" "$FILESEARCH_ROOT/macos/ApplicationIdentity.swift" "$FILESEARCH_ROOT/macos/LegacyDataMigration.swift" "$FILESEARCH_ROOT/macos/SearchProtocol.swift" "$FILESEARCH_ROOT/macos/Localization.swift" "$FILESEARCH_ROOT/macos/SearchClient.swift" "$FILESEARCH_ROOT/macos/CLI.swift" -framework ServiceManagement -o "$FILESEARCH_APP/Contents/MacOS/$FILESEARCH_CLI_EXECUTABLE"
swiftc "${FILESEARCH_FLAGS[@]}" "$FILESEARCH_ROOT/macos/ApplicationIdentity.swift" "$FILESEARCH_ROOT/macos/LegacyDataMigration.swift" "$FILESEARCH_ROOT/macos/SearchProtocol.swift" "$FILESEARCH_ROOT/macos/Localization.swift" "$FILESEARCH_ROOT/macos/SearchClient.swift" "$FILESEARCH_ROOT/macos/UpdateManager.swift" "$FILESEARCH_ROOT/macos/UpdateUI.swift" "$FILESEARCH_ROOT/macos/SelectionResolver.swift" "$FILESEARCH_ROOT/macos/FileOperationReview.swift" "$FILESEARCH_ROOT/macos/Application.swift" "$FILESEARCH_ROOT/macos/DuplicateResultsWindow.swift" "$FILESEARCH_ROOT/macos/SettingsWindow.swift" -framework AppKit -framework SwiftUI -framework ServiceManagement -framework Quartz -framework Carbon -framework CryptoKit -o "$FILESEARCH_APP/Contents/MacOS/$FILESEARCH_EXECUTABLE"
python3 "$FILESEARCH_ROOT/scripts/build_identity.py" --bundle "$FILESEARCH_APP"
xcrun xcstringstool compile "$FILESEARCH_ROOT/Resources/Localizable.xcstrings" --output-directory "$FILESEARCH_APP/Contents/Resources"
xcrun xcstringstool compile "$FILESEARCH_ROOT/Resources/Features.xcstrings" --output-directory "$FILESEARCH_APP/Contents/Resources"
xcrun xcstringstool compile "$FILESEARCH_ROOT/Resources/InfoPlist.xcstrings" --output-directory "$FILESEARCH_APP/Contents/Resources"
cp "$FILESEARCH_ROOT/THIRD_PARTY_NOTICES.txt" "$FILESEARCH_APP/Contents/Resources/ThirdPartyNotices.txt"
python3 "$FILESEARCH_ROOT/scripts/build_update_config.py" "$FILESEARCH_APP"
xattr -cr "$FILESEARCH_APP"
if [[ "${FILESEARCH_COMPILE_ONLY:-0}" == "1" || -z "$FILESEARCH_IDENTITY" ]]; then
  printf 'Compilation complete; no distribution signature, not installed: %s\n' "$FILESEARCH_APP"
  printf 'The XPC service rejects unsigned/ad-hoc builds. Configure FILESEARCH_SIGN_IDENTITY for signed integration.\n'
  exit 0
fi
codesign --force --options runtime --timestamp --identifier "$FILESEARCH_SERVICE_IDENTIFIER" --sign "$FILESEARCH_IDENTITY" "$FILESEARCH_APP/Contents/MacOS/$FILESEARCH_SERVICE_EXECUTABLE"
codesign --force --options runtime --timestamp --identifier "$FILESEARCH_CLI_IDENTIFIER" --sign "$FILESEARCH_IDENTITY" "$FILESEARCH_APP/Contents/MacOS/$FILESEARCH_CLI_EXECUTABLE"
codesign --force --options runtime --timestamp --sign "$FILESEARCH_IDENTITY" "$FILESEARCH_APP"
codesign --verify --deep --strict --verbose=2 "$FILESEARCH_APP"
python3 "$FILESEARCH_ROOT/scripts/build_identity.py" --verify-signatures "$FILESEARCH_APP"
printf '%s\n' "$FILESEARCH_APP"
