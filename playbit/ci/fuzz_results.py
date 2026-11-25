"""Validate and publish fuzz evidence; never execute downloaded inputs or code."""
import html
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import sys

import fuzz
import publish


def validate(root):
    value = fuzz.read_json(root / 'manifest.json')
    if (set(value) != {'restore', 'baseline', 'target_triple', 'toolchain', 'targets'} or
            value['restore'] not in ('pending', 'success') or value['target_triple'] != fuzz.TRIPLE or
            not isinstance(value['toolchain'], dict) or set(value['targets']) != set(fuzz.TARGETS)):
        raise ValueError('invalid fuzz run manifest')
    fuzz.check_baseline(value['baseline'])
    if not set(value['baseline']['targets']) <= set(fuzz.TARGETS):
        raise ValueError('unknown restored fuzz target')
    if set(value['toolchain']) not in (set(), {'rustc', 'cargo_fuzz', 'channel'}):
        raise ValueError('invalid fuzz toolchain fields')
    for version in value['toolchain'].values():
        if not isinstance(version, str) or len(version) > 2048:
            raise ValueError('invalid fuzz toolchain version')
    if value['toolchain'] and (
            not re.fullmatch(r'nightly-[0-9]{4}-[0-9]{2}-[0-9]{2}', value['toolchain']['channel']) or
            not re.fullmatch(r'cargo-fuzz [0-9]+\.[0-9]+\.[0-9]+(?:-[A-Za-z0-9.]+)?', value['toolchain']['cargo_fuzz'])):
        raise ValueError('invalid installable fuzz toolchain')
    expected = {'manifest.json', 'provenance.json'}
    lock = root / 'Cargo.lock'
    if lock.exists():
        expected.add('Cargo.lock')
        if lock.is_symlink() or not lock.is_file() or lock.stat().st_size > fuzz.MAX_LOCK:
            raise ValueError('invalid fuzz lockfile')
    for target, item in value['targets'].items():
        if (set(item) != {'status', 'exit_code', 'starting_inputs', 'corpus', 'crashes'} or
                item['status'] not in fuzz.STATES or type(item['starting_inputs']) is not int or
                not 0 <= item['starting_inputs'] <= fuzz.MAX_WORKING_INPUTS or
                (item['exit_code'] is not None and (type(item['exit_code']) is not int or not -255 <= item['exit_code'] <= 255))):
            raise ValueError('invalid fuzz target outcome')
        if item['status'] == 'success' and (item['exit_code'] != 0 or value['restore'] != 'success' or
                                          not value['toolchain']):
            raise ValueError('incomplete successful fuzz evidence')
        for kind in ('corpus', 'crashes'):
            if item[kind] is None:
                continue
            name = f'{target}-{kind}.zip'
            expected.add(name)
            path = root / name
            if path.is_symlink():
                raise ValueError('fuzz archive is a symlink')
            fuzz.check_descriptor(item[kind])
            entries = fuzz.read_bundle(path, crashes=kind == 'crashes')
            if fuzz.describe(path, entries) != item[kind]:
                raise ValueError('fuzz archive metadata mismatch')
    if {path.name for path in root.iterdir()} != expected:
        raise ValueError('unexpected fuzz transfer contents')
    return value


def stage(root, products, report):
    value = validate(root)
    destination = products / 'fuzz'
    destination.mkdir(parents=True)
    for path in root.iterdir():
        if path.name != 'provenance.json':
            shutil.copyfile(path, destination / path.name)
    report['fuzz'] = value


def read_s3_json(key, destination):
    # A range bounds a misplaced/oversized metadata object before decoding it.
    try:
        publish.aws('s3api', 'get-object', '--bucket', publish.BUCKET, '--key', key,
                    '--range', f'bytes=0-{fuzz.MAX_METADATA}', str(destination))
    except subprocess.CalledProcessError as error:
        if b'(NoSuchKey)' in (error.stderr or b''):
            return None
        raise
    return fuzz.read_json(destination)


