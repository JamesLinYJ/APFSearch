# APFSearch artwork

`AppIcon.svg` is the editable 1024 × 1024 application icon. Its transparent outer margin keeps the blue tile aligned with other macOS application icons. The folded document and search lens remain recognizable without a wordmark. Gradients and restrained shadows provide depth without a busy background.

`Mark.svg` is a single-color, transparent version for documentation and other small placements. Change its stroke color to adapt it to a light or dark surface. Neither source contains fonts, linked images, scripts, or external resources.

The artwork is original to this project and distributed under its MIT license.

## Generate the macOS icon

Run from the repository root on macOS with the Swift command-line tools installed:

```sh
swift scripts/build_icon.swift Resources/Brand/AppIcon.svg /private/tmp/APFSearch-artwork/AppIcon.icns
```

The normal `build.sh` runs this step automatically before code signing. It renders the SVG directly at each size in the sRGB color space, then uses `iconutil` to package these representations:

| Logical size | Standard pixels | Retina pixels |
| --- | --- | --- |
| 16 pt | 16 | 32 |
| 32 pt | 32 | 64 |
| 128 pt | 128 | 256 |
| 256 pt | 256 | 512 |
| 512 pt | 512 | 1024 |

Intermediate PNG files are temporary. Generated icons belong in build output, not source control. Keep the bundle icon declaration in `scripts/build_identity.py` in sync with the output filename in `build.sh`.

## Installer window

`InstallerBackground.svg` supplies the 800 × 500 point DMG background and direction arrow. `scripts/render_installer.swift` adds typography using macOS system fonts and writes a TIFF with 1× and 2× representations. The app and Applications icons are real Finder items, not painted into the background.

```sh
swift scripts/render_installer.swift Resources/Brand/InstallerBackground.svg /private/tmp/APFSearch-artwork/InstallerBackground.tiff
```

See the [distribution guide](../../docs/DISTRIBUTION.md#dmg-and-zip-downloads) for reproducible window layout and packaging.
