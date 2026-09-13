#!/usr/bin/env python3
"""Apply the checked-in semantic key migration once; never used at runtime.

Catalog wording remains exclusively in String Catalog locale records. The map
is a source migration artifact, not a translation dictionary for the app.
"""
import json
import pathlib
import re

ROOT = pathlib.Path(__file__).resolve().parents[1]
MAPPING = json.loads((ROOT / 'Resources/LocalizationKeyMigration.json').read_text())['old_to_new']
CALL = re.compile(r'\bL[FT]?\(\s*("(?:[^"\\]|\\.)*")')


def replace_calls(text):
    def replace(match):
        key = json.loads(match.group(1))
        if key not in MAPPING:
            return match.group()
        start, end = match.span(1)
        return match.group()[:start - match.start()] + json.dumps(MAPPING[key]) + match.group()[end - match.start():]
    return CALL.sub(replace, text)


for path in list((ROOT / 'macos').glob('*.swift')) + list((ROOT / 'tests').glob('*.swift')) + [ROOT / 'tests/check_localization.py']:
    text = replace_calls(path.read_text())
    # Explicit wire-key assertions are identifiers. Displayed strings and
    # arbitrary file metadata in other tests must remain untouched.
    if path.name == 'ServiceTests.swift':
        lines = text.splitlines(keepends=True)
        for i, line in enumerate(lines):
            if re.search(r'\["(?:key|[a-z]+_key)"\]', line):
                for old, new in MAPPING.items():
                    line = line.replace('as? String == ' + json.dumps(old, ensure_ascii=False), 'as? String == ' + json.dumps(new))
            lines[i] = line
        text = ''.join(lines)
    path.write_text(text)

catalog_path = ROOT / 'Resources/Localizable.xcstrings'
catalog = json.loads(catalog_path.read_text())
converted = {}
for key, record in catalog['strings'].items():
    stable = MAPPING.get(key, key)
    if stable in converted:
        raise SystemExit('Duplicate catalog identifier: ' + stable)
    converted[stable] = record
catalog['strings'] = dict(sorted(converted.items()))
catalog_path.write_text(json.dumps(catalog, ensure_ascii=False, indent=2) + '\n')
print(json.dumps({'catalog_identifiers': len(converted), 'runtime_mapping': False}))
