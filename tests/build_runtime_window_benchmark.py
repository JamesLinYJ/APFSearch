#!/usr/bin/env python3
"""Build a signed production AppKit controller harness with actual SearchClient.

Only the app entry point and automatic startup requests are suppressed. The
production history timer is invalidated immediately after it is created, outside
the measured interval, so the harness cannot add test text to service history.
"""
from test_bundle import MINIMUM_MACOS_VERSION, swift_target
import argparse
import datetime
import hashlib
import json
import os
import pathlib
import subprocess
import tempfile
from test_bundle import create_test_bundle

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--work', type=pathlib.Path)
parser.add_argument('--phase-timing', action='store_true', help='Add timing-only hooks to the disposable controller copy for separate diagnostics')
parser.add_argument('--identity', default=os.environ.get('APFSEARCH_SIGN_IDENTITY'), required=not os.environ.get('APFSEARCH_SIGN_IDENTITY'))
args = parser.parse_args()
project = pathlib.Path(__file__).resolve().parents[1]
work = args.work or pathlib.Path(tempfile.mkdtemp(prefix='APFSearch-window-benchmark-'))
work.mkdir(parents=True, exist_ok=True)
source = (project / 'macos/Application.swift').read_text()
source_hash = hashlib.sha256(source.encode()).hexdigest()
patches = [
    ('@main\nfinal class AppDelegate', 'final class AppDelegate', 'Replace only application entry point with benchmark main'),
    ('''        pollStatus()
        refreshShortcuts()
        runQuery()
''', '', 'Suppress automatic initializer requests so each measured query starts at its text notification'),
    ('                self.recordHistory()', '                self.recordHistory()\n                self.historyTimer?.invalidate() // Test-only side-effect isolation, after measured display submission.', 'Immediately cancel history persistence after production completion, outside the measured interval'),
]
if args.phase_timing:
    patches += [
        ('    func controlTextDidChange(_ obj: Notification) {', '    func controlTextDidChange(_ obj: Notification) {\n        WindowBenchmarkPhases.mark("input_handler_enter")', 'Timestamp the production input handler'),
        ('    func runQuery() {', '    func runQuery() {\n        WindowBenchmarkPhases.mark("run_query_enter")', 'Timestamp actual debounce completion and query entry'),
        ('        updateStatus(); requestPage(0)', '        updateStatus(); WindowBenchmarkPhases.mark("query_status_ready"); requestPage(0)', 'Timestamp pre-request status work'),
        ('''        SearchClient.shared.call(request) { [weak self] reply in
            let incomingLease = (reply["snapshot_lease"] as? String).map { SearchSnapshotLease(token: $0, listID: leaseListID) }
            guard let self else { return }
            self.afterResultAnimation { [weak self] in
''', '''        WindowBenchmarkPhases.mark("request_dispatch")
        SearchClient.shared.call(request) { [weak self] reply in
            WindowBenchmarkPhases.mark("reply_delivered")
            let incomingLease = (reply["snapshot_lease"] as? String).map { SearchSnapshotLease(token: $0, listID: leaseListID) }
            guard let self else { return }
            self.afterResultAnimation { [weak self] in
                WindowBenchmarkPhases.mark("reply_apply_begin")
''', 'Timestamp dispatch and decoded main-thread reply delivery without changing SearchClient'),
        ('            self.applyResultRows(rows, offset: page * self.pageSize, count: count, replaceCache: isInitialQuery, animate: false)', '            self.applyResultRows(rows, offset: page * self.pageSize, count: count, replaceCache: isInitialQuery, animate: false)\n            WindowBenchmarkPhases.mark("rows_applied")', 'Timestamp real table row and cell application'),
        ('''                self.window?.contentView?.layoutSubtreeIfNeeded(); self.table.displayIfNeeded()
                self.elapsed =''', '''                WindowBenchmarkPhases.mark("final_layout_begin")
                self.window?.contentView?.layoutSubtreeIfNeeded()
                WindowBenchmarkPhases.mark("final_layout_end")
                self.table.displayIfNeeded()
                WindowBenchmarkPhases.mark("display_submitted")
                self.elapsed =''', 'Timestamp layout and display submission boundaries'),
    ]
for old, new, description in patches:
    if source.count(old) != 1:
        raise SystemExit(f'Production boundary changed: {description}')
    source = source.replace(old, new, 1)
application = work / 'ApplicationUnderTest.swift'
application.write_text(source)
sources = [project / 'macos' / name for name in ['ApplicationIdentity.swift', 'SearchProtocol.swift', 'Localization.swift', 'SettingsWindow.swift', 'SelectionResolver.swift', 'FileOperationReview.swift', 'DuplicateResultsWindow.swift', 'UpdateManager.swift', 'UpdateUI.swift', 'SearchClient.swift']]
sources += [application, project / 'tests/RuntimeWindowBenchmark.swift']
executable = work / 'RuntimeWindowBenchmark'
command = ['swiftc', '-module-cache-path', str(work / 'ModuleCache'), '-swift-version', '5', '-O', '-target', swift_target()] + list(map(str, sources))
if args.phase_timing:
    command += ['-D', 'BENCHMARK_PHASE_TIMING']
for framework in ['AppKit', 'SwiftUI', 'ServiceManagement', 'Quartz', 'Carbon', 'CryptoKit']:
    command += ['-framework', framework]
command += ['-o', str(executable)]
subprocess.run(command, check=True)
app = work / 'RuntimeWindowBenchmark.app'
bundled_executable = create_test_bundle(executable, app)
receipt = {
    'built_at_utc': datetime.datetime.now(datetime.timezone.utc).isoformat(),
    'app_source_sha256': source_hash,
    'application_under_test_sha256': hashlib.sha256(application.read_bytes()).hexdigest(),
    'production_sources_sha256': {str(path.relative_to(project)): hashlib.sha256(path.read_bytes()).hexdigest() for path in sources if path.is_relative_to(project)},
    'test_only_transformations': [description for _, _, description in patches],
    'transport': 'Unmodified macos/SearchClient.swift, real authenticated Mach XPC',
    'phase_timing_enabled': args.phase_timing,
    'defaults': 'Unique test bundle identifier; signature identifier is org.apfsearch.cli for existing service authentication',
    'compiler_command': command,
}
(app / 'Contents/Resources/build-receipt.json').write_text(json.dumps(receipt, indent=2) + '\n')
subprocess.run(['codesign', '--force', '--options', 'runtime', '--timestamp=none', '--identifier', 'org.apfsearch.cli', '--sign', args.identity, str(app)], check=True)
subprocess.run(['codesign', '--verify', '--strict', str(app)], check=True)
print(json.dumps({'app': str(app), 'executable': str(bundled_executable), 'app_source_sha256': source_hash, 'launched': False}, indent=2))
