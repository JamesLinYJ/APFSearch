#!/usr/bin/env python3
"""Exercise production client recovery with deterministic OS boundary doubles."""
import pathlib
import subprocess
import tempfile

root = pathlib.Path(__file__).resolve().parents[1]
with tempfile.TemporaryDirectory(prefix='APFSearch-service-recovery-tests-') as temporary:
    work = pathlib.Path(temporary)
    client = work / 'SearchClient.swift'
    source = (root / 'macos/SearchClient.swift').read_text().replace('import ServiceManagement\n', '')
    boundary = '    localizedServiceResponse(jsonObject(data), bundle: bundle, locale: locale)'
    if source.count(boundary) != 1:
        raise SystemExit('Response decoder changed; update the thread-observation hook')
    source = source.replace(boundary, '    DecoderProbe.record()\n    return ' + boundary.strip())
    client.write_text(source)
    binary = work / 'ServiceRecoveryTests'
    sources = [root / 'macos' / name for name in ('ApplicationIdentity.swift', 'SearchProtocol.swift', 'Localization.swift')]
    subprocess.run(['swiftc', '-swift-version', '5', *map(str, sources), str(client), str(root / 'tests/ServiceRecoveryTests.swift'), '-o', str(binary)], check=True)
    subprocess.run([str(binary)], check=True)
