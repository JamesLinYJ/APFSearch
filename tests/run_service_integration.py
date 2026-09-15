#!/usr/bin/env python3
"""Link the two Swift integration suites to one already-built core library.

This entry point never runs Cargo or talks to the installed Mach service. It
requires the expected static-library hash and uses new disposable fixtures.
"""
from test_bundle import MINIMUM_MACOS_VERSION, swift_target
import argparse
import datetime
import hashlib
import json
import pathlib
import subprocess
import tempfile
from test_bundle import create_test_bundle

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--core-sha256', required=True)
parser.add_argument('--library', type=pathlib.Path)
parser.add_argument('--work', type=pathlib.Path)
args = parser.parse_args()
project = pathlib.Path(__file__).resolve().parents[1]
work = args.work or pathlib.Path(tempfile.mkdtemp(prefix='APFSearch-integration-'))
work.mkdir(parents=True, exist_ok=True)
# Match the scanner's canonical namespace even when tempfile returns /var.
work = work.resolve()
library = args.library or project / 'core/target/release/libapfsearch_core.a'

def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()

if sha(library) != args.core_sha256:
    raise SystemExit('Static library differs from the requested frozen revision; no tests were run.')
source_names = ['ApplicationIdentity.swift', 'SearchProtocol.swift', 'Localization.swift', 'SearchService.swift', 'ContentIndexer.swift', 'FileOperations.swift']
sources = [project / 'macos' / name for name in source_names]
source_hashes = {path.name: sha(path) for path in sources}
executables = {}
sdk = subprocess.check_output(['xcrun', '--sdk', 'macosx', '--show-sdk-path'], text=True).strip()
for name in ['ContentAndFileTests', 'ServiceTests']:
    executable = work / name
    command = ['xcrun', '--sdk', 'macosx', 'swiftc', '-sdk', sdk, '-module-cache-path', str(work / 'ModuleCache'), '-D', 'TEST_BUILD', '-swift-version', '5', '-O', '-target', swift_target()]
    command += list(map(str, sources)) + [str(project / 'tests' / (name + '.swift')), str(library)]
    for framework in ['AppKit', 'PDFKit', 'AVFoundation', 'ImageIO', 'Security', 'DiskArbitration', 'CoreServices', 'CoreFoundation']:
        command += ['-framework', framework]
    command += ['-lc++', '-o', str(executable)]
    subprocess.run(command, check=True)
    executables[name] = create_test_bundle(executable, work / (name + '.app'))
if sha(library) != args.core_sha256:
    raise SystemExit('Core library changed during linking; results cannot describe one revision.')
fixture = work / 'content-file-fixture'
content = subprocess.run([str(executables['ContentAndFileTests']), str(fixture), '-AppleLanguages', '(zh-Hans)'], text=True, capture_output=True)
(work / 'content-stderr.log').write_text(content.stderr)
(work / 'content-stdout.json').write_text(content.stdout)
content_report = json.loads(content.stdout)
if content.returncode:
    raise SystemExit(f'Content/file suite failed; inspect {work}')
service = subprocess.run([str(executables['ServiceTests']), str(project), str(fixture), str(work / 'service'), '-AppleLanguages', '(zh-Hans)'], text=True, capture_output=True)
(work / 'service-stderr.log').write_text(service.stderr)
(work / 'service-stdout.json').write_text(service.stdout)
if service.returncode:
    raise SystemExit(f'Service suite failed; inspect {work}')
service_report = json.loads(service.stdout)
if source_hashes != {path.name: sha(path) for path in sources} or sha(library) != args.core_sha256:
    raise SystemExit('Sources changed during integration; do not use this run as a frozen-revision receipt.')
stamp = datetime.datetime.now(datetime.timezone.utc).isoformat()
(project / 'validation').mkdir(parents=True, exist_ok=True)
for name, filename, report in [('ContentAndFileTests', 'content-and-files.json', content_report), ('ServiceTests', 'service.json', service_report)]:
    report.update(tested_core_staticlib_sha256=args.core_sha256, executed_at_utc=stamp,
                  test_executable_sha256=sha(executables[name]), swift_source_sha256=source_hashes,
                  test_source_sha256=sha(project / 'tests' / (name + '.swift')))
    (project / 'validation' / filename).write_text(json.dumps(report, ensure_ascii=False, indent=2) + '\n')
print(json.dumps({'success': content_report['success'] and service_report['success'], 'content_and_file_count': content_report['count'], 'service_count': service_report['count'], 'tested_core_staticlib_sha256': args.core_sha256, 'work': str(work)}, indent=2))
