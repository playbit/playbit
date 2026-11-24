#!/usr/bin/env python3
"""Select CI check groups from Git inputs; validate selected job outcomes.

Python's standard library keeps planning usable before any build-tool setup.
Unknown inputs select every check.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parent.parent
GROUPS = ('linux', 'macos', 'web')
FILE_GROUPS = {
    'playbit/src/channels.json': GROUPS,
}
PATH_GROUPS = (
    ('playbit/platform/web/', ('web', 'macos')),
    ('playbit/platform/ios/', ('macos', 'web')),
    ('playbit/platform/macos/', ('macos',)),
    ('playbit/src/desktop/', ('macos',)),
    ('playbit/src/platform/ios/', ('macos',)),
    ('playbit/src/platform/web/', ('linux', 'web')),
    ('vendor/gpui/', ('macos',)),
    ('vendor/accesskit_macos/', ('macos',)),
    ('playbit/src/bin/', ('linux', 'macos')),
    ('playbit/src/client/', ('linux', 'macos')),
    ('playbit/src/engine/', ('linux', 'macos')),
    ('playbit/src/ui/desktop/', ('macos',)),
    ('playbit/src/ui/', ('linux', 'macos')),
    ('playbit/sdk/', ('linux', 'macos')),
    ('playbit/skills/', ('macos',)),
    ('guests/', ('linux', 'macos', 'web')),
    ('playbit-api/', ('linux', 'macos', 'web')),
    ('playbit/trace/', ('linux', 'macos', 'web')),
    ('playbit/kvdb/', ('linux', 'macos', 'web')),
    ('playbit/src/', ('linux', 'macos', 'web')),
)
# These callers do not implement the namespace publication/qualification core.
# Everything else that selects macOS retains the complete history workloads.
STORAGE_UNRELATED = (
    'playbit/src/desktop/', 'playbit/src/bin/', 'playbit/src/client/', 'playbit/src/ui/',
    'playbit/src/engine/', 'playbit/src/network/', 'playbit/src/runtime/',
    'playbit/src/platform/ios/', 'playbit/src/platform/web/', 'playbit/src/platform/provider/',
    'playbit/platform/', 'playbit/sdk/', 'playbit/skills/',
    'vendor/gpui/', 'vendor/accesskit_macos/',
)


def select(changed):
    """Return reasons for every group, preserving the order used by local CI."""
    selected = {group: [] for group in GROUPS}
    for path in changed if changed is not None else [None]:
        if path is None:
            affected = GROUPS
            reason = 'Full run or unavailable change history'
        elif prose(path):
            continue
        else:
            affected = FILE_GROUPS.get(path)
            if affected is None:
                affected = next((names for prefix, names in PATH_GROUPS if path.startswith(prefix)), GROUPS)
            reason = path
        for group in affected:
            selected[group].append(reason)
    return {group: {'run': bool(reasons), 'reasons': reasons} for group, reasons in selected.items()}


def storage_history(changed):
    """Skip long histories only for known consumers outside their dependencies."""
    if changed is None:
        reasons = ['Full run or unavailable change history']
    else:
        reasons = [path for path in changed if select([path])['macos']['run']
                   and not path.startswith(STORAGE_UNRELATED)]
    return {'run': bool(reasons), 'reasons': reasons}


def prose(path):
    return path.endswith('.md') and ('/' not in path or path.startswith(('docs/', 'playbit-api/')))


def git(*args):
    return subprocess.check_output(['git', '-C', str(ROOT), *args], text=True).strip()


def plan(base=None, head='HEAD', full=False):
    head = git('rev-parse', '--verify', head + '^{commit}')
    changed = None
    if not full and base:
        try:
            base = git('rev-parse', '--verify', base + '^{commit}')
            # Deletions and both sides of a rename must participate in selection.
            changed = subprocess.check_output(['git', '-C', str(ROOT), 'diff', '--name-only',
                                               '--no-renames', '-z', base + '...' + head])
            changed = changed.decode(errors='surrogateescape').split('\0')[:-1]
        except subprocess.CalledProcessError:
            print('Change history unavailable; selecting every check.', file=sys.stderr)
    return {'base': base, 'head': head, 'groups': select(changed),
            'storage_history': storage_history(changed)}


def github_outputs(value):
    groups = value['groups']
    output = {name.replace('-', '_'): str(item['run']).lower() for name, item in groups.items()}
    output['selected'] = json.dumps([name for name, item in groups.items() if item['run']])
    output['storage_history'] = str(value['storage_history']['run']).lower()
    with open(os.environ['GITHUB_OUTPUT'], 'a') as file:
        file.writelines(f'{key}={item}\n' for key, item in output.items())


def validate_results(expected, results):
    """A selected job must succeed; unselected jobs must actually be skipped."""
    failures = []
    for job, selected in expected.items():
        if not isinstance(selected, bool):
            raise ValueError(f'{job}: expected a Boolean selection')
        result = results.get(job, {}).get('result')
        wanted = 'success' if selected else 'skipped'
        if result != wanted:
            failures.append(f'{job}: expected {wanted}, got {result or "missing"}')
    if failures:
        raise ValueError(
            'CI gate failed because dependency jobs did not have the required results.\n'
            "This gate only aggregates results; inspect the listed jobs' logs for the underlying cause.\n"
            + '\n'.join(failures))


def gate():
    results = json.loads(os.environ['CI_RESULTS'])
    expected = json.loads(os.environ['CI_EXPECTED'])
    validate_results(expected, results)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='command', required=True)
    planning = sub.add_parser('plan')
    planning.add_argument('--base')
    planning.add_argument('--head', default='HEAD')
    planning.add_argument('--full', action='store_true')
    planning.add_argument('--github', action='store_true')
    sub.add_parser('gate')
    args = parser.parse_args()
    if args.command == 'gate':
        gate()
        return
    base, full = args.base, args.full
    if args.github:
        base = os.environ.get('CI_BASE')
        full = full or os.environ.get('GITHUB_EVENT_NAME') != 'pull_request'
    value = plan(base, args.head, full)
    if args.github:
        github_outputs(value)
    print(json.dumps(value, indent=4))


if __name__ == '__main__':
    try:
        main()
    except (ValueError, subprocess.CalledProcessError) as error:
        sys.exit(str(error))
