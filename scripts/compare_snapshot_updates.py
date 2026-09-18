#!/usr/bin/env python3
"""Alternate real-metadata update fixtures on disposable APFS database clones.

The database must be a prior consistent SQLite backup, paired with caches made
from that backup. No user files are scanned or changed by the fixture.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import statistics
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--database', type=Path, required=True)
    for side in ('baseline', 'candidate'):
        parser.add_argument('--' + side + '-binary', type=Path, required=True)
        parser.add_argument('--' + side + '-cache', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--rounds', type=int, default=5)
    args = parser.parse_args()
    if args.rounds < 1:
        parser.error('rounds must be positive')
    args.output.mkdir(parents=True, exist_ok=False, mode=0o700)
    records = {'baseline': [], 'candidate': []}
    binary_sha256 = {side: hashlib.sha256(getattr(args, side + '_binary').read_bytes()).hexdigest()
                     for side in records}
    metrics = {'baseline': [], 'candidate': []}
    for index in range(args.rounds):
        sides = ('baseline', 'candidate') if index % 2 == 0 else ('candidate', 'baseline')
        for side in sides:
            environment = os.environ | {
                'APFSEARCH_ACCEPTANCE_DATABASE': str(args.database.resolve()),
                'APFSEARCH_ACCEPTANCE_CACHE': str(getattr(args, side + '_cache').resolve()),
            }
            result = subprocess.run([
                str(getattr(args, side + '_binary').resolve()), '--ignored', '--exact',
                'compact_acceptance_tests::database_update_lifecycle_profile', '--nocapture',
            ], env=environment, text=True, capture_output=True, timeout=900)
            log = args.output / f'{index + 1}-{side}.log'
            log.write_text(result.stdout + result.stderr)
            if result.returncode:
                raise RuntimeError(f'Update fixture failed; inspect {log}')
            record = next(json.loads(line) for line in result.stdout.splitlines() if line.startswith('{"'))
            records[side].append(record)
            # Exclude the optional structural inventory traversal from timing.
            before, after = record['rounds'][0]['before'], record['rounds'][-1]['after']
            values = {key: after[key] - before[key] for key in (
                'cpu_seconds', 'disk_bytes_read', 'disk_bytes_written', 'logical_writes', 'pageins')}
            values |= {key: after[key] for key in (
                'resident_bytes', 'physical_footprint_bytes', 'peak_physical_footprint_bytes')}
            values['elapsed_ms'] = sum(row['elapsed_ms'] for row in record['rounds'])
            # Keep the close/reopen counters: delayed writes are not omitted.
            values['writes_through_reopen'] = record['after_reopen']['disk_bytes_written'] - before['disk_bytes_written']
            metrics[side].append(values)
            print(json.dumps({'round': index + 1, 'side': side, 'entries': record['entries'], **values}), flush=True)
    if len({row['entries'] for runs in records.values() for row in runs}) != 1:
        raise AssertionError('The baseline and candidate must contain the same row count')
    medians = {side: {key: statistics.median(row[key] for row in rows) for key in rows[0]}
               for side, rows in metrics.items()}
    report = {'rounds': args.rounds, 'changes_per_round': 'Ten rename/restore cycles, window leases and a held export',
              'binary_sha256': binary_sha256,
              'scope': 'SQLite, production snapshot updates and persistence; excludes filesystem reconciliation and XPC',
              'medians': medians, 'records': records}
    (args.output / 'comparison.json').write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps({'medians': medians}), flush=True)


if __name__ == '__main__':
    main()
