#!/usr/bin/env python3
"""Verify collision-safe legacy data and preference migration in isolation."""
import json
import pathlib
import subprocess
import tempfile

PROJECT = pathlib.Path(__file__).resolve().parents[1]
with tempfile.TemporaryDirectory(prefix='FileSearch-migration-tests-') as temporary:
    temporary = pathlib.Path(temporary)
    executable = temporary / 'LegacyDataMigrationTests'
    subprocess.run(['swiftc', '-module-cache-path', str(temporary / 'ModuleCache'), '-swift-version', '5',
                    '-target', 'arm64-apple-macos15.0', str(PROJECT / 'macos/ApplicationIdentity.swift'),
                    str(PROJECT / 'macos/LegacyDataMigration.swift'), str(PROJECT / 'tests/LegacyDataMigrationTests.swift'),
                    '-o', str(executable)], check=True)
    result = subprocess.run([str(executable)], text=True, capture_output=True)
    if not result.stdout:
        raise SystemExit(result.stderr)
    report = json.loads(result.stdout)
    (PROJECT / 'validation').mkdir(parents=True, exist_ok=True)
    (PROJECT / 'validation/legacy-migration.json').write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps(report))
    raise SystemExit(result.returncode)
