#!/usr/bin/env python3
"""Create verified architecture-specific DMG and ZIP downloads from a notarized app.

The source must contain both architectures. No source build, account lookup, or
credential export happens here. Output directories are never reused.
"""
import argparse
import hashlib
import json
import os
import pathlib
import plistlib
import re
import subprocess
import tempfile

import dmgbuild

from build_identity import IDENTITY, PROJECT, verify_bundle_signatures
from build_toolchain import verify_bundle_sdk

VARIANTS = {'Universal': ('arm64', 'x86_64'), 'AppleSilicon': ('arm64',), 'Intel': ('x86_64',)}


def run(*arguments):
    return subprocess.run([str(value) for value in arguments], check=True, capture_output=True).stdout


def verify_application(application, architectures):
    verify_bundle_sdk(application)
    verify_bundle_signatures(application)
    run('codesign', '--verify', '--deep', '--strict', application)
    run('xcrun', 'stapler', 'validate', application)
    assessment = subprocess.run(['spctl', '--assess', '--type', 'execute', '--verbose=2', str(application)],
                                check=True, capture_output=True, text=True)
    if 'source=Notarized Developer ID' not in assessment.stderr:
        raise ValueError('Gatekeeper did not identify a notarized Developer ID application')
    for key in ('applicationExecutable', 'serviceExecutable', 'cliExecutable'):
        binary = application / 'Contents/MacOS' / IDENTITY[key]
        if re.search(rb'/Users/(?!Shared/)[^/\x00]+/', binary.read_bytes()):
            raise ValueError(f'{binary.name} contains a private build path; rebuild with prefix mapping')
        actual = run('xcrun', 'lipo', '-archs', binary).decode().split()
        if set(actual) != set(architectures):
            raise ValueError(f'{binary.name} has unexpected architectures: {actual}')


def prepare_application(source, destination, architectures):
    run('ditto', source, destination)
    if len(architectures) == 1:
        for key in ('applicationExecutable', 'serviceExecutable', 'cliExecutable'):
            binary = destination / 'Contents/MacOS' / IDENTITY[key]
            temporary = binary.with_suffix('.thin')
            # Each Mach-O slice has its own signature. Extract it without
            # changing its bytes, then require both signature and ticket checks.
            run('xcrun', 'lipo', binary, '-thin', architectures[0], '-output', temporary)
            temporary.replace(binary)
    verify_application(destination, architectures)


def verify_disk_image(image, architectures):
    run('hdiutil', 'verify', image)
    mounted = plistlib.loads(run('hdiutil', 'attach', '-readonly', '-nobrowse', '-noautoopen', '-plist', image))
    volume = next(item for item in mounted['system-entities'] if 'mount-point' in item)
    try:
        root = pathlib.Path(volume['mount-point'])
        if (root / 'Applications').readlink() != pathlib.Path('/Applications'):
            raise ValueError('The installation shortcut must target /Applications')
        verify_application(root / 'APFSearch.app', architectures)
    finally:
        run('hdiutil', 'detach', volume['dev-entry'])


def notarize_disk_image(image, profile, keychain=None):
    identity = os.environ.get('APFSEARCH_SIGN_IDENTITY')
    if not identity or identity == '-':
        raise ValueError('DMG notarization requires APFSEARCH_SIGN_IDENTITY')
    run('codesign', '--sign', identity, '--timestamp', image)
    arguments = ['xcrun', 'notarytool', 'submit', image, '--wait',
                 '--output-format', 'json', '--keychain-profile', profile]
    if keychain:
        arguments.extend(['--keychain', keychain])
    response = json.loads(run(*arguments))
    if response.get('status') != 'Accepted':
        raise ValueError('DMG notarization was not accepted: ' + str(response.get('status')))
    run('xcrun', 'stapler', 'staple', image)
    run('xcrun', 'stapler', 'validate', image)
    run('codesign', '--verify', '--strict', image)
    run('spctl', '--assess', '--type', 'open', '--context', 'context:primary-signature', image)


