#!/usr/bin/env python3
"""Fail closed on version drift or an incomplete/publicly unsafe download set."""
import argparse
import hashlib
import json
from pathlib import Path
import plistlib
import re
import tempfile
import tomllib

from build_identity import PROJECT, prepare_bundle

VARIANTS = {'Universal': ['arm64', 'x86_64'], 'AppleSilicon': ['arm64'], 'Intel': ['x86_64']}


def release_version(tag):
    if not re.fullmatch(r'v[0-9]{1,6}(?:\.[0-9]{1,6}){2}', tag):
        raise ValueError('Expected a numeric vMajor.Minor.Patch tag')
    return tag[1:]


def validate_source(tag):
    version = release_version(tag)
    manifest = tomllib.loads((PROJECT / 'core/Cargo.toml').read_text())
    lock = tomllib.loads((PROJECT / 'core/Cargo.lock').read_text())
    package = manifest['package']
    locked = [item['version'] for item in lock['package'] if item['name'] == package['name']]
    with tempfile.TemporaryDirectory(prefix='APFSearch-release-metadata-') as temporary:
        bundle = Path(temporary) / 'APFSearch.app'
        (bundle / 'Contents').mkdir(parents=True)
        prepare_bundle(bundle)
        info = plistlib.loads((bundle / 'Contents/Info.plist').read_bytes())
    if package['version'] != version or locked != [version] or info['CFBundleShortVersionString'] != version:
        raise ValueError('Tag, Cargo manifest/lock and app version must agree')
    if info['CFBundleVersion'] != '1':
        raise ValueError('The internal build version must remain 1')
    notes = PROJECT / 'docs/releases' / (version + '.md')
    if not notes.is_file() or not notes.read_text().strip():
        raise ValueError('Version-specific release notes are required')
    return version


def validate_downloads(directory, version):
    expected = {f'APFSearch-{version}-{variant}.{extension}': (architectures, extension)
                for variant, architectures in VARIANTS.items() for extension in ('zip', 'dmg')}
    files = list(directory.iterdir())
    if {path.name for path in files} != set(expected) | {'downloads.json', 'SHA256SUMS.txt'}:
        raise ValueError('Only the complete eight-file public download set is allowed')
    if any(path.is_symlink() or not path.is_file() for path in files):
        raise ValueError('Release assets must be regular files, never links or directories')
    manifest = json.loads((directory / 'downloads.json').read_text())
    if manifest['version'] != version or manifest['build'] != '1' or manifest['minimum_macos_version'] != '14.0':
        raise ValueError('Download metadata disagrees with the release identity')
    records = manifest['downloads']
    if len(records) != len(expected) or {item['file'] for item in records} != set(expected):
        raise ValueError('Manifest must describe each expected variant exactly once')
    checksums = {}
    for item in records:
        name = item['file']
        path = directory / name
        architectures, extension = expected[name]
        if item['architectures'] != architectures or item['application_notarized'] is not True:
            raise ValueError('Unexpected architectures or missing app notarization')
        if item['container_notarized'] is not (extension == 'dmg'):
            raise ValueError('DMGs require their own notarization; ZIPs carry the app ticket')
        with path.open('rb') as stream:
            digest = hashlib.file_digest(stream, 'sha256').hexdigest()
        if item['sha256'] != digest or item['bytes'] != path.stat().st_size or item['bytes'] <= 0:
            raise ValueError('An asset differs from its recorded size or checksum')
        checksums[name] = digest
    lines = (directory / 'SHA256SUMS.txt').read_text().splitlines()
    if len(lines) != len(expected) or set(lines) != {f'{digest}  {name}' for name, digest in checksums.items()}:
        raise ValueError('The checksum list must match every final download')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('tag')
    parser.add_argument('--downloads', type=Path)
    args = parser.parse_args()
    version = validate_source(args.tag)
    if args.downloads:
        validate_downloads(args.downloads, version)
    print('Release identity and requested artifacts verified.')
