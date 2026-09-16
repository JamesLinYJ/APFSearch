#!/bin/bash
# Build, notarize and verify the same DMG/ZIP variants distributed on GitHub.
set -euo pipefail
umask 077
APFSEARCH_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$APFSEARCH_ROOT"
python3 scripts/validate_release.py "$1"
: "${APFSEARCH_SIGN_IDENTITY:?A Developer ID Application identity is required}"
: "${APFSEARCH_NOTARY_PROFILE:?A notarytool keychain profile is required}"
: "${APFSEARCH_NOTARY_KEYCHAIN:?A dedicated notarization keychain is required}"
: "${APFSEARCH_RELEASE_DIR:?A new private release directory is required}"
: "${APFSEARCH_PACKAGING_PYTHON:?An isolated packaging interpreter is required}"
test "$APFSEARCH_SIGN_IDENTITY" != '-'
APFSEARCH_RELEASE_DIR="$(python3 scripts/build_workspace.py release "$APFSEARCH_RELEASE_DIR")"
test ! -e "$APFSEARCH_RELEASE_DIR/downloads"
export APFSEARCH_ARCHITECTURES='arm64 x86_64'
export APFSEARCH_COMPILE_ONLY=0
./build.sh
APFSEARCH_APP="$(python3 scripts/build_workspace.py build)/APFSearch.app"
ditto -c -k --keepParent "$APFSEARCH_APP" "$APFSEARCH_RELEASE_DIR/notary-submission.zip"
xcrun notarytool submit "$APFSEARCH_RELEASE_DIR/notary-submission.zip" --wait --output-format json \
  --keychain-profile "$APFSEARCH_NOTARY_PROFILE" --keychain "$APFSEARCH_NOTARY_KEYCHAIN" \
  > "$APFSEARCH_RELEASE_DIR/notary-result.json"
python3 - "$APFSEARCH_RELEASE_DIR/notary-result.json" <<'PY'
import json, sys
result = json.load(open(sys.argv[1]))
if result.get('status') != 'Accepted':
    raise SystemExit('Application notarization was not accepted: ' + str(result.get('status')))
print('Application notarization accepted.')
PY
xcrun stapler staple "$APFSEARCH_APP"
"$APFSEARCH_PACKAGING_PYTHON" scripts/package_distribution.py "$APFSEARCH_APP" \
  "$APFSEARCH_RELEASE_DIR/downloads" --notary-profile "$APFSEARCH_NOTARY_PROFILE" \
  --notary-keychain "$APFSEARCH_NOTARY_KEYCHAIN"
python3 scripts/validate_release.py "$1" --downloads "$APFSEARCH_RELEASE_DIR/downloads"
