#!/usr/bin/env python3
"""Explicit, read-only lstat/scandir parity audit; never starts the indexer.

The report lists every requested root and every gap. A mutable filesystem is not
an atomic snapshot: differing/stale observations are evidence to reconcile, not
permission to remove indexed rows. No file bodies are read or changed.
"""
from __future__ import annotations
import argparse
import json
import os
from pathlib import Path
import resource
import sqlite3
import stat
import time
from urllib.parse import quote


def within(path: str, root: str) -> bool:
    return path == root or path.startswith(root.rstrip('/') + '/')


def audit(database: Path, roots: list[Path], *, max_entries: int = 100_000,
          seconds: float = 60, exclusions: list[Path] | None = None) -> dict:
    if not roots or max_entries < 1 or seconds <= 0:
        raise ValueError('Explicit roots and positive entry/time budgets are required')
    database = database.absolute()
    requested = sorted({os.path.abspath(p) for p in roots})
    if any(Path(path).parent == Path(path) for path in requested):
        raise ValueError('Filesystem-root audits are not allowed; select a bounded directory')
    def crosses_mount(path: str, ancestor: str) -> bool:
        cursor = Path(path)
        while str(cursor) != ancestor and cursor != cursor.parent:
            if os.path.ismount(cursor):
                return True
            cursor = cursor.parent
        return False
    roots_text = [p for p in requested if not any(p != parent and within(p, parent)
                  and not crosses_mount(p, parent) for parent in requested)]
    excluded = [str(database.parent)] + [os.path.abspath(p) for p in exclusions or []]
    if any(any(within(root, scope) for scope in excluded) for root in roots_text):
        raise ValueError('An audit root is inside the index data directory or an excluded scope')
    uri = 'file:' + quote(str(database), safe='/') + '?mode=ro'
    start = time.monotonic(); before = resource.getrusage(resource.RUSAGE_SELF)
    connection = sqlite3.connect(uri, uri=True, timeout=2)
    connection.row_factory = sqlite3.Row
    connection.execute('PRAGMA query_only=ON'); connection.execute('BEGIN')
    def settings() -> dict:
        return {row['key']: json.loads(row['value']) for row in connection.execute('SELECT key,value FROM settings')}
    state = settings()
    configured = state.get('roots', [])
    if not isinstance(configured, list) or not all(isinstance(root, str) for root in configured):
        configured = []
    unsupported = [root for root in roots_text if configured and not any(within(root, p) for p in configured)]
    if unsupported:
        connection.close(); raise ValueError('Requested roots are outside configured index scope: ' + ', '.join(unsupported))
    expected: dict[str, dict] = {}
    gaps: list[dict] = []
    limited = False
    # Indexed path ranges use files.path's existing unique B-tree. Prefixes are
    # lexical byte ranges, never LIKE patterns that interpret '_' or '%'.
    for root in roots_text:
        prefix = root.rstrip('/') + '/'
        query = 'SELECT path,file_id,size,is_dir,is_symlink,modified_ns,changed_ns,accessible FROM files WHERE path=? OR (path>=? AND path<?)'
        for row in connection.execute(query, (root, prefix, prefix + chr(0x10FFFF))):
            if any(within(row['path'], scope) for scope in excluded):
                continue
            if len(expected) >= max_entries or time.monotonic() - start >= seconds:
                limited = True; break
            expected[row['path']] = dict(row)
        if limited:
            break
    current: dict[str, dict] = {}
    root_reports = []
    for root in roots_text:
        count = 0; root_gaps = len(gaps)
        try:
            # A symlink in a selected root would silently audit a different tree.
            if str(Path(root).resolve(strict=True)) != root:
                gaps.append({'path': root, 'reason': 'root contains a symlink; select its canonical path'})
                root_reports.append({'root': root, 'observed': 0, 'complete': False}); continue
            root_device = os.lstat(root).st_dev
        except OSError as error:
            gaps.append({'path': root, 'reason': str(error)})
            root_reports.append({'root': root, 'observed': 0, 'complete': False}); continue
        pending = [root]
        while pending and not limited:
            if len(current) >= max_entries or time.monotonic() - start >= seconds:
                limited = True; break
            path = pending.pop()
            if any(within(path, scope) for scope in excluded):
                continue
            try:
                meta = os.lstat(path)
                is_dir = stat.S_ISDIR(meta.st_mode)
                if meta.st_dev != root_device and is_dir and os.path.ismount(path):
                    if path not in roots_text:
                        gaps.append({'path': path, 'reason': 'mount boundary; audit that configured root explicitly'})
                    # An explicit nested volume is traversed under its own root.
                    # APFS firmlinks are not directory symlinks or mount points.
                    continue
                current[path] = {'file_id': meta.st_ino, 'size': meta.st_size, 'is_dir': int(is_dir),
                                 'is_symlink': int(stat.S_ISLNK(meta.st_mode)),
                                 'modified_ns': meta.st_mtime_ns, 'changed_ns': meta.st_ctime_ns}
                count += 1
                if is_dir:
                    # Bound the stack as well as the result set for huge directories.
                    with os.scandir(path) as children:
                        for child in children:
                            if len(current) + len(pending) >= max_entries:
                                limited = True; break
                            pending.append(child.path)
            except OSError as error:
                gaps.append({'path': path, 'reason': str(error)})
        root_reports.append({'root': root, 'observed': count, 'complete': not limited and len(gaps) == root_gaps})
    differences = []
    for path, actual in current.items():
        indexed = expected.get(path)
        if indexed is None:
            differences.append({'path': path, 'kind': 'missing_from_index'}); continue
        if not indexed['accessible']:
            differences.append({'path': path, 'kind': 'accessible_but_hidden'}); continue
        fields = ['file_id', 'is_dir', 'is_symlink', 'modified_ns', 'changed_ns']
        if not actual['is_dir']:
            fields.append('size')
        mismatch = [field for field in fields if indexed[field] != actual[field]]
        if mismatch:
            differences.append({'path': path, 'kind': 'metadata_mismatch', 'fields': mismatch})
    for path, indexed in expected.items():
        if path not in current and indexed['accessible'] and not limited and not any(within(path, gap['path']) for gap in gaps):
            differences.append({'path': path, 'kind': 'indexed_but_not_observed'})
    connection.execute('COMMIT')
    after_state = settings(); connection.close()
    changed = any(state.get(key) != after_state.get(key) for key in ('revision', 'generation', 'roots', 'uncovered'))
    usage = resource.getrusage(resource.RUSAGE_SELF)
    complete = not limited and not gaps and not changed
    return {'success': complete and not differences, 'complete': complete, 'roots': root_reports,
            'requested_roots': requested, 'configured_roots': configured, 'exclusions': excluded,
            'revision': state.get('revision'), 'index_changed_during_audit': changed,
            'budget_exhausted': limited, 'indexed_rows': len(expected), 'observed_rows': len(current),
            'differences': differences, 'gaps': gaps, 'elapsed_seconds': time.monotonic() - start,
            'process_usage': {'max_rss_native_units': usage.ru_maxrss,
                              'block_inputs': usage.ru_inblock - before.ru_inblock,
                              'block_outputs': usage.ru_oublock - before.ru_oublock},
            'boundary': 'Read-only SQLite transaction and independent lstat/scandir; no file-body reads. '
                        'Metadata parity at observation times, not an atomic filesystem snapshot. '
                        'Mount/firmlink/permission gaps are explicit; process block counts are not physical SSD writes.'}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('database', type=Path)
    parser.add_argument('--root', type=Path, action='append', required=True)
    parser.add_argument('--exclude', type=Path, action='append', default=[])
    parser.add_argument('--max-entries', type=int, default=100_000)
    parser.add_argument('--seconds', type=float, default=60)
    parser.add_argument('--output', type=Path)
    args = parser.parse_args()
    report = audit(args.database, args.root, max_entries=args.max_entries, seconds=args.seconds, exclusions=args.exclude)
    encoded = json.dumps(report, ensure_ascii=False, indent=2) + '\n'
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True); args.output.write_text(encoded)
    else:
        print(encoded, end='')
    return 0 if report['success'] else (1 if report['complete'] else 2)


if __name__ == '__main__':
    raise SystemExit(main())
