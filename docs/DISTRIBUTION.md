# Signing and notarization

APFSearch is distributed outside the Mac App Store. Apple notarization checks the
actual signed binary; it does not certify feature completeness, performance, or
compatibility with every supported Mac. Each changed build needs its own
submission. See [Apple's notarization guide](https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution).

## Xcode account workflow

This route uses the developer account already signed into Xcode. It does not
require exporting an account password into the repository or sharing it in chat.
Select the intended Xcode with `DEVELOPER_DIR`, then build with your own Developer
ID Application identity as described in the [README](../README.md#getting-started).

Create an archive from the signed build. The destination must not already exist:

```sh
APFSEARCH_BUILT_APP="$(python3 scripts/build_workspace.py build)/APFSearch.app"
python3 scripts/create_archive.py "$APFSEARCH_BUILT_APP" /private/tmp/APFSearch-distribution
```

The helper preserves the signed application and records its real architectures,
version, and public signing identity in a standard `.xcarchive`. It also writes
manual-signing export options using that existing identity. It does not submit
anything. Keep generated archives, options, and distribution logs out of Git.

Submit using Xcode's Developer ID upload flow:

```sh
xcodebuild -exportArchive \
  -archivePath /private/tmp/APFSearch-distribution/APFSearch.xcarchive \
  -exportOptionsPlist /private/tmp/APFSearch-distribution/ExportOptions.plist \
  -exportPath /private/tmp/APFSearch-distribution/upload \
  -allowProvisioningUpdates
```

This sends the app to Apple for notarization, not to the App Store. Xcode may
require renewed account authentication. Manual signing retains the existing
Developer ID identity; the helper does not create certificates or app IDs.
Upload success is not notarization acceptance.

After Apple finishes processing, export the notarized app:

```sh
xcodebuild -exportNotarizedApp \
  -archivePath /private/tmp/APFSearch-distribution/APFSearch.xcarchive \
  -exportPath /private/tmp/APFSearch-distribution/notarized
```

If processing is still pending, keep the archive and check again later. Inspect
Xcode's distribution log for a rejected submission. Do not resubmit the same
binary merely because processing takes time.

## Verify the actual deliverable

```sh
APP=/private/tmp/APFSearch-distribution/notarized/APFSearch.app
xcrun stapler validate "$APP"
codesign --verify --deep --strict "$APP"
spctl --assess --type execute --verbose=2 "$APP"
```

Require a valid stapled ticket and Gatekeeper acceptance with source
`Notarized Developer ID`. Verify both `arm64` and `x86_64` slices in the app,
indexer, and CLI. A runtime check under Rosetta is useful, but does not replace
physical Intel hardware or minimum-OS testing.

Package the **exported, notarized** app so the ticket is included:

```sh
ditto -c -k --keepParent "$APP" /private/tmp/APFSearch-distribution/APFSearch.zip
shasum -a 256 /private/tmp/APFSearch-distribution/APFSearch.zip
```

Unpack and verify a copy before delivery. Do not modify signed resources after
notarization. The zip itself is not stapled; it carries the stapled app.

## DMG and ZIP downloads

Use Python 3.11 or later in an isolated environment for the packaging tools:

```sh
python3 -m venv /private/tmp/APFSearch-packaging-tools
/private/tmp/APFSearch-packaging-tools/bin/pip install -r scripts/requirements-distribution.txt
/private/tmp/APFSearch-packaging-tools/bin/python scripts/package_distribution.py \
  /private/tmp/APFSearch-distribution/notarized/APFSearch.app \
  /private/tmp/APFSearch-downloads
```

The output contains Universal, AppleSilicon, and Intel variants, each as a DMG
and ZIP, plus `SHA256SUMS.txt` and `downloads.json`. The helper extracts signed
Mach-O slices and requires valid app, service, and CLI signatures, a stapled app
ticket, and Gatekeeper acceptance after extraction. It also verifies an unpacked
ZIP and each mounted read-only DMG. It refuses embedded personal home paths.

DMG windows use an 800 × 500 point background with standard and Retina TIFF
representations, a real application icon, and a shortcut to `/Applications`.
Finder window metadata is generated with [dmgbuild](https://dmgbuild.readthedocs.io/),
without automating Finder or changing user preferences. The signed app's Finder
attributes are left untouched. Artwork remains editable in `Resources/Brand`.

By default the DMG is an unsigned container carrying the signed, stapled app;
the container itself has not been notarized. The manifest records that boundary.
To also sign and notarize each final DMG, set `APFSEARCH_SIGN_IDENTITY` and pass
`--notary-profile` with an existing notarytool keychain profile. Hashes are written
after any container stapling. Never advertise container notarization based only
on an app ticket.

Builds remap Rust source paths and strip linker debug-map entries before signing.
Public release bundles contain no index, search history, credentials, or local
validation captures. Developer ID certificates do expose Apple's registered
signing identity; neutral package names cannot anonymize that certificate.

## GitHub Actions releases

The `Signed macOS release` workflow builds Universal, AppleSilicon and Intel
DMG/ZIP downloads. It uses a Developer ID Application identity and an App Store
Connect API key; it does not need an Apple account password, Developer ID
Installer certificate, or update-signing key.

Configure a GitHub environment named `release`, with deployment branches limited
to `main`. Store these **environment secrets**, not repository files or variables:

| Secret | Value |
| --- | --- |
| `APFSEARCH_CERTIFICATE_P12` | Base64-encoded, password-encrypted export of only the selected Developer ID Application identity |
| `APFSEARCH_CERTIFICATE_PASSWORD` | Random password protecting that export |
| `APFSEARCH_SIGN_IDENTITY` | Selected certificate's SHA-1 fingerprint |
| `APP_STORE_CONNECT_PRIVATE_KEY` | Dedicated App Store Connect team API private key, using the Developer role |
| `APP_STORE_CONNECT_KEY_ID` | API key identifier |
| `APP_STORE_CONNECT_ISSUER_ID` | API issuer identifier |

Upload secret values through GitHub's encrypted Secrets interface or `gh secret
set --env release` with standard input. Do not place them in commands, Git,
issue text, screenshots, or workflow artifacts. Keep the original Apple key in
a private local location; Apple allows downloading it only once. An API key can
be revoked independently of the Developer ID certificate.

Update the app and Cargo version together and add `docs/releases/VERSION.md`.
Open a pull request, pass the required checks and squash-merge into `main`, then run **Actions → Signed macOS release → Run
workflow** on `main`, supplying its full commit SHA and a new matching `vX.Y.Z`
tag. The workflow checks version consistency and requires the selected commit
to equal the current `main` checkout. Tags and published releases cannot be
overwritten. Pull requests and other branches cannot use the release environment.

Tests run before credentials are imported. Raw secret environment variables are
scoped to the import step; credentials enter a temporary private keychain, removed
even when a later step fails. Notarization must return `Accepted`; signature,
stapled ticket and Gatekeeper checks are required for every architecture and
every mounted DMG. Only the eight allowlisted download files are published.
The workflow downloads its draft assets and compares them before making the
stable release public and marking it as latest. Failures leave any draft unpublished for inspection.
No local index, runtime report or credentials are uploaded as workflow artifacts.

GitHub repository administrators and trusted code on `main` control release
secrets. Restrict repository write access accordingly. If adding collaborators,
consider required reviews and environment reviewers before granting write access.

## Optional installer and automated update releases

`scripts/release.sh` can separately build a
signed installer and authenticated update manifest. It requires separately
configured Developer ID Installer and update-signing credentials, and uses
`notarytool` with a keychain profile or app-specific credentials. It notarizes
the app and final installer, verify both tickets, and hash the final package
after stapling. Their credentials belong in the local keychain or protected CI
secrets, never in source control.