def read_s3_bundle(item, destination):
    publish.verify_object(publish.BUCKET, item['path'], item['sha256'], item['bytes'])
    publish.aws('s3api', 'get-object', '--bucket', publish.BUCKET, '--key', item['path'],
                '--range', f'bytes=0-{fuzz.MAX_ARCHIVE}', str(destination))
    entries = fuzz.read_bundle(destination)
    if fuzz.describe(destination, entries) != {key: item[key] for key in ('sha256', 'bytes', 'inputs', 'expanded_bytes')}:
        raise ValueError('stored fuzz snapshot mismatch')
    return entries


def promotion_inputs(report, target, current, incoming, root):
    restored = report['fuzz']['baseline']['targets'].get(target)
    # Replace the baseline this worker exercised, retaining discoveries promoted
    # since it restored. Unioning the entire old baseline would undo compaction.
    values = {}
    if current and current != restored:
        values = read_s3_bundle(current, root / 'current.zip')
        if restored:
            for name in read_s3_bundle(restored, root / 'restored.zip'):
                values.pop(name, None)
    values.update(read_s3_bundle(incoming, root / 'incoming.zip'))
    return values


def promote(report, run):
    # The serialized main reporter is the sole writer. PR discoveries are kept
    # with their runs, but cannot change the shared baseline for other builds.
    if (not report.get('fuzz') or run['event'] not in ('push', 'workflow_dispatch') or
            run['head_branch'] != 'main' or (run.get('head_repository') or {}).get('full_name') != publish.repository()):
        return
    source, run_id, attempt = publish.identity({
        'GITHUB_SHA': report['source_commit'], 'GITHUB_RUN_ID': report['run_id'],
        'GITHUB_RUN_ATTEMPT': report['run_attempt'],
    })
    receipt = {'source_commit': source, 'run_id': run_id, 'run_attempt': attempt}
    receipt_key = f'fuzz/promotions/{run_id}-{attempt}.json'
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        previous = read_s3_json(receipt_key, root / 'receipt.json')
        if previous is not None:
            if previous != receipt:
                raise ValueError('fuzz promotion identity mismatch')
            return
        baseline = read_s3_json(fuzz.BASELINE_KEY, root / 'latest.json')
        if baseline is None:
            raise ValueError('initialize the shared fuzz baseline before promotion')
        fuzz.check_baseline(baseline)
        # Only main promotes; remove retired targets from its shared index.
        baseline = {'targets': {target: item for target, item in baseline['targets'].items()
                                if target in fuzz.TARGETS}}
        for target, item in report['fuzz']['targets'].items():
            if (item['status'] != 'success' or not item['corpus'] or not item['crashes'] or
                    item['crashes']['inputs']):
                continue
            current = baseline['targets'].get(target)
            incoming = {**item['corpus'], 'path': f'{source}/{run_id}-{attempt}/fuzz/{target}-corpus.zip'}
            values = promotion_inputs(report, target, current, incoming, root)
            size = sum(map(len, values.values()))
            if len(values) > fuzz.MAX_INPUTS or size > fuzz.MAX_BYTES:
                message = (f'{target}: shared corpus promotion needs compaction '
                           f'({len(values)} inputs, {size} bytes); baseline unchanged. '
                           f'Run evidence retained at {incoming["path"]}.')
                print(f'::warning::{message}', flush=True)
                if os.environ.get('GITHUB_STEP_SUMMARY'):
                    with open(os.environ['GITHUB_STEP_SUMMARY'], 'a') as step:
                        step.write(message + '\n')
                continue
            archive = root / 'merged.zip'
            description = fuzz.write_bundle(archive, values)
            key = f'fuzz/corpus/{target}/{description["sha256"]}.zip'
            publish.put(archive, key)
            baseline['targets'][target] = {**description, 'path': key}
        # Publish all immutable bundles before changing the one shared manifest.
        # Reapplying a run after an interrupted update is idempotent. A receipt
        # also prevents old reporting retries from undoing later compaction.
        fuzz.write_json(root / 'latest.json', baseline)
        publish.put(root / 'latest.json', fuzz.BASELINE_KEY, False)
        fuzz.write_json(root / 'receipt.json', receipt)
        publish.put(root / 'receipt.json', receipt_key)


