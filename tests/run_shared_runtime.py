#!/usr/bin/env python3
"""Compare small real updates, then exercise a separately signed fixture service.

Requires already-built binaries. Never modifies the installed app or its agent.
The large offline fixture tests cache-backed queries only, not SQLite recovery.
"""
import argparse
import json
import os
from pathlib import Path
import re
import statistics
import subprocess
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('baseline-binary', 'candidate-binary', 'service-receipt',
                 'window-app', 'application-bundle', 'probe', 'output'):
        parser.add_argument('--' + name, type=Path, required=True)
    args = parser.parse_args()
    # LaunchServices does not preserve this driver's working directory.
    # Both configuration and report arguments must reach the GUI as absolute paths.
    for name, value in vars(args).items():
        setattr(args, name, value.resolve())
    args.output.mkdir(parents=True, exist_ok=False, mode=0o700)
    updates = {'baseline': [], 'candidate': []}
    for run in range(5):
        for side in (('baseline', 'candidate') if run % 2 == 0 else ('candidate', 'baseline')):
            result = subprocess.run([
                str(getattr(args, side + '_binary')), '--ignored', '--exact',
                'resource_workload_tests::reconciliation_resource_workload', '--nocapture',
            ], env=os.environ | {'APFSEARCH_WORKLOAD_LAYOUT': 'directories'},
                capture_output=True, text=True, timeout=180)
            (args.output / f'updates-{run + 1}-{side}.log').write_text(result.stdout + result.stderr)
            result.check_returncode()
            value = next(json.loads(line.removeprefix('RESOURCE_RESULT '))
                         for line in result.stdout.splitlines() if line.startswith('RESOURCE_RESULT '))
            updates[side].extend(value)
    update_summary = {side: {
        'batches': len(rows), 'wall_ms_median': statistics.median(row['wall_ms'] for row in rows),
        'cpu_ms_total': sum(row['cpu_ms'] for row in rows),
        'disk_written_total': sum(row['disk_written'] for row in rows),
        'logical_writes_total': sum(row['logical_writes'] for row in rows),
    } for side, rows in updates.items()}
    (args.output / 'updates.json').write_text(json.dumps({'summary': update_summary, 'records': updates}, indent=2) + '\n')
    print(json.dumps({'updates': update_summary}), flush=True)

    receipt = json.loads(args.service_receipt.read_text())
    endpoint = receipt['service_name']
    if not re.fullmatch(r'org\.apfsearch\.fixture\.[a-z0-9.-]+', endpoint):
        raise ValueError('Refusing a non-fixture service endpoint')
    domain = f'gui/{os.getuid()}'
    target = domain + '/' + endpoint
    if subprocess.run(['launchctl', 'print', target], capture_output=True).returncode == 0:
        raise RuntimeError('Fixture service is already loaded; do not take ownership of it')
    subprocess.run(['launchctl', 'bootstrap', domain, receipt['plist']], check=True)
    pid = None
    try:
        for _ in range(100):
            status = subprocess.check_output(['launchctl', 'print', target], text=True)
            match = re.search(r'^\s*pid = (\d+)', status, re.M)
            if match:
                pid = int(match[1])
                break
            time.sleep(0.1)
        if pid is None:
            raise RuntimeError('Fixture did not start')
        identifier = json.loads((args.service_receipt.parent / 'list.json').read_text())['list_id']
        config = {
            'list_id': identifier, 'application_bundle': str(args.application_bundle),
            'runs': 30, 'warmups': 3, 'timeout': 30, 'setup_timeout': 180,
            'queries': [{'name': name, 'text': text} for name, text in [
                ('application_name', 'crossover'), ('filename', 'report'),
                ('chinese', '报告'), ('unicode', 'café'),
                ('ordinary_path', 'path:/Applications'), ('cross_boundary', 'path:.app/Contents'),
                ('size', 'size:>10mb'), ('extension', 'ext:pdf'),
            ]],
        }
        configuration = args.output / 'window-config.json'
        configuration.write_text(json.dumps(config, ensure_ascii=False, indent=2) + '\n')
        window_report = args.output / 'window.json'
        launch = subprocess.run(['open', '-n', '-W', str(args.window_app), '--args',
                                 str(configuration), str(window_report), '-AppleLanguages', '(zh-Hans)'],
                                timeout=600)
        launch.check_returncode()
        if not window_report.exists():
            raise RuntimeError('The GUI benchmark exited without a report')
        window = json.loads(window_report.read_text())
        print(json.dumps({'window_complete': window.get('complete'), 'window_success': window.get('success')}), flush=True)
        # Measurement settling only; no sleep is added to application scheduling.
        time.sleep(2)
        samples = []
        for index in range(61):
            sample = json.loads(subprocess.check_output([str(args.probe), str(pid)], text=True))
            samples.append(sample)
            if index != 60:
                time.sleep(1)
        first, last = samples[0], samples[-1]
        if len({sample['start_ticks'] for sample in samples}) != 1:
            raise RuntimeError('The measured process restarted')
        duration = last['sample_seconds'] - first['sample_seconds']
        idle = {'seconds': duration,
                'average_cpu_percent': (last['cpu_nanoseconds'] - first['cpu_nanoseconds']) / 1e9 / duration * 100,
                'disk_bytes_read': last['disk_bytes_read'] - first['disk_bytes_read'],
                'disk_bytes_written': last['disk_bytes_written'] - first['disk_bytes_written'],
                'logical_writes': last['logical_writes'] - first['logical_writes'],
                'pageins': last['pageins'] - first['pageins'],
                'resident_bytes_median': statistics.median(sample['resident_bytes'] for sample in samples),
                'physical_footprint_bytes_median': statistics.median(sample['physical_footprint_bytes'] for sample in samples),
                'scope': 'Isolated signed service with an empty live index and the large offline cache fixture; no filesystem event backlog',
                'samples': samples}
        (args.output / 'idle.json').write_text(json.dumps(idle, indent=2) + '\n')
        print(json.dumps({k: v for k, v in idle.items() if k != 'samples'}), flush=True)
    finally:
        subprocess.run(['launchctl', 'bootout', target], check=True)
        for _ in range(100):
            loaded = subprocess.run(['launchctl', 'print', target], capture_output=True).returncode == 0
            alive = pid is not None and subprocess.run([str(args.probe), str(pid)], capture_output=True).returncode == 0
            if not loaded and not alive:
                (args.output / 'cleanup.json').write_text(json.dumps({'service_unloaded': True, 'process_exited': True}) + '\n')
                break
            time.sleep(0.1)
        else:
            raise RuntimeError('Fixture shutdown was not confirmed')


if __name__ == '__main__':
    main()
