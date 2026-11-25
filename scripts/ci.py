#!/usr/bin/env python3
"""Select CI check groups from Git inputs; validate selected job outcomes.

Python's standard library keeps planning usable before any build-tool setup.
Unknown inputs select every check. The public planner has no private imports.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parent.parent
PUBLIC = ('linux', 'macos', 'web')
PRIVATE = ('style', 'kvdb-coverage', 'clippy', 'website', 'service', 'export', 'packaging', 'image', 'playbot')
# Only these orchestration modules are covered entirely by the export tests.
# Release preparation and publication inputs still select every check.
FILE_GROUPS = {
    'service/infra/release_controller.py': ('export',),
    'service/infra/test_release_controller.py': ('export',),
    'service/infra/release_state.py': ('export',),
    'service/infra/test_release_state.py': ('export',),
    # The website consumes these definitions as well as the client builds.
    'playbit/src/channels.json': ('linux', 'macos', 'web', 'style', 'clippy', 'service', 'website'),
    # Website publication signs and verifies catalogs with this generator.
    'playbit/sdk/toolchains/catalog.py': ('linux', 'macos', 'clippy', 'service', 'website'),
}
# These are check dependencies, including non-Cargo fixtures and packaging.
PATH_GROUPS = (
    # These documents are rendered and checked by website/prepare.sh.
    ('docs/manual/', ('website',)),
    ('service/playbot/', ('playbot',)),
    ('service/infra/ci/ap-build-image/', ('image', 'service')),
    ('service/website/', ('website',)),
    ('service/ap/', ('service',)),
    ('playbit/platform/web/', ('web', 'macos', 'service', 'website')),
    ('playbit/platform/ios/', ('macos', 'web', 'service', 'clippy', 'style')),
    ('playbit/platform/macos/', ('macos', 'clippy', 'packaging')),
    # Feature/target gates exclude these modules from other builds, but AP's
    # combined trace catalog consumes all playbit/src and vendor/gpui sources.
    ('playbit/src/desktop/', ('macos', 'clippy', 'style', 'service')),
    ('playbit/src/platform/ios/', ('macos', 'clippy', 'style', 'service')),
    ('playbit/src/platform/web/', ('linux', 'web', 'style', 'service')),
    ('vendor/gpui/', ('macos', 'clippy', 'service')),
    ('vendor/accesskit_macos/', ('macos', 'clippy')),
    # AP's fixtures build the process composition, including CLI/native UI.
    ('playbit/src/bin/', ('linux', 'macos', 'clippy', 'style', 'service')),
    ('playbit/src/client/', ('linux', 'macos', 'clippy', 'style', 'service')),
    ('playbit/src/engine/', ('linux', 'macos', 'clippy', 'style', 'service')),
    ('playbit/src/ui/desktop/', ('macos', 'clippy', 'style', 'service')),
    ('playbit/src/ui/', ('linux', 'macos', 'clippy', 'style', 'service')),
    ('playbit/sdk/', ('linux', 'macos', 'clippy', 'service')),
    ('playbit/skills/', ('macos',)),
    ('guests/', ('linux', 'macos', 'web', 'clippy', 'style', 'service')),
    ('playbit-api/', ('linux', 'macos', 'web', 'clippy', 'style', 'service', 'website')),
    ('playbit/trace/', ('linux', 'macos', 'web', 'clippy', 'style', 'service')),
    ('playbit/kvdb/', ('linux', 'macos', 'web', 'clippy', 'style', 'service', 'kvdb-coverage')),
    ('playbit/src/', ('linux', 'macos', 'web', 'clippy', 'style', 'service')),
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


def select(changed, private=False):
    """Return reasons for every group, preserving the order used by local CI."""
    groups = PUBLIC + PRIVATE if private else PUBLIC
    selected = {group: [] for group in groups}
    if private:
        selected['export'].append('Source classification and CI tooling always run')
    for path in changed if changed is not None else [None]:
        if path is None:
            affected = groups
            reason = 'Full run or unavailable change history'
        elif prose(path) and not path.startswith('docs/manual/'):
            continue
        else:
            affected = FILE_GROUPS.get(path)
            if affected is None:
                affected = next((names for prefix, names in PATH_GROUPS if path.startswith(prefix)), groups)
            reason = path
        for group in affected:
            if group in selected:
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
    return path.endswith('.md') and (
        '/' not in path or path.startswith(('docs/', 'playbit-api/'))
        or path == 'service/infra/ci/self-hosted/README.md')


def git(*args):
    return subprocess.check_output(['git', '-C', str(ROOT), *args], text=True).strip()


def plan(base=None, head='HEAD', full=False, private=False):
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
    return {'base': base, 'head': head, 'groups': select(changed, private),
            'storage_history': storage_history(changed)}


def github_outputs(value):
    groups = value['groups']
    output = {name.replace('-', '_'): str(item['run']).lower() for name, item in groups.items()}
    output['selected'] = json.dumps([name for name, item in groups.items() if item['run']])
    output['public'] = str(any(groups[name]['run'] for name in PUBLIC)).lower()
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
    if 'CI_EXPECTED' not in os.environ and results.get('plan', {}).get('result') != 'success':
        raise ValueError('Planning did not succeed')
    if 'CI_EXPECTED' in os.environ:
        expected = json.loads(os.environ['CI_EXPECTED'])
    else:
        selected = set(json.loads(os.environ['CI_SELECTED']))
        if selected.difference(PUBLIC + PRIVATE):
            raise ValueError('Plan contains unknown check groups')
        expected = {name: name in selected for name in PRIVATE}
        expected['test'] = bool(selected.intersection(PUBLIC))
    validate_results(expected, results)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='command', required=True)
    planning = sub.add_parser('plan')
    planning.add_argument('--base')
    planning.add_argument('--head', default='HEAD')
    planning.add_argument('--full', action='store_true')
    planning.add_argument('--private', action='store_true')
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
    value = plan(base, args.head, full, args.private)
    if args.github:
        github_outputs(value)
    print(json.dumps(value, indent=4))


if __name__ == '__main__':
    try:
        main()
    except (ValueError, subprocess.CalledProcessError) as error:
        sys.exit(str(error))