def reproduction(report, target, crashes=False):
    channel = report['fuzz']['toolchain']['channel']
    version = report['fuzz']['toolchain']['cargo_fuzz'].removeprefix('cargo-fuzz ')
    source = report['source_commit']
    url = f'https://ci.playbit.app/{source}/{report["run_id"]}-{report["run_attempt"]}/fuzz'
    kind = 'crashes' if crashes else 'corpus'
    lines = [
        '(', 'set -eu', f'pb_fuzz_target={target}', 'pb_fuzz_checkout=$(mktemp -d)',
        f'gh repo clone {publish.repository()} "$pb_fuzz_checkout" -- --no-checkout',
        'cd "$pb_fuzz_checkout"', f'git fetch origin {source}', f'git checkout --detach {source}',
        f'rustup toolchain install {channel} --profile minimal --component rust-src',
        f'cargo +{channel} install cargo-fuzz --version {version} --locked --root "$pb_fuzz_checkout/.fuzz-tools"',
        'export PATH="$pb_fuzz_checkout/.fuzz-tools/bin:$PATH"',
        f'curl --fail --location --proto =https "{url}/Cargo.lock" -o playbit/fuzz/Cargo.lock',
        f'curl --fail --location --proto =https "{url}/$pb_fuzz_target-{kind}.zip" -o inputs.zip',
    ]
    if crashes:
        lines += ['mkdir reproducers', 'unzip inputs.zip -d reproducers',
                  'for pb_fuzz_input in "$pb_fuzz_checkout"/reproducers/*; do',
                  f'    cargo +{channel} fuzz run --fuzz-dir playbit/fuzz --target {fuzz.TRIPLE} '
                  '--debug-assertions "$pb_fuzz_target" "$pb_fuzz_input"', 'done']
    else:
        lines += ['mkdir -p "playbit/fuzz/corpus/$pb_fuzz_target"',
                  'unzip inputs.zip -d "playbit/fuzz/corpus/$pb_fuzz_target"',
                  f'cargo +{channel} fuzz run --fuzz-dir playbit/fuzz '
                  f'--target "$(rustc +{channel} -vV | sed -n \'s/^host: //p\')" '
                  '--debug-assertions "$pb_fuzz_target" -- -max_total_time=60']
    return '\n'.join(lines + [')'])


