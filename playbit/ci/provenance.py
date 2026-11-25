#!/usr/bin/env python3
"""Record the actual checkout before tests, including a PR's synthetic merge."""
import json
import os
from pathlib import Path
import subprocess
import sys

from publish import identity


def record(output):
    source, run, attempt = identity(os.environ)
    checkout = subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip()
    if checkout != source:
        raise ValueError('checkout does not match the build source')
    event = json.loads(Path(os.environ['GITHUB_EVENT_PATH']).read_text())
    pr = event.get('pull_request')
    output.mkdir(parents=True, exist_ok=True)
    (output / 'provenance.json').write_text(json.dumps({
        'source_commit': source, 'head_commit': pr['head']['sha'] if pr else source,
        'run_id': run, 'run_attempt': attempt,
        'pull_request': pr['number'] if pr else None,
    }, sort_keys=True) + '\n')


if __name__ == '__main__':
    record(Path(sys.argv[1]))
