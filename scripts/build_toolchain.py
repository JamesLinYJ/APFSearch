#!/usr/bin/env python3
"""Require a modern macOS SDK in the toolchain and every distributed Mach-O slice."""
import argparse
import pathlib
import re
import subprocess

from build_configuration import MINIMUM_MACOS_SDK_VERSION, MINIMUM_MACOS_VERSION
from build_identity import IDENTITY


def version_tuple(value):
    if not re.fullmatch(r'[0-9]+(?:\.[0-9]+){0,2}', value):
        raise ValueError(f'Invalid SDK or deployment version: {value!r}')
    return tuple(int(part) for part in value.split('.')) + (0,) * (3 - len(value.split('.')))


def require_sdk(version):
    if version_tuple(version) < version_tuple(MINIMUM_MACOS_SDK_VERSION):
        raise ValueError(f'macOS SDK {version} is too old: select Xcode with macOS SDK '
                         f'{MINIMUM_MACOS_SDK_VERSION} or later for native Liquid Glass.')


def output(*arguments):
    return subprocess.run(arguments, check=True, capture_output=True, text=True).stdout.strip()


def validate_build_commands(load_commands):
    """Inspect one architecture. A plist SDK label cannot substitute for linking."""
    commands = re.split(r'^Load command \d+\s*$', load_commands, flags=re.MULTILINE)
    builds = [command for command in commands if re.search(r'^\s*cmd LC_BUILD_VERSION\s*$', command, re.MULTILINE)]
    if len(builds) != 1:
        raise ValueError('Expected one LC_BUILD_VERSION in each Mach-O architecture')
    fields = dict(re.findall(r'^\s*(platform|minos|sdk)\s+(\S+)\s*$', builds[0], re.MULTILINE))
    if fields.get('platform') not in ('1', 'MACOS'):
        raise ValueError('Expected a macOS Mach-O slice')
    require_sdk(fields.get('sdk', ''))
    if version_tuple(fields.get('minos', '')) != version_tuple(MINIMUM_MACOS_VERSION):
        raise ValueError(f'Mach-O deployment target must remain {MINIMUM_MACOS_VERSION}')
    return fields['sdk']


def verify_bundle_sdk(application):
    architectures = None
    for key in ('applicationExecutable', 'serviceExecutable', 'cliExecutable'):
        binary = application / 'Contents/MacOS' / IDENTITY[key]
        slices = set(output('xcrun', 'lipo', '-archs', str(binary)).split())
        if not slices or not slices <= {'arm64', 'x86_64'}:
            raise ValueError(f'{binary.name} has unsupported or missing architectures')
        if architectures is not None and slices != architectures:
            raise ValueError('Application, service and CLI architectures must agree')
        architectures = slices
        for architecture in sorted(slices):
            sdk = validate_build_commands(output('xcrun', 'otool', '-arch', architecture, '-l', str(binary)))
            print(f'{binary.name} ({architecture}): SDK {sdk}, minimum macOS {MINIMUM_MACOS_VERSION}')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bundle', type=pathlib.Path)
    args = parser.parse_args()
    if args.bundle:
        verify_bundle_sdk(args.bundle)
    else:
        version = output('xcrun', '--sdk', 'macosx', '--show-sdk-version')
        require_sdk(version)
        print(output('xcodebuild', '-version'))
        print(f'macOS SDK {version}; minimum runtime macOS {MINIMUM_MACOS_VERSION}')


if __name__ == '__main__':
    try:
        main()
    except (ValueError, subprocess.CalledProcessError) as error:
        raise SystemExit(str(error)) from error
