#!/usr/bin/env python3
"""Build an authenticated fixture service without touching the installed agent.

Only the Mach endpoint name differs. The production service, core, signature
validation, protocol payloads and client allowlist remain unchanged.
This script creates a launchd plist but deliberately does not bootstrap it.
"""
import argparse
import hashlib
import json
import pathlib
import plistlib
import subprocess
import uuid
from test_bundle import create_test_bundle, swift_target

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--work', type=pathlib.Path, required=True)
parser.add_argument('--library', type=pathlib.Path, required=True)
parser.add_argument('--identity', required=True)
args = parser.parse_args()
project = pathlib.Path(__file__).resolve().parents[1]
args.work.mkdir(parents=True, exist_ok=False)
work = args.work.resolve()
endpoint = 'org.apfsearch.fixture.' + uuid.uuid4().hex
source = (project / 'macos/SearchProtocol.swift').read_text()
declaration = 'let serviceName = ApplicationIdentity.serviceIdentifier'
if source.count(declaration) != 1:
    raise SystemExit('Protocol endpoint declaration changed')
protocol = work / 'SearchProtocolUnderTest.swift'
protocol.write_text(source.replace(declaration, 'let serviceName = ' + json.dumps(endpoint)))
names = ['ApplicationIdentity.swift', 'Localization.swift', 'SearchService.swift', 'ContentIndexer.swift', 'FileOperations.swift']
sources = [project / 'macos' / name for name in names] + [protocol]
binary = work / 'APFSearchService'
sdk = subprocess.check_output(['xcrun', '--sdk', 'macosx', '--show-sdk-path'], text=True).strip()
command = ['xcrun', '--sdk', 'macosx', 'swiftc', '-sdk', sdk, '-module-cache-path', str(work / 'ModuleCache'), '-swift-version', '5', '-O', '-target', swift_target()]
command += list(map(str, sources)) + [str(args.library.resolve())]
for framework in ['AppKit', 'PDFKit', 'AVFoundation', 'ImageIO', 'Security', 'DiskArbitration', 'CoreServices', 'CoreFoundation']:
    command += ['-framework', framework]
command += ['-lc++', '-o', str(binary)]
subprocess.run(command, check=True)
app = work / 'FixtureService.app'
binary = create_test_bundle(binary, app)
subprocess.run(['codesign', '--force', '--options', 'runtime', '--timestamp=none', '--identifier', 'org.apfsearch.indexer', '--sign', args.identity, str(app)], check=True)
subprocess.run(['codesign', '--verify', '--strict', str(app)], check=True)
data = work / 'data'
data.mkdir()
job = {
    'Label': endpoint, 'ProgramArguments': [str(binary)],
    'MachServices': {endpoint: True}, 'ProcessType': 'Adaptive', 'RunAtLoad': True,
    'EnvironmentVariables': {'APFSEARCH_DATA_DIR': str(data)},
    'StandardOutPath': str(work / 'service.stdout.log'),
    'StandardErrorPath': str(work / 'service.stderr.log'),
}
plist = work / (endpoint + '.plist')
plist.write_bytes(plistlib.dumps(job))
receipt = {
    'service_name': endpoint, 'plist': str(plist), 'binary': str(binary), 'data': str(data),
    'core_sha256': hashlib.sha256(args.library.read_bytes()).hexdigest(),
    'source_sha256': {path.name: hashlib.sha256(path.read_bytes()).hexdigest() for path in sources},
    'bootstrapped': False,
}
(work / 'receipt.json').write_text(json.dumps(receipt, indent=2) + '\n')
print(json.dumps(receipt, indent=2))
