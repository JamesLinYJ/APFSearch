# Native macOS build toolchain

APFSearch requires a macOS **26 or newer SDK**, while its deployment target remains
**macOS 14**. These are different settings: the SDK linked into an executable
enables the modern AppKit appearance on newer systems; the deployment target
determines the oldest supported runtime.

The v0.1.5 GitHub Apple Silicon download linked SDK 15.5. Its source was current,
but the macOS 15 runner selected an older Xcode, leaving native toolbar and search
controls in the earlier appearance. Rebuilding with a new SDK is required; editing
Info.plist labels or adding custom blur layers does not repair that build.

Validation and release jobs select Xcode 26.6 on the macOS 26 runner explicitly.
This provides SDK 26.5 and native Liquid Glass. At the time of this change,
[GitHub's Xcode 27 image](https://github.com/actions/runner-images/blob/main/images/macos/xcode-27-arm64-Readme.md)
still ships a beta toolchain on a preview runner, so it is not the release default.
Local builds can use Xcode 27 through `DEVELOPER_DIR`. Update both workflows
together when selecting a newer stable CI toolchain; never silently fall back to
an older SDK if the selected Xcode is missing.

`build.sh` resolves Swift and the SDK from the same Xcode selection. It checks the
SDK before compiling and verifies `LC_BUILD_VERSION` in every app, service and
CLI architecture before signing. Packaging repeats that validation after slicing,
after ZIP extraction and inside the mounted DMG. All builds still require minimum
macOS 14.0 in their Mach-O load commands.

Run `python3 tests/test_build_toolchain.py` for the regression fixtures, including
a universal executable with a modern ARM slice but an outdated Intel slice.
Use `python3 scripts/build_toolchain.py --bundle /path/to/APFSearch.app` to check an
actual bundle. SDK checks establish build correctness; visually verify the signed
application on a system that supports Liquid Glass as well.
