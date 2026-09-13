#!/usr/bin/env python3
"""Check local unsigned-code rejection and portable signing constraints.

Uses disposable bundles only. A successful Apple-signed XPC connection still
requires integration testing with an explicitly configured signing identity.
"""
from test_bundle import MINIMUM_MACOS_VERSION, swift_target
import importlib.util
import pathlib
import subprocess
import tempfile
import unittest
from unittest import mock

PROJECT = pathlib.Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location('build_identity', PROJECT / 'scripts/build_identity.py')
BUILD_IDENTITY = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(BUILD_IDENTITY)


class ApplicationIdentityTests(unittest.TestCase):
    def test_adhoc_service_cannot_authorize_clients(self):
        with tempfile.TemporaryDirectory(prefix='APFSearch-identity-') as directory:
            work = pathlib.Path(directory)
            bundle = work / 'IdentityTest.app'
            binaries = bundle / 'Contents/MacOS'
            binaries.mkdir(parents=True)
            executable = binaries / BUILD_IDENTITY.IDENTITY['serviceExecutable']
            source = work / 'IdentityTest.swift'
            source.write_text('''import Foundation
@main struct IdentityTest {
    static func main() {
        guard ApplicationIdentity.clientSecurityRequirement == nil else { exit(1) }
    }
}
''')
            subprocess.run(['swiftc', '-module-cache-path', str(work / 'ModuleCache'),
                            '-swift-version', '5', '-target', swift_target(),
                            str(PROJECT / 'macos/ApplicationIdentity.swift'), str(source),
                            '-o', str(executable)], check=True)
            subprocess.run(['codesign', '--force', '--sign', '-', '--identifier',
                            BUILD_IDENTITY.IDENTITY['serviceIdentifier'], str(executable)],
                           check=True, capture_output=True)
            subprocess.run([str(executable)], check=True)
            with self.assertRaisesRegex(ValueError, 'team signature'):
                BUILD_IDENTITY.verify_bundle_signatures(bundle)

    def test_different_signing_teams_are_rejected(self):
        service = subprocess.CompletedProcess([], 0, '', 'TeamIdentifier=TEAM000001\n')
        verified = subprocess.CompletedProcess([], 0)
        client = subprocess.CompletedProcess([], 0, '', 'TeamIdentifier=TEAM000002\n')
        with mock.patch.object(BUILD_IDENTITY.subprocess, 'run', side_effect=[service, verified, client]):
            with self.assertRaisesRegex(ValueError, 'same team'):
                BUILD_IDENTITY.verify_bundle_signatures(pathlib.Path('/example/IdentityTest.app'))

    def test_trust_anchor_validation_failure_is_propagated(self):
        detail = subprocess.CompletedProcess([], 0, '', 'TeamIdentifier=TEAM000001\n')
        error = subprocess.CalledProcessError(3, ['codesign', '--verify'])
        with mock.patch.object(BUILD_IDENTITY.subprocess, 'run', side_effect=[detail, error]):
            with self.assertRaises(subprocess.CalledProcessError):
                BUILD_IDENTITY.verify_bundle_signatures(pathlib.Path('/example/IdentityTest.app'))

    def test_all_signed_objects_require_trusted_team_and_fixed_identifier(self):
        detail = subprocess.CompletedProcess([], 0, '', 'TeamIdentifier=TEAM000001\n')
        with mock.patch.object(BUILD_IDENTITY.subprocess, 'run', return_value=detail) as run:
            BUILD_IDENTITY.verify_bundle_signatures(pathlib.Path('/example/IdentityTest.app'))
        requirements = [call.args[0][4] for call in run.call_args_list if '--verify' in call.args[0]]
        expected = [BUILD_IDENTITY.IDENTITY[field]
                    for field in ['serviceIdentifier', 'cliIdentifier', 'bundleIdentifier']]
        self.assertEqual(requirements, [
            f'=anchor apple generic and identifier "{identifier}" '
            'and certificate leaf[subject.OU] = "TEAM000001"' for identifier in expected])


if __name__ == '__main__':
    unittest.main()
