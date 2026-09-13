#!/usr/bin/env python3
"""Package a signed APFSearch bundle for Xcode's Developer ID distribution flow.

This preserves the signed app and reads only its public signing metadata. It
does not retrieve credentials, change signing identities, or submit to Apple.
"""
import argparse
import datetime
import hashlib
import pathlib
import plistlib
import re
import subprocess
import tempfile

from build_identity import IDENTITY, verify_bundle_signatures


def create_archive(application, destination):
    application = application.resolve()
    verify_bundle_signatures(application)
    subprocess.run(['codesign', '--verify', '--deep', '--strict', str(application)], check=True)
    info = plistlib.loads((application / 'Contents/Info.plist').read_bytes())
    detail = subprocess.run(['codesign', '--display', '--verbose=4', str(application)],
                            check=True, capture_output=True, text=True).stderr
    team = re.search(r'^TeamIdentifier=(.+)$', detail, re.MULTILINE).group(1)
    signer = re.search(r'^Authority=(.+)$', detail, re.MULTILINE).group(1)
    if not signer.startswith('Developer ID Application:'):
        raise ValueError('Direct distribution requires a Developer ID Application signature')
    executable = application / 'Contents/MacOS' / IDENTITY['applicationExecutable']
    architectures = subprocess.check_output(['xcrun', 'lipo', '-archs', str(executable)], text=True).split()
    with tempfile.TemporaryDirectory(prefix='APFSearch-certificate-') as directory:
        prefix = str(pathlib.Path(directory) / 'certificate')
        subprocess.run(['codesign', '--display', '--extract-certificates=' + prefix, str(application)],
                       check=True, capture_output=True)
        fingerprint = hashlib.sha1(pathlib.Path(prefix + '0').read_bytes()).hexdigest().upper()

    # Never replace an earlier archive, submission record, or exported release.
    destination.mkdir(parents=True, exist_ok=False)
    archive = destination / (IDENTITY['applicationExecutable'] + '.xcarchive')
    archived_app = archive / 'Products/Applications' / application.name
    archived_app.parent.mkdir(parents=True)
    subprocess.run(['ditto', str(application), str(archived_app)], check=True)
    subprocess.run(['codesign', '--verify', '--deep', '--strict', str(archived_app)], check=True)
    metadata = {
        'ArchiveVersion': 2, 'CreationDate': datetime.datetime.utcnow(),
        'Name': IDENTITY['applicationExecutable'], 'SchemeName': IDENTITY['applicationExecutable'],
        'ApplicationProperties': {
            'ApplicationPath': 'Applications/' + application.name,
            'CFBundleIdentifier': info['CFBundleIdentifier'],
            'CFBundleShortVersionString': info['CFBundleShortVersionString'],
            'CFBundleVersion': info['CFBundleVersion'], 'Architectures': architectures,
            'SigningIdentity': signer, 'Team': team,
        },
    }
    (archive / 'Info.plist').write_bytes(plistlib.dumps(metadata))
    options = {'method': 'developer-id', 'destination': 'upload', 'teamID': team,
               'signingStyle': 'manual', 'signingCertificate': fingerprint, 'stripSwiftSymbols': False}
    (destination / 'ExportOptions.plist').write_bytes(plistlib.dumps(options))
    print(archive)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('application', type=pathlib.Path)
    parser.add_argument('destination', type=pathlib.Path, help='New directory for archive and export options')
    args = parser.parse_args()
    create_archive(args.application, args.destination)
