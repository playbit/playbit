#!/usr/bin/env python3
"""Sign a channel-bound SDK catalog after immutable upload. Never reads keys into logs."""
import argparse
import base64
import json
from pathlib import Path
import subprocess
import tempfile
import sys
sys.path.insert(0,str(Path(__file__).resolve().parents[3]/"scripts"))
from identifier import decode

if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--key', type=Path, required=True)
    parser.add_argument('--channel', choices=('public', 'testing'), required=True)
    parser.add_argument('--sequence', type=int, required=True)
    parser.add_argument('--artifact', nargs=2, action='append', metavar=('METADATA', 'BLOB_URL'), required=True)
    parser.add_argument('--output', type=Path, required=True)
    options = parser.parse_args()
    if not 1 <= options.sequence <= 2**64-1: raise SystemExit('Sequence must fit a positive u64')
    if len(options.artifact) > 32: raise SystemExit('Catalog exceeds 32 artifacts')
    artifacts = []
    for metadata, url in options.artifact:
        value = json.loads(Path(metadata).read_text())
        if not url.startswith('https://blob.playbit.app/'): raise SystemExit('Require immutable Playbit blob URL')
        if set(value) != {'language','version','platform','driver','bytes','expanded_bytes'}:
            raise SystemExit('Unexpected SDK metadata fields')
        if value['language'] not in ('rust','ocaml') or value['platform'] != 'macos-arm64' or value['driver'] != 1 or not 0 < value['bytes'] <= 1024**3 or not 0 < value['expanded_bytes'] <= 4*1024**3:
            raise SystemExit('Unsupported SDK metadata or resource bounds')
        decode(url.removeprefix('https://blob.playbit.app/'),32)
        if not isinstance(value['version'],str) or not 1 <= len(value['version']) <= 128: raise SystemExit('Invalid SDK version')
        value['url'] = url
        artifacts.append(value)
    payload = json.dumps(dict(channel=options.channel, sequence=options.sequence, artifacts=artifacts), sort_keys=True, separators=(',', ':')).encode()
    with tempfile.TemporaryDirectory(prefix='sdk-sign-') as temporary:
        source = Path(temporary) / 'payload'; signature = Path(temporary) / 'signature'
        source.write_bytes(payload)
        subprocess.run(['openssl', 'pkeyutl', '-sign', '-rawin', '-inkey', str(options.key), '-in', str(source), '-out', str(signature)], check=True)
        envelope = dict(payload=base64.b64encode(payload).decode(), signature=base64.b64encode(signature.read_bytes()).decode())
    result = json.dumps(envelope, sort_keys=True).encode() + b'\n'
    if len(result) > 65536: raise SystemExit('Catalog exceeds 64 KiB')
    options.output.write_bytes(result)
