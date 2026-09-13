"""Integrate reviewed source patches; no network calls or branch mutations."""
from pathlib import Path
import hashlib
import json
import subprocess

for name in ('application-final.patch', 'property-compat.patch'):
    path = Path(__file__).with_name(name)
    subprocess.run(['git', 'apply', '--check', str(path)], check=True)
    subprocess.run(['git', 'apply', str(path)], check=True)

expected = json.loads(Path(__file__).with_name('expected-sources.json').read_text())
for name, digest in expected.items():
    actual = hashlib.sha256(Path(name).read_bytes()).hexdigest()
    if actual != digest:
        raise SystemExit(f'Source differs from the reviewed implementation: {name}: {actual}')
print(f'Validated {len(expected)} reviewed source digests before formatting')
