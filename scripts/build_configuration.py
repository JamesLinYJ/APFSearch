#!/usr/bin/env python3
"""Shared deployment target and architecture names for builds and test bundles."""
import argparse
import os
import pathlib
import platform

MINIMUM_MACOS_VERSION = '14.0'
# The linked SDK controls native AppKit appearance independently of deployment.
MINIMUM_MACOS_SDK_VERSION = '26.0'
RUST_TARGETS = {'arm64': 'aarch64-apple-darwin', 'x86_64': 'x86_64-apple-darwin'}


def rust_flags():
    """Keep compiler diagnostics useful without embedding a builder's home path.

    Cargo's encoded form preserves spaces in individual prefix-map arguments.
    Existing caller flags keep Cargo's whitespace-separated RUSTFLAGS semantics.
    More specific source mappings follow the home mapping because rustc uses the
    last matching prefix.
    """
    encoded = os.environ.get('CARGO_ENCODED_RUSTFLAGS')
    flags = encoded.split('\x1f') if encoded is not None else os.environ.get('RUSTFLAGS', '').split()
    project = pathlib.Path(__file__).resolve().parents[1]
    flags += [f'--remap-path-prefix={pathlib.Path.home()}=/build',
              f'--remap-path-prefix={project}=.']
    return '\x1f'.join(flag for flag in flags if flag)


def swift_target(architecture=None):
    architecture = architecture or platform.machine()
    if architecture not in RUST_TARGETS:
        raise ValueError(f'Unsupported macOS architecture: {architecture}')
    return f'{architecture}-apple-macos{MINIMUM_MACOS_VERSION}'


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('field', choices=['minimum_macos_version', 'swift_target', 'rust_target', 'rust_flags'])
    parser.add_argument('architecture', nargs='?', choices=RUST_TARGETS)
    args = parser.parse_args()
    if args.field == 'rust_flags':
        print(rust_flags())
    elif args.field == 'minimum_macos_version':
        print(MINIMUM_MACOS_VERSION)
    elif args.field == 'swift_target':
        print(swift_target(args.architecture))
    else:
        print(RUST_TARGETS[args.architecture or platform.machine()])
