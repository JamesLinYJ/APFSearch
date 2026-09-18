#!/bin/bash
set -euo pipefail
APFSEARCH_ROOT="$(cd "$(dirname "$0")" && pwd)"
python3 "$APFSEARCH_ROOT/scripts/build_toolchain.py"
# Bind Swift and native dependency builds to the same selected Xcode SDK.
export SDKROOT="$(xcrun --sdk macosx --show-sdk-path)"
APFSEARCH_SWIFTC="$(xcrun --sdk macosx --find swiftc)"
APFSEARCH_BUILD="$(python3 "$APFSEARCH_ROOT/scripts/build_workspace.py" build)"
APFSEARCH_EXECUTABLE="$(python3 "$APFSEARCH_ROOT/scripts/build_identity.py" applicationExecutable)"
APFSEARCH_SERVICE_EXECUTABLE="$(python3 "$APFSEARCH_ROOT/scripts/build_identity.py" serviceExecutable)"
APFSEARCH_CLI_EXECUTABLE="$(python3 "$APFSEARCH_ROOT/scripts/build_identity.py" cliExecutable)"
APFSEARCH_SERVICE_IDENTIFIER="$(python3 "$APFSEARCH_ROOT/scripts/build_identity.py" serviceIdentifier)"
APFSEARCH_CLI_IDENTIFIER="$(python3 "$APFSEARCH_ROOT/scripts/build_identity.py" cliIdentifier)"
APFSEARCH_APP="$APFSEARCH_BUILD/$APFSEARCH_EXECUTABLE.app"
APFSEARCH_IDENTITY="${APFSEARCH_SIGN_IDENTITY:-}"
if [[ "$APFSEARCH_IDENTITY" == "-" && "${APFSEARCH_COMPILE_ONLY:-0}" != "1" ]]; then
  printf 'Ad-hoc signing cannot authenticate the XPC service. Use APFSEARCH_COMPILE_ONLY=1 or a trusted Apple signing identity.\n' >&2
  exit 1
fi
mkdir -p "$APFSEARCH_APP/Contents/MacOS" "$APFSEARCH_APP/Contents/Resources" "$APFSEARCH_APP/Contents/Library/LaunchAgents"
export PCRE2_SYS_STATIC=1
MACOSX_DEPLOYMENT_TARGET="$(python3 "$APFSEARCH_ROOT/scripts/build_configuration.py" minimum_macos_version)"
export MACOSX_DEPLOYMENT_TARGET
export CARGO_ENCODED_RUSTFLAGS="$(python3 "$APFSEARCH_ROOT/scripts/build_configuration.py" rust_flags)"
read -r -a APFSEARCH_ARCHITECTURES <<< "${APFSEARCH_ARCHITECTURES:-arm64 x86_64}"
APFSEARCH_CARGO_TARGET="$(python3 "$APFSEARCH_ROOT/scripts/build_workspace.py" directory "${CARGO_TARGET_DIR:-$APFSEARCH_ROOT/core/target}")"
for APFSEARCH_ARCHITECTURE in "${APFSEARCH_ARCHITECTURES[@]}"; do
  APFSEARCH_RUST_TARGET="$(python3 "$APFSEARCH_ROOT/scripts/build_configuration.py" rust_target "$APFSEARCH_ARCHITECTURE")"
  if [[ ! -d "$(rustc --print target-libdir --target "$APFSEARCH_RUST_TARGET")" ]]; then
    printf 'Install the Rust target before building: rustup target add %s\n' "$APFSEARCH_RUST_TARGET" >&2
    exit 1
  fi
done
mkdir -p "$APFSEARCH_BUILD/ModuleCache"
for APFSEARCH_ARCHITECTURE in "${APFSEARCH_ARCHITECTURES[@]}"; do
  APFSEARCH_RUST_TARGET="$(python3 "$APFSEARCH_ROOT/scripts/build_configuration.py" rust_target "$APFSEARCH_ARCHITECTURE")"
  APFSEARCH_SWIFT_TARGET="$(python3 "$APFSEARCH_ROOT/scripts/build_configuration.py" swift_target "$APFSEARCH_ARCHITECTURE")"
  APFSEARCH_SLICE="$APFSEARCH_BUILD/slices/$APFSEARCH_ARCHITECTURE"
  mkdir -p "$APFSEARCH_SLICE/ModuleCache"
  cargo build --locked --manifest-path "$APFSEARCH_ROOT/core/Cargo.toml" --release --target "$APFSEARCH_RUST_TARGET" --target-dir "$APFSEARCH_CARGO_TARGET"
  APFSEARCH_FLAGS=(-module-cache-path "$APFSEARCH_SLICE/ModuleCache" -swift-version 5 -O -sdk "$SDKROOT" -target "$APFSEARCH_SWIFT_TARGET")
  APFSEARCH_LIB="$APFSEARCH_CARGO_TARGET/$APFSEARCH_RUST_TARGET/release/libapfsearch_core.a"
  "$APFSEARCH_SWIFTC" "${APFSEARCH_FLAGS[@]}" "$APFSEARCH_ROOT/macos/ApplicationIdentity.swift" "$APFSEARCH_ROOT/macos/SearchProtocol.swift" "$APFSEARCH_ROOT/macos/Localization.swift" "$APFSEARCH_ROOT/macos/SearchService.swift" "$APFSEARCH_ROOT/macos/ContentIndexer.swift" "$APFSEARCH_ROOT/macos/FileOperations.swift" "$APFSEARCH_LIB" -framework AppKit -framework PDFKit -framework AVFoundation -framework ImageIO -framework Security -framework DiskArbitration -framework CoreServices -framework CoreFoundation -lc++ -o "$APFSEARCH_SLICE/$APFSEARCH_SERVICE_EXECUTABLE"
  "$APFSEARCH_SWIFTC" "${APFSEARCH_FLAGS[@]}" "$APFSEARCH_ROOT/macos/ApplicationIdentity.swift" "$APFSEARCH_ROOT/macos/SearchProtocol.swift" "$APFSEARCH_ROOT/macos/Localization.swift" "$APFSEARCH_ROOT/macos/SearchClient.swift" "$APFSEARCH_ROOT/macos/CLI.swift" -framework ServiceManagement -o "$APFSEARCH_SLICE/$APFSEARCH_CLI_EXECUTABLE"
  "$APFSEARCH_SWIFTC" "${APFSEARCH_FLAGS[@]}" "$APFSEARCH_ROOT/macos/ApplicationIdentity.swift" "$APFSEARCH_ROOT/macos/SearchProtocol.swift" "$APFSEARCH_ROOT/macos/Localization.swift" "$APFSEARCH_ROOT/macos/SearchClient.swift" "$APFSEARCH_ROOT/macos/UpdateManager.swift" "$APFSEARCH_ROOT/macos/UpdateUI.swift" "$APFSEARCH_ROOT/macos/SelectionResolver.swift" "$APFSEARCH_ROOT/macos/FileOperationReview.swift" "$APFSEARCH_ROOT/macos/ResultIconLoader.swift" "$APFSEARCH_ROOT/macos/Application.swift" "$APFSEARCH_ROOT/macos/DuplicateResultsWindow.swift" "$APFSEARCH_ROOT/macos/SettingsWindow.swift" -framework AppKit -framework SwiftUI -framework ServiceManagement -framework Quartz -framework Carbon -framework CryptoKit -o "$APFSEARCH_SLICE/$APFSEARCH_EXECUTABLE"
