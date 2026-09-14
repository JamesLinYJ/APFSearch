#!/usr/bin/env python3
"""Measure the real settings tab with an in-memory 888-path coverage fixture."""
import argparse
import pathlib
import subprocess
import tempfile
from test_bundle import swift_target, create_test_bundle

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--source', type=pathlib.Path, help='Optional baseline SettingsWindow.swift')
parser.add_argument('--output', type=pathlib.Path, required=True)
args = parser.parse_args()
root = pathlib.Path(__file__).resolve().parents[1]
with tempfile.TemporaryDirectory(prefix='APFSearch-settings-responsiveness-') as directory:
    work = pathlib.Path(directory)
    binary = work / 'SettingsResponsivenessTests'
    sources = [root / 'macos' / name for name in ('ApplicationIdentity.swift', 'SearchProtocol.swift', 'Localization.swift')]
    sources += [args.source or root / 'macos/SettingsWindow.swift', root / 'tests/SettingsResponsivenessTests.swift']
    subprocess.run(['swiftc', '-swift-version', '5', '-O', '-target', swift_target(), *map(str, sources), '-framework', 'AppKit', '-framework', 'SwiftUI', '-framework', 'ServiceManagement', '-o', str(binary)], check=True)
    executable = create_test_bundle(binary, work / 'SettingsResponsivenessTests.app')
    result = subprocess.run([str(executable), '-AppleLanguages', '(zh-Hans)'], text=True, capture_output=True, check=True)
    args.output.write_text(result.stdout)
    print(result.stdout)
