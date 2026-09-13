#!/usr/bin/env python3
"""Embed public update configuration before signing; never embeds private keys."""
import base64
import os
from pathlib import Path
import plistlib
import re
import sys
from urllib.parse import urlsplit


def secure_url(value: str) -> bool:
    try:
        parsed = urlsplit(value)
        return (len(value.encode()) <= 4096 and not any(c in value for c in '\r\n\0')
                and parsed.scheme == 'https' and bool(parsed.hostname)
                and parsed.username is None and parsed.password is None
                and not parsed.fragment and parsed.port in (None, 443))
    except ValueError:
        return False


def configure(app: Path, environ: dict[str, str]) -> None:
    resources = app / 'Contents/Resources'
    key = environ.get('APFSEARCH_UPDATE_PUBLIC_KEY', '')
    feed = environ.get('APFSEARCH_UPDATE_FEED_URL', '')
    version = environ.get('APFSEARCH_RELEASE_VERSION', '')
    if bool(key) != bool(feed):
        raise ValueError('Set APFSEARCH_UPDATE_PUBLIC_KEY and APFSEARCH_UPDATE_FEED_URL together')
    if key and (len(base64.b64decode(key, validate=True)) != 32 or not secure_url(feed)):
        raise ValueError('Update configuration requires a 32-byte Ed25519 public key and HTTPS feed')
    if version and not re.fullmatch(r'[0-9]{1,6}(\.[0-9]{1,6}){1,3}', version):
        raise ValueError('Release version must have two to four numeric components')
    resources.mkdir(parents=True, exist_ok=True)
    # An unsigned developer rebuild must not accidentally retain a previous
    # release's feed/key from the same output directory.
    for name, value in [('UpdatePublicKey', key), ('UpdateFeedURL', feed)]:
        path = resources / (name + '.txt')
        if value:
            path.write_text(value + '\n', encoding='utf-8')
        else:
            path.unlink(missing_ok=True)
    if version:
        path = app / 'Contents/Info.plist'
        values = plistlib.loads(path.read_bytes())
        values['CFBundleShortVersionString'] = version
        path.write_bytes(plistlib.dumps(values))


if __name__ == '__main__':
    if len(sys.argv) != 2:
        raise SystemExit('usage: build_update_config.py APP_BUNDLE')
    try:
        configure(Path(sys.argv[1]), dict(os.environ))
    except (ValueError, OSError) as error:
        raise SystemExit(str(error)) from error