def render(report):
    value = report.get('fuzz')
    if value is None:
        return '<h2>Fuzzing</h2><p>No fuzz evidence was uploaded; this does not establish that no crashes occurred.</p>' if 'fuzz' in report['jobs'] else ''
    body = '<h2>Fuzzing</h2>'
    if value['restore'] != 'success':
        body += '<p>Corpus restore did not complete. Targets may not have run.</p>'
    body += '<div class="fuzz-table"><table><thead><tr><th>Target</th><th>Outcome</th><th>Starting inputs</th><th>Saved inputs</th><th>Downloads</th></tr></thead><tbody>'
    for target in fuzz.TARGETS:
        item = value['targets'][target]
        status = 'interrupted' if item['status'] == 'running' else item['status']
        count = item['corpus']['inputs'] if item['corpus'] else 'unavailable'
        if item['corpus'] is None or item['crashes'] is None:
            status += '; collection incomplete'
        body += f'<tr><td><code>{target}</code></td><td>{status}</td><td>{item["starting_inputs"]}</td><td>{count}</td><td>'
        if item['corpus']:
            body += f'<a href="fuzz/{target}-corpus.zip">Corpus ZIP</a>'
        if item['crashes'] and item['crashes']['inputs']:
            body += f' · <a href="fuzz/{target}-crashes.zip">{item["crashes"]["inputs"]} reproducer(s)</a>'
        body += '</td></tr>'
    body += '</tbody></table></div><p><a href="fuzz/manifest.json">Fuzz manifest, toolchain versions and archive hashes</a>'
    has_lock = any(item['path'].endswith('/fuzz/Cargo.lock') for item in report['artifacts'])
    if has_lock:
        body += ' · <a href="fuzz/Cargo.lock" download="Cargo.lock">Cargo.lock for this run</a>'
    body += '</p><details><summary>Restored baseline snapshots</summary><ul>'
    for target, item in value['baseline']['targets'].items():
        body += f'<li>{target}: <a href="/{item["path"]}"><code>{item["sha256"]}</code></a></li>'
    if not value['baseline']['targets']:
        body += '<li>Explicit empty baseline; generated seeds only.</li>'
    body += '</ul></details>'
    body += '<h3>Use these downloads</h3><p>A corpus ZIP contains accumulated test inputs for its named target. Extract it into that target’s corpus directory to continue fuzzing from this run’s discoveries. A reproducer ZIP contains individual inputs saved when libFuzzer reported a crash, timeout, out-of-memory condition, leak, or slow input.</p>'
    body += '<p><code>Cargo.lock</code> records the exact dependency versions used by this run’s standalone fuzz workspace. It belongs at <code>playbit/fuzz/Cargo.lock</code>. The snippets below download it to that location automatically.</p>'
    if not has_lock or not value['toolchain']:
        body += '<p>The lockfile or toolchain metadata was not captured, so a complete reproduction command is unavailable for this run.</p>'
        return body
    body += '<p>The following snippet gets a complete repository checkout at this run’s exact commit, installs its recorded nightly and cargo-fuzz versions, downloads the lockfile and inputs, and runs locally. It uses a fresh temporary directory and installs cargo-fuzz inside that checkout. Prerequisites: <code>git</code>, <code>gh</code> signed in with access to this private repository, <code>rustup</code>, <code>curl</code> and <code>unzip</code>.</p>'
    example = next((target for target in fuzz.TARGETS if value['targets'][target]['corpus']), None)
    if example:
        command = reproduction(report, example)
        body += f'<h3>Continue fuzzing locally</h3><p>Copy this entire snippet. To use another target’s corpus, change the <code>pb_fuzz_target={example}</code> line to its name from the table. The archive is extracted into <code>playbit/fuzz/corpus/$pb_fuzz_target</code> before fuzzing for 60 seconds on your machine.</p><pre><code>{html.escape(command)}</code></pre>'
    for target in fuzz.TARGETS:
        item = value['targets'][target]
        if not item['crashes'] or not item['crashes']['inputs']:
            continue
        command = reproduction(report, target, crashes=True)
        body += f'<h3>Reproduce {target}</h3><p>This complete snippet gets the repository, lockfile and <a href="fuzz/{target}-crashes.zip">{target}-crashes.zip</a>, then replays the saved inputs on Linux ARM64, matching CI, stopping when an input fails. For local investigation on Apple Silicon, replace <code>--target {fuzz.TRIPLE}</code> with <code>--target aarch64-apple-darwin</code>; platform-specific behavior may differ.</p><pre><code>{html.escape(command)}</code></pre>'
        body += f'<p>Input archive: {item["crashes"]["bytes"]} bytes · SHA-256 <code>{item["crashes"]["sha256"]}</code></p>'
    return body


def initialize_baseline():
    # Explicit bootstrap, never a fallback for denied or failed downloads.
    if not publish.BUCKET:
        raise ValueError('configure CI_RESULTS_BUCKET')
    with tempfile.TemporaryDirectory() as temporary:
        path = Path(temporary) / 'latest.json'
        fuzz.write_json(path, {'targets': {}})
        checksum = publish.sha256(path)
        try:
            publish.aws('s3api', 'put-object', '--bucket', publish.BUCKET, '--key', fuzz.BASELINE_KEY,
                        '--body', str(path), '--if-none-match', '*', '--content-type', 'application/json',
                        '--metadata', 'sha256=' + checksum, '--cache-control', 'public,max-age=60')
        except subprocess.CalledProcessError:
            # An identical bootstrap retry is harmless; an existing nonempty
            # baseline must fail verification rather than being reset.
            pass
        publish.verify_object(publish.BUCKET, fuzz.BASELINE_KEY, checksum, path.stat().st_size)


if __name__ == '__main__':
    if sys.argv[1:] != ['initialize']:
        raise ValueError('expected: fuzz_results.py initialize')
    initialize_baseline()
