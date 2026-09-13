#!/bin/bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BUILD="${FILESEARCH_BUILD_DIR:-/private/tmp/FileSearch-release}"
OUT="${FILESEARCH_RELEASE_DIR:-$ROOT/dist}"
APP_NAME="$(python3 "$ROOT/scripts/build_identity.py" applicationExecutable)"
APP="$BUILD/$APP_NAME.app"
PKG="$OUT/$APP_NAME.pkg"
APP_ARCHIVE="$OUT/$APP_NAME-notarization.zip"

: "${FILESEARCH_SIGN_IDENTITY:?Set FILESEARCH_SIGN_IDENTITY to a Developer ID Application identity}"
: "${FILESEARCH_INSTALLER_IDENTITY:?Set FILESEARCH_INSTALLER_IDENTITY to a Developer ID Installer identity}"

rm -rf "$BUILD"
mkdir -p "$OUT"
FILESEARCH_BUILD_DIR="$BUILD" "$ROOT/build.sh"

codesign --verify --deep --strict --verbose=2 "$APP"
spctl --assess --type execute --verbose=2 "$APP"

notary_args=()
if [[ -n "${FILESEARCH_NOTARY_PROFILE:-}" ]]; then
  notary_args+=(--keychain-profile "$FILESEARCH_NOTARY_PROFILE")
else
  : "${APPLE_ID:?Set APPLE_ID or FILESEARCH_NOTARY_PROFILE}"
  : "${APPLE_TEAM_ID:?Set APPLE_TEAM_ID or FILESEARCH_NOTARY_PROFILE}"
  : "${APPLE_APP_SPECIFIC_PASSWORD:?Set APPLE_APP_SPECIFIC_PASSWORD or FILESEARCH_NOTARY_PROFILE}"
  notary_args+=(--apple-id "$APPLE_ID" --team-id "$APPLE_TEAM_ID" --password "$APPLE_APP_SPECIFIC_PASSWORD")
fi

# Submit the signed app itself first, then package the stapled app. This keeps
# both the application and the installer independently valid when distributed.
rm -f "$APP_ARCHIVE"
ditto -c -k --keepParent "$APP" "$APP_ARCHIVE"
xcrun notarytool submit "$APP_ARCHIVE" --wait "${notary_args[@]}"
xcrun stapler staple "$APP"
xcrun stapler validate "$APP"
codesign --verify --deep --strict --verbose=2 "$APP"
spctl --assess --type execute --verbose=2 "$APP"
rm -f "$APP_ARCHIVE"

rm -f "$PKG"
productbuild --component "$APP" /Applications --sign "$FILESEARCH_INSTALLER_IDENTITY" "$PKG"
pkgutil --check-signature "$PKG"
xcrun notarytool submit "$PKG" --wait "${notary_args[@]}"
xcrun stapler staple "$PKG"
xcrun stapler validate "$PKG"
pkgutil --check-signature "$PKG"
spctl --assess --type install --verbose=2 "$PKG"

DIGEST="$(shasum -a 256 "$PKG" | awk '{print $1}')"
printf '%s  %s\n' "$DIGEST" "$(basename "$PKG")" | tee "$PKG.sha256"
if [[ -n "${FILESEARCH_RELEASE_VERSION:-}" || -n "${FILESEARCH_UPDATE_URL:-}" || -n "${FILESEARCH_UPDATE_PRIVATE_KEY:-}" ]]; then
  : "${FILESEARCH_RELEASE_VERSION:?Set all update-manifest variables together}"
  : "${FILESEARCH_UPDATE_URL:?Set all update-manifest variables together}"
  : "${FILESEARCH_UPDATE_PRIVATE_KEY:?Set all update-manifest variables together}"
  swift "$ROOT/scripts/sign_update_manifest.swift" \
    "$FILESEARCH_RELEASE_VERSION" "$FILESEARCH_UPDATE_URL" "$DIGEST" "$OUT/update.json"
fi
printf 'Release installer: %s\n' "$PKG"