def package_distribution(source, destination, variants, notary_profile=None, notary_keychain=None):
    source = source.resolve()
    verify_application(source, VARIANTS['Universal'])
    info = plistlib.loads((source / 'Contents/Info.plist').read_bytes())
    version = info['CFBundleShortVersionString']
    if not re.fullmatch(r'[0-9]+(?:\.[0-9]+){1,3}', version):
        raise ValueError('The application version must be numeric')
    destination.mkdir(parents=True, exist_ok=False)
    records = []
    with tempfile.TemporaryDirectory(prefix='APFSearch-packaging-') as directory:
        staging = pathlib.Path(directory)
        background = staging / 'InstallerBackground.tiff'
        run('swift', '-module-cache-path', staging / 'ModuleCache', PROJECT / 'scripts/render_installer.swift',
            PROJECT / 'Resources/Brand/InstallerBackground.svg', background)
        for variant in variants:
            architectures = VARIANTS[variant]
            application = staging / variant / 'APFSearch.app'
            prepare_application(source, application, architectures)
            basename = f'APFSearch-{version}-{variant}'
            archive = destination / (basename + '.zip')
            run('ditto', '-c', '-k', '--keepParent', application, archive)
            with tempfile.TemporaryDirectory(prefix='APFSearch-archive-check-') as extraction:
                run('ditto', '-x', '-k', archive, extraction)
                verify_application(pathlib.Path(extraction) / application.name, architectures)
            image = destination / (basename + '.dmg')
            dmgbuild.build_dmg(str(image), f'APFSearch {version} {variant}', settings={
                'format': 'UDZO', 'compression_level': 6,
                'files': [str(application)], 'symlinks': {'Applications': '/Applications'},
                # Setting Finder flags on the signed app adds FinderInfo and
                # invalidates strict signature verification. Leave it untouched.
                'icon': str(application / 'Contents/Resources/AppIcon.icns'),
                'background': str(background), 'window_rect': ((180, 160), (800, 500)),
                'icon_locations': {application.name: (220, 237), 'Applications': (580, 237)},
                'default_view': 'icon-view', 'include_icon_view_settings': True,
                'include_list_view_settings': False, 'show_icon_preview': False,
                'show_toolbar': False, 'show_sidebar': False, 'show_status_bar': False,
                'show_pathbar': False, 'show_tab_view': False,
                'arrange_by': None, 'grid_spacing': 80, 'grid_offset': (0, 0),
                'scroll_position': (0, 0), 'label_pos': 'bottom', 'text_size': 14, 'icon_size': 112,
            }, lookForHiDPI=False)
            if notary_profile:
                notarize_disk_image(image, notary_profile, notary_keychain)
            verify_disk_image(image, architectures)
            for artifact in (archive, image):
                with artifact.open('rb') as stream:
                    digest = hashlib.file_digest(stream, 'sha256').hexdigest()
                records.append({'file': artifact.name, 'sha256': digest, 'bytes': artifact.stat().st_size,
                                'architectures': architectures, 'application_notarized': True,
                                'container_notarized': bool(notary_profile) if artifact == image else False})
            print(f'Verified {variant}: DMG and ZIP', flush=True)
    (destination / 'SHA256SUMS.txt').write_text(''.join(f"{record['sha256']}  {record['file']}\n" for record in records))
    (destination / 'downloads.json').write_text(json.dumps({
        'version': version, 'build': info['CFBundleVersion'],
        'minimum_macos_version': info['LSMinimumSystemVersion'], 'downloads': records,
    }, indent=2) + '\n')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('application', type=pathlib.Path, help='Signed, stapled universal application')
    parser.add_argument('destination', type=pathlib.Path, help='New directory for verified release downloads')
    parser.add_argument('--variant', choices=VARIANTS, action='append', help='Defaults to all three variants')
    parser.add_argument('--notary-profile', help='Optional existing notarytool profile to also notarize DMG containers')
    parser.add_argument('--notary-keychain', help='Dedicated keychain containing the notarization profile')
    args = parser.parse_args()
    variants = args.variant or list(VARIANTS)
    if len(set(variants)) != len(variants):
        parser.error('Each variant may be selected only once')
    if args.notary_keychain and not args.notary_profile:
        parser.error('--notary-keychain requires --notary-profile')
    package_distribution(args.application, args.destination, variants, args.notary_profile, args.notary_keychain)
