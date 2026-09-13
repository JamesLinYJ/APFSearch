#!/usr/bin/env python3
"""Small read-only audit and public release-configuration regression fixtures."""
import base64
import importlib.util
import json
import os
from pathlib import Path
import plistlib
import sqlite3
import sys
import tempfile
import unittest
from unittest.mock import patch

root = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(root/'scripts'))
from audit_scope import audit
from build_update_config import configure, secure_url
from compare_everything import validate_reference


class ReleaseTests(unittest.TestCase):
    def test_public_configuration_and_stale_cleanup(self):
        with tempfile.TemporaryDirectory() as temporary:
            app = Path(temporary)/'Fixture.app'
            (app/'Contents').mkdir(parents=True)
            (app/'Contents/Info.plist').write_bytes(plistlib.dumps({'CFBundleVersion': '0.1'}))
            environment = {'FILESEARCH_UPDATE_PUBLIC_KEY': base64.b64encode(bytes(range(32))).decode(),
                           'FILESEARCH_UPDATE_FEED_URL': 'https://example.com/update.json', 'FILESEARCH_RELEASE_VERSION': '1.2.3'}
            configure(app, environment)
            self.assertEqual(plistlib.loads((app/'Contents/Info.plist').read_bytes())['CFBundleVersion'], '1.2.3')
            self.assertEqual((app/'Contents/Resources/UpdatePublicKey.txt').read_text().strip(), environment['FILESEARCH_UPDATE_PUBLIC_KEY'])
            configure(app, {})
            self.assertFalse((app/'Contents/Resources/UpdatePublicKey.txt').exists())
            self.assertFalse((app/'Contents/Resources/UpdateFeedURL.txt').exists())
            for bad in [{'FILESEARCH_UPDATE_FEED_URL': 'https://example.com'},
                        {'FILESEARCH_RELEASE_VERSION': '../2'},
                        dict(environment, FILESEARCH_UPDATE_PUBLIC_KEY='short'),
                        dict(environment, FILESEARCH_UPDATE_FEED_URL='http://example.com')]:
                with self.assertRaises(ValueError): configure(app, bad)

    def test_reference_comparison_rejects_truncation_and_ambiguous_basenames(self):
        reference = {'query': 'ext:txt', 'total': 2, 'rows': [{'relative_path': 'a/same.txt'}, {'relative_path': 'b/same.txt'}]}
        self.assertEqual(validate_reference(reference), ['a/same.txt', 'b/same.txt'])
        for bad in [dict(reference, total=3), dict(reference, rows=[{'name': 'same.txt'}], total=1),
                    dict(reference, rows=[{'relative_path': '../outside'}], total=1)]:
            with self.assertRaises(ValueError): validate_reference(bad)

    def test_https_configuration_rejects_credentials_and_controls(self):
        self.assertTrue(secure_url('https://example.com/manifest.json'))
        for value in ['http://example.com', 'https://a:b@example.com', 'https://example.com:80/a',
                      'https://example.com/#fragment', 'https://example.com/\nname', 'file:///tmp/a']:
            self.assertFalse(secure_url(value), value)


class AuditTests(unittest.TestCase):
    def test_filesystem_root_is_rejected_before_database_access_or_traversal(self):
        for scope in ['/', '//', '/tmp/..', '/./']:
            with self.subTest(scope=scope), patch('audit_scope.sqlite3.connect') as connect, \
                 patch('audit_scope.os.lstat') as lstat, patch('audit_scope.os.scandir') as scandir:
                with self.assertRaisesRegex(ValueError, 'Filesystem-root'):
                    audit(Path('/audit-fixture/data/index.sqlite'), [Path(scope)])
                connect.assert_not_called()
                lstat.assert_not_called()
                scandir.assert_not_called()

    def fixture(self, temporary):
        directory = Path(temporary).resolve()
        files = (directory/'files'); files.mkdir()
        (files/'nested').mkdir(); (files/'nested'/'文件.txt').write_text('fixture')
        os.link(files/'nested'/'文件.txt', files/'alias.txt')
        (files/'directory-link').symlink_to(files/'nested', target_is_directory=True)
        data = directory/'data'; data.mkdir(); database = data/'index.sqlite'
        connection = sqlite3.connect(database)
        connection.executescript('CREATE TABLE settings(key TEXT PRIMARY KEY,value TEXT); CREATE TABLE files(path TEXT PRIMARY KEY,file_id INTEGER,size INTEGER,is_dir INTEGER,is_symlink INTEGER,modified_ns INTEGER,changed_ns INTEGER,accessible INTEGER);')
        connection.executemany('INSERT INTO settings VALUES(?,?)', [('roots', json.dumps([str(files)])), ('revision','1'), ('generation','1')])
        paths = [files, *files.rglob('*')]
        for path in paths:
            meta = path.lstat()
            connection.execute('INSERT INTO files VALUES(?,?,?,?,?,?,?,1)', (str(path), meta.st_ino, meta.st_size, int(path.is_dir() and not path.is_symlink()), int(path.is_symlink()), meta.st_mtime_ns, meta.st_ctime_ns))
        connection.commit(); connection.close()
        return database, files

    def test_independent_parity_preserves_links_and_never_writes_database(self):
        with tempfile.TemporaryDirectory() as temporary:
            database, files = self.fixture(temporary)
            before = database.read_bytes()
            result = audit(database, [files, files/'nested'])
            self.assertTrue(result['success'], result)
            self.assertEqual(result['observed_rows'], 5)
            self.assertEqual(database.read_bytes(), before)
            self.assertEqual(len(result['roots']), 1)

    def test_differences_and_limits_never_report_complete_success(self):
        with tempfile.TemporaryDirectory() as temporary:
            database, files = self.fixture(temporary)
            (files/'alias.txt').unlink(); (files/'new.txt').write_text('new')
            report = audit(database, [files])
            self.assertFalse(report['success'])
            kinds = {item['kind'] for item in report['differences']}
            self.assertIn('missing_from_index', kinds); self.assertIn('indexed_but_not_observed', kinds)
            limited = audit(database, [files], max_entries=2)
            self.assertTrue(limited['budget_exhausted']); self.assertFalse(limited['complete'])
            with self.assertRaises(ValueError): audit(database, [files.parent])

    def test_selected_symlink_is_explicitly_incomplete(self):
        with tempfile.TemporaryDirectory() as temporary:
            database, files = self.fixture(temporary)
            report = audit(database, [files/'directory-link'])
            self.assertFalse(report['complete']); self.assertTrue(report['gaps'])


if __name__ == '__main__': unittest.main()