done
for APFSEARCH_BINARY in "$APFSEARCH_EXECUTABLE" "$APFSEARCH_SERVICE_EXECUTABLE" "$APFSEARCH_CLI_EXECUTABLE"; do
  APFSEARCH_SLICES=()
  for APFSEARCH_ARCHITECTURE in "${APFSEARCH_ARCHITECTURES[@]}"; do
    APFSEARCH_SLICES+=("$APFSEARCH_BUILD/slices/$APFSEARCH_ARCHITECTURE/$APFSEARCH_BINARY")
  done
  xcrun lipo -create "${APFSEARCH_SLICES[@]}" -output "$APFSEARCH_APP/Contents/MacOS/$APFSEARCH_BINARY"
  # Rust's prebuilt standard library can contribute linker debug-map entries
  # containing the local static-library path. Release executables do not need
  # these entries; strip them before signing, retaining ordinary symbols.
  xcrun strip -S "$APFSEARCH_APP/Contents/MacOS/$APFSEARCH_BINARY"
done
python3 "$APFSEARCH_ROOT/scripts/build_toolchain.py" --bundle "$APFSEARCH_APP"
python3 "$APFSEARCH_ROOT/scripts/build_identity.py" --bundle "$APFSEARCH_APP"
swift -module-cache-path "$APFSEARCH_BUILD/ModuleCache" "$APFSEARCH_ROOT/scripts/build_icon.swift" "$APFSEARCH_ROOT/Resources/Brand/AppIcon.svg" "$APFSEARCH_APP/Contents/Resources/AppIcon.icns"
xcrun xcstringstool compile "$APFSEARCH_ROOT/Resources/Localizable.xcstrings" --output-directory "$APFSEARCH_APP/Contents/Resources"
xcrun xcstringstool compile "$APFSEARCH_ROOT/Resources/Features.xcstrings" --output-directory "$APFSEARCH_APP/Contents/Resources"
xcrun xcstringstool compile "$APFSEARCH_ROOT/Resources/InfoPlist.xcstrings" --output-directory "$APFSEARCH_APP/Contents/Resources"
cp "$APFSEARCH_ROOT/THIRD_PARTY_NOTICES.txt" "$APFSEARCH_APP/Contents/Resources/ThirdPartyNotices.txt"
python3 "$APFSEARCH_ROOT/scripts/build_update_config.py" "$APFSEARCH_APP"
xattr -cr "$APFSEARCH_APP"
if [[ "${APFSEARCH_COMPILE_ONLY:-0}" == "1" || -z "$APFSEARCH_IDENTITY" ]]; then
  printf 'Compilation complete; no distribution signature, not installed: %s\n' "$APFSEARCH_APP"
  printf 'The XPC service rejects unsigned/ad-hoc builds. Configure APFSEARCH_SIGN_IDENTITY for signed integration.\n'
  exit 0
fi
codesign --force --options runtime --timestamp --identifier "$APFSEARCH_SERVICE_IDENTIFIER" --sign "$APFSEARCH_IDENTITY" "$APFSEARCH_APP/Contents/MacOS/$APFSEARCH_SERVICE_EXECUTABLE"
codesign --force --options runtime --timestamp --identifier "$APFSEARCH_CLI_IDENTIFIER" --sign "$APFSEARCH_IDENTITY" "$APFSEARCH_APP/Contents/MacOS/$APFSEARCH_CLI_EXECUTABLE"
codesign --force --options runtime --timestamp --sign "$APFSEARCH_IDENTITY" "$APFSEARCH_APP"
codesign --verify --deep --strict --verbose=2 "$APFSEARCH_APP"
python3 "$APFSEARCH_ROOT/scripts/build_identity.py" --verify-signatures "$APFSEARCH_APP"
printf '%s\n' "$APFSEARCH_APP"
