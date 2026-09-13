#!/usr/bin/env python3
"""Package a test executable with the same String Catalog as the app."""
import argparse
import pathlib
import plistlib
import shutil
import subprocess
import sys
import uuid

PROJECT = pathlib.Path(__file__).resolve().parents[1]
sys.path.insert(0, str(PROJECT / 'scripts'))
from build_identity import catalog_languages
from build_configuration import MINIMUM_MACOS_VERSION, swift_target


def create_test_bundle(executable, app):
    executable, app = pathlib.Path(executable), pathlib.Path(app)
    contents = app / 'Contents'
    resources, macos = contents / 'Resources', contents / 'MacOS'
    resources.mkdir(parents=True, exist_ok=True)
    macos.mkdir(parents=True, exist_ok=True)
    bundled_executable = macos / executable.name
    if executable.resolve() != bundled_executable.resolve():
        shutil.copy2(executable, bundled_executable)
    info = {
        'CFBundleName': executable.name, 'CFBundleExecutable': executable.name,
        'CFBundleIdentifier': 'org.apfsearch.app.tests.' + executable.name.lower() + '.' + uuid.uuid4().hex,
        'CFBundlePackageType': 'APPL', 'CFBundleVersion': '1',
        'CFBundleDevelopmentRegion': 'en',
        'CFBundleLocalizations': catalog_languages(),
        'LSMinimumSystemVersion': MINIMUM_MACOS_VERSION, 'LSUIElement': True,
        'NSHighResolutionCapable': True,
    }
    (contents / 'Info.plist').write_bytes(plistlib.dumps(info))
    for table in ['Localizable', 'Features', 'InfoPlist']:
        subprocess.run(['xcrun', 'xcstringstool', 'compile', str(PROJECT / f'Resources/{table}.xcstrings'),
                        '--output-directory', str(resources)], check=True)
    return bundled_executable


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('executable', type=pathlib.Path)
    parser.add_argument('app', type=pathlib.Path)
    args = parser.parse_args()
    print(create_test_bundle(args.executable, args.app))
