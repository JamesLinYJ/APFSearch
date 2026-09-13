#!/usr/bin/env python3
"""Compare complete, captured Everything results with an isolated APFSearch scan.

Reference JSON is an array of {query,total,rows:[{relative_path:...}],sort:[...]}.
`total` is mandatory: a truncated reference must never pass as complete parity.
Relative paths include directories, so same-name files in different folders stay
separate. Dates and Windows-only attributes need explicit platform-specific cases.
"""
from __future__ import annotations
import argparse
from collections import Counter
import ctypes
import json
from pathlib import Path, PurePosixPath
import tempfile


def relative_path(row: dict) -> str:
    value = row.get('relative_path')
    if not isinstance(value, str):
        raise ValueError('Each captured row must include relative_path, not just a basename')
    value = value.replace('\\', '/')
    path = PurePosixPath(value)
    if path.is_absolute() or '..' in path.parts or ':' in value:
        raise ValueError('Reference paths must be relative to the shared fixture')
    return str(path)


def validate_reference(reference: dict) -> list[str]:
    if not isinstance(reference.get('query'), str) or not isinstance(reference.get('rows'), list):
        raise ValueError('Reference requires query and rows')
    if reference.get('total') != len(reference['rows']):
        raise ValueError('Missing total or truncated Everything reference')
    return [relative_path(row) for row in reference['rows']]


class Engine:
    def __init__(self, library: Path, database: Path):
        self.lib = ctypes.CDLL(str(library.resolve()))
        self.lib.filesearch_engine_open.argtypes = [ctypes.c_char_p]
        self.lib.filesearch_engine_open.restype = ctypes.c_void_p
        self.lib.filesearch_engine_call.argtypes = [ctypes.c_void_p, ctypes.c_char_p]
        self.lib.filesearch_engine_call.restype = ctypes.c_void_p
        self.lib.filesearch_engine_free_string.argtypes = [ctypes.c_void_p]
        self.lib.filesearch_engine_close.argtypes = [ctypes.c_void_p]
        self.handle = self.lib.filesearch_engine_open(str(database).encode())
        if not self.handle:
            raise RuntimeError('Cannot open isolated fixture engine')

    def close(self):
        if self.handle:
            self.lib.filesearch_engine_close(self.handle); self.handle = None

    def call(self, request: dict) -> dict:
        pointer = self.lib.filesearch_engine_call(self.handle, json.dumps(request).encode())
        if not pointer:
            raise RuntimeError('Empty engine response')
        try:
            result = json.loads(ctypes.string_at(pointer))
        finally:
            self.lib.filesearch_engine_free_string(pointer)
        if result.get('success') is not True:
            raise RuntimeError(result.get('error', 'Query failed'))
        return result

    def all_rows(self, reference: dict) -> list[dict]:
        request = {'op': 'query', 'text': reference['query'], 'offset': 0, 'limit': 1000,
                   'retain_snapshot': True, 'sort': reference.get('sort', [{'field': 'path', 'ascending': True}])}
        page = self.call(request)
        lease = page.get('snapshot_lease')
        if not isinstance(lease, str):
            raise RuntimeError('Initial query did not retain a snapshot')
        try:
            total = page['total']; generation = page['generation']; result = []
            if not isinstance(total, int) or not 0 <= total <= 100_000:
                raise RuntimeError('Comparison fixture exceeded the explicit 100,000-row budget')
            while True:
                rows = page['rows']
                if (page['generation'] != generation or page['total'] != total
                        or page['offset'] != len(result) or len(rows) != min(1000, total - len(result))):
                    raise RuntimeError('Inconsistent or truncated APFSearch page')
                result.extend(rows)
                if len(result) == total:
                    return result
                request.pop('retain_snapshot', None)
                request.update(snapshot_lease=lease, generation=generation, offset=len(result))
                page = self.call(request)
        finally:
            self.call({'op': 'release_snapshot', 'snapshot_lease': lease})


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('library', type=Path)
    parser.add_argument('fixture', type=Path)
    parser.add_argument('reference', type=Path)
    parser.add_argument('output', type=Path)
    args = parser.parse_args()
    fixture = args.fixture.resolve(strict=True)
    references = json.loads(args.reference.read_text(encoding='utf-8-sig'))
    if not references or len(references) > 1000:
        raise ValueError('Reference must contain between 1 and 1000 explicit cases')
    expected = [validate_reference(ref) for ref in references]
    comparisons = []
    with tempfile.TemporaryDirectory(prefix='FileSearch-compatibility-') as directory:
        engine = Engine(args.library, Path(directory)/'index.sqlite')
        try:
            engine.call({'op': 'scan', 'roots': [str(fixture)], 'watch': False, 'wait': True})
            for reference, wanted in zip(references, expected):
                try:
                    rows = engine.all_rows(reference)
                    actual = [str(Path(row['path']).relative_to(fixture)) for row in rows]
                    # Native collation differs across platforms. Membership is
                    # always compared; order is opt-in with an explicit common sort.
                    same = actual == wanted if reference.get('compare_order') else Counter(actual) == Counter(wanted)
                    comparisons.append({'query': reference['query'], 'pass': same, 'actual': actual, 'expected': wanted})
                except (KeyError, RuntimeError, ValueError) as error:
                    comparisons.append({'query': reference['query'], 'pass': False, 'error': str(error)})
        finally:
            engine.close()
    report = {'success': all(item['pass'] for item in comparisons), 'cases': comparisons,
              'scope': 'Only the supplied complete captured cases, not blanket Everything compatibility'}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2)+'\n')
    print(json.dumps({'passed': sum(item['pass'] for item in comparisons), 'total': len(comparisons)}))
    return 0 if report['success'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
