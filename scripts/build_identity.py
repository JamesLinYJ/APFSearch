#!/usr/bin/env python3
"""Use the Swift identity constants for bundle metadata and code signing."""
import argparse
import json
import pathlib
import plistlib
import re
import subprocess
from build_configuration import MINIMUM_MACOS_VERSION

PROJECT = pathlib.Path(__file__).resolve().parents[1]
IDENTITY = dict(re.findall(r'^    static let (\w+) = "([^"]*)"$', (PROJECT / 'macos/ApplicationIdentity.swift').read_text(), re.MULTILINE))


def catalog_languages():
    """Use the native permission/display-name catalog for bundle language metadata."""
    catalog = json.loads((PROJECT / 'Resources/InfoPlist.xcstrings').read_text())
    return sorted(catalog['strings']['CFBundleDisplayName']['localizations'])


def verify_bundle_signatures(bundle):
    """Require the same trusted Apple team for the app, CLI, and indexer."""
    executables = bundle / 'Contents/MacOS'
    signed_objects = [
        (executables / IDENTITY['serviceExecutable'], IDENTITY['serviceIdentifier']),
        (executables / IDENTITY['cliExecutable'], IDENTITY['cliIdentifier']),
        (bundle, IDENTITY['bundleIdentifier']),
    ]
    team = None
    for path, identifier in signed_objects:
        detail = subprocess.run(['codesign', '--display', '--verbose=4', str(path)],
                                check=True, text=True, capture_output=True)
        match = re.search(r'^TeamIdentifier=([A-Z0-9]{10})$', detail.stderr, re.MULTILINE)
        if not match:
            raise ValueError(f'{path.name} does not have an Apple development team signature')
        current_team = match.group(1)
        if team is not None and team != current_team:
            raise ValueError('App, CLI, and indexer must be signed by the same team')
        team = current_team
        requirement = (f'anchor apple generic and identifier "{identifier}" '
                       f'and certificate leaf[subject.OU] = "{team}"')
        subprocess.run(['codesign', '--verify', '--strict', '--test-requirement', '=' + requirement,
                        str(path)], check=True)


def prepare_bundle(bundle):
    contents = bundle / 'Contents'
    info_strings = json.loads((PROJECT / 'Resources/InfoPlist.xcstrings').read_text())['strings']
    info = {key: record['localizations']['en']['stringUnit']['value'] for key, record in info_strings.items()}
    info.update({
        'CFBundleName': IDENTITY['applicationExecutable'], 'CFBundleIdentifier': IDENTITY['bundleIdentifier'],
        'CFBundleDevelopmentRegion': 'en', 'CFBundleLocalizations': catalog_languages(),
        'CFBundleVersion': '1', 'CFBundleShortVersionString': '0.1.6',
        'CFBundleExecutable': IDENTITY['applicationExecutable'], 'CFBundlePackageType': 'APPL',
        'CFBundleIconFile': 'AppIcon',
        'LSMinimumSystemVersion': MINIMUM_MACOS_VERSION, 'NSHighResolutionCapable': True,
        'NSPrincipalClass': 'NSApplication',
    })
    (contents / 'Info.plist').write_bytes(plistlib.dumps(info))
    agents = contents / 'Library/LaunchAgents'
    agents.mkdir(parents=True, exist_ok=True)
    label = IDENTITY['serviceIdentifier']
    # Searches are user-requested work. Let launchd account for XPC activity
    # instead of permanently throttling the service as unattended background work.
    agent = {'Label': label, 'BundleProgram': 'Contents/MacOS/' + IDENTITY['serviceExecutable'],
             'MachServices': {label: True}, 'RunAtLoad': True, 'ProcessType': 'Adaptive'}
    (agents / (label + '.plist')).write_bytes(plistlib.dumps(agent))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('field', nargs='?', choices=sorted(IDENTITY))
    parser.add_argument('--bundle', type=pathlib.Path)
    parser.add_argument('--verify-signatures', type=pathlib.Path)
    args = parser.parse_args()
    if args.verify_signatures:
        verify_bundle_signatures(args.verify_signatures)
    elif args.bundle:
        prepare_bundle(args.bundle)
    elif args.field:
        print(IDENTITY[args.field])
    else:
        parser.error('Provide a field, --bundle, or --verify-signatures')
