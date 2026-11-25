#!/usr/bin/env python3
"""Retain one exact main push range for the independent release CI gate."""
import json
import os
from pathlib import Path
import re
import subprocess
import sys

MAX_EVENT_BYTES = 8 * 1024 * 1024


def record(output):
    if os.environ['GITHUB_EVENT_NAME'] != 'push' or os.environ['GITHUB_REF'] != 'refs/heads/main':
        raise ValueError('Release CI range requires a main push')
    # Keep this producer self-contained: the release policy fingerprints its
    # entire application logic without coupling to the CI report publisher.
    source = os.environ['GITHUB_SHA']
    run, attempt = int(os.environ['GITHUB_RUN_ID']), int(os.environ['GITHUB_RUN_ATTEMPT'])
    if not re.fullmatch('[0-9a-f]{40}', source) or not 0 < run < 2**63 or not 0 < attempt < 2**63:
        raise ValueError('Invalid release CI source/run/attempt')
    checkout = subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True, timeout=30).strip()
    if checkout != source:
        raise ValueError('Release CI range checkout differs from triggering source')
    with Path(os.environ['GITHUB_EVENT_PATH']).open('rb') as stream:
        raw = stream.read(MAX_EVENT_BYTES+1)
    if len(raw) > MAX_EVENT_BYTES:
        raise ValueError('Push event exceeds release CI range limit')
    event = json.loads(raw)
    before = event.get('before')
    if (event.get('after') != source or event.get('ref') != 'refs/heads/main'
            or not isinstance(before, str) or not re.fullmatch('[0-9a-f]{40}', before)):
        raise ValueError('Push event does not identify this main range')
    output.mkdir(parents=True, exist_ok=False)
    (output/'range.json').write_text(json.dumps(dict(schema=1, source_commit=source,
            before=None if before == '0'*40 else before, run_id=run, run_attempt=attempt), sort_keys=True)+'\n')


if __name__ == '__main__':
    record(Path(sys.argv[1]))
