#!/bin/bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BUILD="${FILESEARCH_BUILD_DIR:-/private/tmp/FileSearch-release}"
OUT="${FILESEARCH_RELEASE_DIR:-$ROOT/dist}"
APP_NAME="$(python3 "$ROOT/scripts/build_identity.py" applicationExecutable)"
APP="$BUILD/$APP_NAME.app"
PKG="$OUT/$APP_NAME.pkg"

: "${FILESEARCH_SIGN_IDENTITY:?Set FILESEARCH_SIGN_IDENTITY to a Developer ID Application identity}"
: "${FILESEARCH_INSTALLER_IDENTITY:?Set FILESEARCH_INSTALLER_IDENTITY to a Developer ID Installer identity}"

rm -rf "$BUILD"
mkdir -p "$OUT"
FILESEARCH_BUILD_DIR="$BUILD" "$ROOT/build.sh"

codesign --verify --deep --strict --verbose=2 "$APP"
spctl --assess --type execute --verbose=2 "$APP"
rm -f "$PKG"
productbuild --component "$APP" /Applications --sign "$FILESEARCH_INSTALLER_IDENTITY" "$PKG"
pkgutil --check-signature "$PKG"

notarize=(xcrun notarytool submit "$PKG" --wait)
if [[ -n "${FILESEARCH_NOTARY_PROFILE:-}" ]]; then
  notarize+=(--keychain-profile "$FILESEARCH_NOTARY_PROFILE")
else
  : "${APPLE_ID:?Set APPLE_ID or FILESEARCH_NOTARY_PROFILE}"
  : "${APPLE_TEAM_ID:?Set APPLE_TEAM_ID or FILESEARCH_NOTARY_PROFILE}"
  : "${APPLE_APP_SPECIFIC_PASSWORD:?Set APPLE_APP_SPECIFIC_PASSWORD or FILESEARCH_NOTARY_PROFILE}"
  notarize+=(--apple-id "$APPLE_ID" --team-id "$APPLE_TEAM_ID" --password "$APPLE_APP_SPECIFIC_PASSWORD")
fi
"${notarize[@]}"

xcrun stapler staple "$APP"
xcrun stapler validate "$APP"
# Rebuild the signed installer so the stapled app is the payload that users receive.
rm -f "$PKG"
productbuild --component "$APP" /Applications --sign "$FILESEARCH_INSTALLER_IDENTITY" "$PKG"
"${notarize[@]/$PKG/$PKG}"
xcrun stapler staple "$PKG"
xcrun stapler validate "$PKG"
pkgutil --check-signature "$PKG"
spctl --assess --type install --verbose=2 "$PKG"

shasum -a 256 "$PKG" | tee "$PKG.sha256"
printf 'Release installer: %s\n' "$PKG"
