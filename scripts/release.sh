#!/bin/bash
# Build in a new child directory; never remove a caller-supplied build root.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
: "${FILESEARCH_SIGN_IDENTITY:?Developer ID Application identity required}"
: "${FILESEARCH_INSTALLER_IDENTITY:?Developer ID Installer identity required}"
: "${FILESEARCH_RELEASE_VERSION:?Numeric release version required}"
: "${FILESEARCH_UPDATE_PUBLIC_KEY:?Embedded update public key required}"
: "${FILESEARCH_UPDATE_PRIVATE_KEY:?Matching update signing key required}"
: "${FILESEARCH_UPDATE_FEED_URL:?HTTPS update feed required}"
: "${FILESEARCH_UPDATE_URL:?HTTPS immutable package URL required}"
[[ "$FILESEARCH_SIGN_IDENTITY" != '-' ]]
[[ "$FILESEARCH_RELEASE_VERSION" =~ ^[0-9]{1,6}(\.[0-9]{1,6}){1,3}$ ]]
BUILD_ROOT="${FILESEARCH_BUILD_DIR:-/private/tmp/FileSearch-release}"
OUT="${FILESEARCH_RELEASE_DIR:-$ROOT/dist/$FILESEARCH_RELEASE_VERSION}"
mkdir -p "$BUILD_ROOT" "$(dirname "$OUT")"
# Refuse replacing a previous release or another process's output.
mkdir "$OUT"
BUILD="$(mktemp -d "$BUILD_ROOT/build.XXXXXX")"
cleanup() { rm -rf -- "$BUILD"; }
trap cleanup EXIT
APP_NAME="$(python3 "$ROOT/scripts/build_identity.py" applicationExecutable)"
APP="$BUILD/$APP_NAME.app"
PKG="$OUT/$APP_NAME.pkg"
ARCHIVE="$BUILD/application.zip"
FILESEARCH_COMPILE_ONLY=0 FILESEARCH_BUILD_DIR="$BUILD" "$ROOT/build.sh"
codesign --verify --deep --strict --verbose=2 "$APP"

notary_args=()
if [[ -n "${FILESEARCH_NOTARY_PROFILE:-}" ]]; then
  notary_args+=(--keychain-profile "$FILESEARCH_NOTARY_PROFILE")
else
  : "${APPLE_ID:?Apple ID or a notary keychain profile required}"
  : "${APPLE_TEAM_ID:?Apple team required}"
  : "${APPLE_APP_SPECIFIC_PASSWORD:?App-specific notary password required}"
  notary_args+=(--apple-id "$APPLE_ID" --team-id "$APPLE_TEAM_ID" --password "$APPLE_APP_SPECIFIC_PASSWORD")
fi
notarize() {
  xcrun notarytool submit "$1" --wait --output-format json "${notary_args[@]}" > "$BUILD/notary.json"
  python3 - "$BUILD/notary.json" <<'PY'
import json, sys
result = json.load(open(sys.argv[1]))
if result.get('status') != 'Accepted':
    raise SystemExit('Notarization did not return Accepted: ' + str(result.get('status')))
print('Notarization accepted: ' + str(result.get('id')))
PY
}
# Assess with Gatekeeper only after the notarization ticket exists.
ditto -c -k --keepParent "$APP" "$ARCHIVE"
notarize "$ARCHIVE"
xcrun stapler staple "$APP"
xcrun stapler validate "$APP"
codesign --verify --deep --strict --verbose=2 "$APP"
spctl --assess --type execute --verbose=2 "$APP"
productbuild --component "$APP" /Applications --sign "$FILESEARCH_INSTALLER_IDENTITY" "$PKG"
pkgutil --check-signature "$PKG"
notarize "$PKG"
xcrun stapler staple "$PKG"
xcrun stapler validate "$PKG"
pkgutil --check-signature "$PKG"
spctl --assess --type install --verbose=2 "$PKG"
# Stapling changes the package, so hash/sign only these final delivered bytes.
DIGEST="$(shasum -a 256 "$PKG" | awk '{print $1}')"
SIZE="$(stat -f '%z' "$PKG")"
printf '%s  %s\n' "$DIGEST" "$APP_NAME.pkg" > "$PKG.sha256"
swift "$ROOT/scripts/sign_update_manifest.swift" "$FILESEARCH_RELEASE_VERSION" \
  "$FILESEARCH_UPDATE_URL" "$DIGEST" "$SIZE" "$OUT/update.json"
printf 'Verified release installer: %s\n' "$PKG"
