#!/usr/bin/env python3
"""Test structured service messages using explicit Foundation locale bundles.

Never sets AppleLanguages, changes user defaults, or restarts a running service.
"""
from test_bundle import MINIMUM_MACOS_VERSION, swift_target
import json, pathlib, plistlib, re, subprocess, tempfile
from test_bundle import create_test_bundle, catalog_languages

root = pathlib.Path(__file__).resolve().parents[1]
catalog = json.loads((root / 'Resources/Localizable.xcstrings').read_text())
catalog['strings'].update(json.loads((root / 'Resources/Features.xcstrings').read_text())['strings'])
pattern = re.compile(r'\bLT\(\s*("(?:[^"\\]|\\.)*")')
keys = set()
for source in (root / 'macos').glob('*.swift'):
    keys.update(json.loads(match.group(1)) for match in pattern.finditer(source.read_text()))
assert keys, 'No structured localization source keys found'
assert not keys - catalog['strings'].keys(), f'Missing structured keys: {keys-catalog["strings"].keys()}'
for key in keys:
    for language in catalog_languages():
        unit = catalog['strings'][key]['localizations'][language]['stringUnit']
        assert unit['state'] == 'translated' and unit['value'], (key, language)
with tempfile.TemporaryDirectory(prefix='APFSearch-wire-localization-') as tmp:
    tmp = pathlib.Path(tmp)
    resources = tmp / 'Resources'; resources.mkdir()
    for table in ['Localizable', 'Features']:
        subprocess.run(['xcrun', 'xcstringstool', 'compile', str(root/f'Resources/{table}.xcstrings'), '--output-directory', str(resources)], check=True)
    test_bundle = tmp/'Reordered.bundle'; test_bundle.mkdir()
    (test_bundle/'Info.plist').write_bytes(plistlib.dumps({'CFBundleIdentifier':'org.apfsearch.app.localized-wire-test'}))
    (test_bundle/'Localizable.strings').write_bytes(plistlib.dumps({'test.reorder':'%2$@ before %1$@', 'test.invalid.format':'%n'}, fmt=plistlib.FMT_BINARY))
    executable = tmp/'LocalizationProtocolTests'
    subprocess.run(['swiftc', '-module-cache-path', str(tmp/'ModuleCache'), '-swift-version', '5', '-target', swift_target(), str(root/'macos/ApplicationIdentity.swift'), str(root/'macos/SearchProtocol.swift'), str(root/'macos/Localization.swift'), str(root/'macos/SearchClient.swift'), str(root/'tests/LocalizationProtocolTests.swift'), '-o', str(executable)], check=True)
    executable = create_test_bundle(executable, tmp/'LocalizationProtocolTests.app')
    result = subprocess.run([str(executable), str(resources), str(test_bundle), '-AppleLanguages', '(zh-Hans)'], capture_output=True, text=True)
    report = json.loads(result.stdout)
    report['structured_catalog_source_keys'] = sorted(keys)
    report['structured_catalog_source_key_count'] = len(keys)
    report['catalog_languages_verified'] = catalog_languages()
    (root/'validation').mkdir(parents=True, exist_ok=True)
    (root/'validation/localization-protocol.json').write_text(json.dumps(report,ensure_ascii=False,indent=2)+'\n')
    print(json.dumps({'success':report['success'],'count':report['count'],'structured_keys':len(keys),'failures':report['failures']},ensure_ascii=False))
    if result.returncode: raise SystemExit(result.returncode)
