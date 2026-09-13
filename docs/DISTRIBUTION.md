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
python3 scripts/create_archive.py /private/tmp/APFSearch-build/APFSearch.app /private/tmp/APFSearch-distribution
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

## Installer and automated update releases

`scripts/release.sh` and `.github/workflows/release.yml` additionally build a
signed installer and authenticated update manifest. They require separately
configured Developer ID Installer and update-signing credentials, and use
`notarytool` with a keychain profile or app-specific credentials. They notarize
the app and final installer, verify both tickets, and hash the final package
after stapling. Their credentials belong in the local keychain or protected CI
secrets, never in source control.
