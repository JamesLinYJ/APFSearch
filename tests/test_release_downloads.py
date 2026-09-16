#!/usr/bin/env python3
"""Public artifact boundary checks, without signing credentials or user files."""
import hashlib
import json
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from validate_release import VARIANTS, release_version, validate_downloads


class DownloadTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        self.version = '1.2.3'
        self.records = []
        for variant, architectures in VARIANTS.items():
            for extension in ('zip', 'dmg'):
                name = f'APFSearch-{self.version}-{variant}.{extension}'
                data = f'Synthetic packaging fixture: {name}'.encode()
                (self.directory / name).write_bytes(data)
                self.records.append({'file': name, 'sha256': hashlib.sha256(data).hexdigest(),
                                     'bytes': len(data), 'architectures': architectures,
                                     'application_notarized': True, 'container_notarized': extension == 'dmg'})
        self.manifest = {'version': self.version, 'build': '1', 'minimum_macos_version': '14.0', 'downloads': self.records}
        self.write_metadata()

    def write_metadata(self):
        (self.directory / 'downloads.json').write_text(json.dumps(self.manifest))
        (self.directory / 'SHA256SUMS.txt').write_text(''.join(f"{record['sha256']}  {record['file']}\n" for record in self.records))

    def validate(self):
        validate_downloads(self.directory, self.version)

    def test_complete_fixture(self):
        self.validate()

    def test_invalid_tag(self):
        for tag in ('1.2.3', 'v1.2.3/secret', 'v1.2.3\n', 'v1.2', '-x'):
            with self.subTest(tag=tag), self.assertRaises(ValueError):
                release_version(tag)

    def test_unexpected_private_file(self):
        (self.directory / 'private-key.p8').write_text('synthetic private-file fixture')
        with self.assertRaises(ValueError): self.validate()

    def test_missing_variant(self):
        (self.directory / self.records[0]['file']).unlink()
        with self.assertRaises(ValueError): self.validate()

    def test_symlink_rejected(self):
        path = self.directory / self.records[0]['file']
        path.unlink()
        path.symlink_to(self.records[1]['file'])
        with self.assertRaises(ValueError): self.validate()

    def test_modified_download(self):
        with (self.directory / self.records[0]['file']).open('ab') as stream:
            stream.write(b'corruption')
        with self.assertRaises(ValueError): self.validate()

    def test_duplicate_or_external_manifest_entry(self):
        original = self.records[0].copy()
        for name in (self.records[1]['file'], '../outside.zip'):
            self.records[0]['file'] = name
            self.write_metadata()
            with self.assertRaises(ValueError): self.validate()
        self.records[0] = original

    def test_unsigned_dmg_or_wrong_architecture(self):
        self.records[1]['container_notarized'] = False
        self.write_metadata()
        with self.assertRaises(ValueError): self.validate()
        self.records[1]['container_notarized'] = True
        self.records[1]['architectures'] = ['arm64']
        self.write_metadata()
        with self.assertRaises(ValueError): self.validate()

    def test_version_drift(self):
        self.manifest['version'] = '9.9.9'
        self.write_metadata()
        with self.assertRaises(ValueError): self.validate()

    def test_checksum_list_must_be_complete(self):
        (self.directory / 'SHA256SUMS.txt').write_text('')
        with self.assertRaises(ValueError): self.validate()


if __name__ == '__main__': unittest.main()
