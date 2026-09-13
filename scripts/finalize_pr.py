"""Integrate the reviewed feature tree in a disposable validation checkout.

The live branch is never reset. Verify the current integrated source first, then
reconstruct the reviewed delta from its immutable original baseline in this
checkout. The workflow publishes only after compilation and regression checks.
"""
from pathlib import Path
import hashlib
import json
import subprocess

BASE = '4d94e03b3e22cc2da380565d4a4c65d12dfccd92'
CURRENT = {
    'macos/Application.swift': '5fabac4fad78f67847e7d98dc42f9b8811efebfbb2e46947f0da4de01d8d0a8c',
    'macos/ContentIndexer.swift': '7cecd850eb58fbf5408180aa8f6aa8be091c4030e66ea40c4bb2505d09fe8f24',
    'core/src/lib.rs': '35db5e5782dce48f6da7b3da09e58550b2f6af4adf91d77e4b231422662f2519',
    'core/src/query.rs': '2d12eba251694fe30721809e5a653b79b129030a7e65a629bfae04637d6e9308',
    'core/src/index_store.rs': 'd4d74e2ba0b73184908475cf6f96582df185a8625373f91c03cc18b2c64caa28',
    'core/src/duplicates.rs': 'e6490e2965b774da32066bebf11cc7c38a993e31eac765ebe8bb4c82575e7103',
    'core/src/relations.rs': 'aabe4e65adcfc7039d875d8415be3e63fe550f9290940b83a5deef675ea25d59',
    'core/tests/feature_contracts.rs': '62e34385f1f165c3f0b20f28ef0c3c66e550d260539b2c70feabfe0e9017d706',
}
for name, digest in CURRENT.items():
    if hashlib.sha256(Path(name).read_bytes()).hexdigest() != digest:
        raise SystemExit('Concurrent source change requires review: ' + name)
# Public, immutable repository input; no credentials are written to source.
subprocess.run(['git', 'fetch', '--no-tags', '--depth=1', 'origin', BASE], check=True)
originals = {name: subprocess.check_output(['git', 'show', BASE + ':' + name]) for name in CURRENT}
for name, content in originals.items():
    Path(name).write_bytes(content)
for name in ('engine.patch', 'query.patch', 'index.patch', 'application-final.patch', 'property-compat.patch', 'content.patch'):
    patch = Path('scripts/feature-integration') / name
    subprocess.run(['git', 'apply', '--check', str(patch)], check=True)
    subprocess.run(['git', 'apply', str(patch)], check=True)
expected = json.loads(Path('scripts/feature-integration/expected-sources.json').read_text())
for name, digest in expected.items():
    if hashlib.sha256(Path(name).read_bytes()).hexdigest() != digest:
        raise SystemExit('Integrated source differs from reviewed digest: ' + name)
print('All reviewed source hashes match before Rust formatting')
