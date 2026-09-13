#!/usr/bin/env python3
"""Compile and execute isolated Swift feature tests, never the installed app."""
from pathlib import Path
import json
import os
import subprocess
import tempfile
from test_bundle import create_test_bundle

root = Path(__file__).resolve().parents[1]
env = dict(os.environ, PCRE2_SYS_STATIC='1', MACOSX_DEPLOYMENT_TARGET='15.0')
subprocess.run(['cargo', 'build', '--locked', '--release', '--manifest-path', str(root/'core/Cargo.toml')], check=True, env=env)
with tempfile.TemporaryDirectory(prefix='FileSearch-feature-tests-') as temporary:
    work = Path(temporary)
    names = ['ApplicationIdentity', 'LegacyDataMigration', 'SearchProtocol', 'Localization',
             'SearchService', 'ContentIndexer', 'FileOperations', 'SelectionResolver', 'UpdateManager']
    command = ['swiftc', '-module-cache-path', str(work/'ModuleCache'), '-D', 'TEST_BUILD',
               '-swift-version', '5', '-O', '-target', 'arm64-apple-macos15.0']
    command += [str(root/'macos'/f'{name}.swift') for name in names]
    command += [str(root/'tests/FeatureTests.swift'), str(root/'tests/UpdateTransportFixture.swift'), str(root/'core/target/release/libfilesearch_core.a')]
    for framework in ['AppKit', 'PDFKit', 'AVFoundation', 'ImageIO', 'Security', 'DiskArbitration', 'CoreServices', 'CoreFoundation', 'CryptoKit']:
        command += ['-framework', framework]
    executable = work/'FeatureTests'
    command += ['-lc++', '-o', str(executable)]
    subprocess.run(command, check=True, env=env)
    bundled = create_test_bundle(executable, work/'FeatureTests.app')
    result = subprocess.run([str(bundled), '-AppleLanguages', '(en)'], capture_output=True, text=True, timeout=120)
    if result.stderr:
        print(result.stderr)
    if not result.stdout.strip():
        raise SystemExit(f'Feature tests returned no report, exit {result.returncode}')
    report = json.loads(result.stdout)
    directory = root/'validation'; directory.mkdir(exist_ok=True)
    (directory/'features.json').write_text(json.dumps(report, ensure_ascii=False, indent=2)+'\n')
    print(json.dumps(report, ensure_ascii=False))
    if result.returncode or not report.get('success'):
        raise SystemExit(result.returncode or 1)
