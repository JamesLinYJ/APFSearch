#!/usr/bin/env python3
"""Reject old SDK slices even when their bundle metadata claims a modern build."""
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from build_toolchain import require_sdk, validate_build_commands, verify_bundle_sdk


def commands(sdk='27.0', minimum='14.0', platform='1'):
    return f'''fixture:
Load command 8
      cmd LC_BUILD_VERSION
  cmdsize 32
 platform {platform}
    minos {minimum}
      sdk {sdk}
   ntools 1
     tool 3
  version 1234.1
Load command 9
      cmd LC_MAIN
'''


class ToolchainTests(unittest.TestCase):
    def test_modern_sdk_does_not_raise_minimum_runtime(self):
        for sdk in ('26.0', '26.5', '27.0'):
            with self.subTest(sdk=sdk):
                self.assertEqual(validate_build_commands(commands(sdk)), sdk)

    def test_old_sdk_and_wrong_deployment_are_rejected(self):
        for value in (commands('15.5'), commands(minimum='27.0'), commands(platform='2'),
                      commands().replace('LC_BUILD_VERSION', 'LC_VERSION_MIN_MACOSX'),
                      commands() + commands(), commands(sdk='unknown')):
            with self.subTest(value=value), self.assertRaises(ValueError):
                validate_build_commands(value)

    def test_sdk_version_is_numeric(self):
        for sdk in ('9.0', '15.5', '', '27-beta'):
            with self.subTest(sdk=sdk), self.assertRaises(ValueError):
                require_sdk(sdk)

    def test_old_intel_slice_cannot_hide_in_universal_bundle(self):
        def fake_output(*arguments):
            if arguments[1] == 'lipo':
                return 'arm64 x86_64'
            return commands('15.5' if arguments[3] == 'x86_64' else '27.0')
        with patch('build_toolchain.output', side_effect=fake_output), \
                patch('builtins.print'), self.assertRaises(ValueError):
            verify_bundle_sdk(Path('/fixture/APFSearch.app'))


if __name__ == '__main__':
    unittest.main()
