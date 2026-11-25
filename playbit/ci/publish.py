#!/usr/bin/env python3
"""Publish safe build summaries and immutable products, never source or logs."""
import hashlib
import html
import json
import mimetypes
import os
from pathlib import Path
import re
import subprocess
import tempfile

BUCKET = os.environ.get('CI_RESULTS_BUCKET')


def repository():
    value = os.environ['GITHUB_REPOSITORY']
    if not re.fullmatch(r'[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+', value):
        raise ValueError('invalid GitHub repository')
    return value


def aws(*args):
    return subprocess.run(['aws', *args], check=True, capture_output=True, timeout=120)


def sha256(path):
    value = hashlib.sha256()
    with path.open('rb') as source:
        for data in iter(lambda: source.read(1024 * 1024), b''):
            value.update(data)
    return value.hexdigest()


def put(path, key, immutable=True):
    if not BUCKET:
        raise ValueError('configure CI_RESULTS_BUCKET')
    digest = sha256(path)
    args = ['s3api', 'put-object', '--bucket', BUCKET, '--key', key, '--body', str(path),
            '--content-type', 'text/html; charset=utf-8' if key.endswith('/') else (mimetypes.guess_type(key)[0] or 'application/octet-stream'),
            '--metadata', 'sha256=' + digest,
            '--cache-control', 'public,max-age=31536000,immutable' if immutable else 'public,max-age=60']
    if immutable:
        args += ['--if-none-match', '*']
    try:
        aws(*args)
    except subprocess.CalledProcessError:
        if not immutable:
            raise
        # An immutable retry is valid only when the stored hash/length match.
    verify_object(BUCKET, key, digest, path.stat().st_size)
    return {'path': key, 'sha256': digest, 'bytes': path.stat().st_size}


def verify_object(bucket, key, digest, size):
    head = json.loads(aws('s3api', 'head-object', '--bucket', bucket, '--key', key).stdout)
    if head.get('Metadata', {}).get('sha256') != digest or head['ContentLength'] != size:
        raise ValueError('stored object does not match uploaded bytes')


def page(title, body):
    return '<!doctype html><html lang="en"><meta charset="utf-8"><meta name="viewport" content="width=device-width"><meta name="robots" content="noindex,nofollow"><title>' + html.escape(title) + '</title><style>body{font:16px/1.5 system-ui;max-width:72em;margin:3em auto;padding:0 1em}code{overflow-wrap:anywhere}a{color:#0759b0}li{margin:.6em 0}small{color:#555}.fuzz-table{overflow:auto}.fuzz-table table{min-width:44em}pre code{white-space:pre-wrap}table{border-collapse:collapse;width:100%}th,td{text-align:left;padding:.5em;border-bottom:1px solid #ddd}pre{overflow:auto;padding:1em;background:#f5f5f5}details{margin:1em 0}</style><h1>' + html.escape(title) + '</h1>' + body


def identity(environment):
    source = environment['GITHUB_SHA']
    run, attempt = int(environment['GITHUB_RUN_ID']), int(environment['GITHUB_RUN_ATTEMPT'])
    if not re.fullmatch('[0-9a-f]{40}', source) or min(run, attempt) <= 0:
        raise ValueError('invalid CI identity')
    return source, run, attempt


def products(root):
    from fuzz import TARGETS
    if not root.exists():
        return []
    result = []
    for path in sorted(root.rglob('*')):
        if path.is_dir():
            continue
        relative = path.relative_to(root).as_posix()
        allowed = relative in ['macos-arm64.zip', 'web-experimental.zip'] or (
            relative.startswith('web/') and len(path.relative_to(root).parts) == 2 and
            (path.suffix in ['.js', '.css', '.wasm', '.ttf', '.svg', '.json'] or path.name in ['index.html', 'LICENSE.txt', 'QUALIFICATION.txt']))
        allowed |= relative in {'web/todo.pbapp', 'web/micro-kart.pbapp', 'web/shared-note.pbapp'}
        allowed |= relative in {'fuzz/manifest.json', 'fuzz/Cargo.lock'} | {
            f'fuzz/{target}-{kind}.zip' for target in TARGETS for kind in ('corpus', 'crashes')}
        if not allowed or path.is_symlink():
            raise ValueError('unexpected public product: ' + relative)
        result.append((relative, path))
    return result


def refresh_run_page(report, pr_title):
    import fuzz_results
    source, run, attempt = identity({
        'GITHUB_SHA': report['source_commit'], 'GITHUB_RUN_ID': report['run_id'],
        'GITHUB_RUN_ATTEMPT': report['run_attempt'],
    })
    prefix = f'{source}/{run}-{attempt}/'
    status, results, artifacts = report['status'], report['jobs'], report['artifacts']
    github = 'https://github.com/' + repository()
    body = f'<p>Source <a href="{github}/commit/{source}"><code>{source}</code></a> · <a href="{github}/actions/runs/{run}">run {run}</a>, attempt {attempt}</p>'
    if report['pull_request'] is not None:
        number = int(report['pull_request'])
        if number <= 0:
            raise ValueError('invalid PR number')
        title = html.escape(pr_title)
        body += f'<p><a href="{github}/pull/{number}">PR #{number}: {title}</a></p>'
    body += '<p><strong>' + status.upper() + '</strong></p><ul>'
    body += ''.join('<li>' + name + ': ' + value + '</li>' for name, value in results.items()) + '</ul>'
    if not report['checkout_verified']:
        body += '<p>No checkout provenance was available; the source above is the triggering head commit. No build products are published.</p>'
    body += fuzz_results.render(report)
    body += '<p>Unsigned macOS development kit. Web examples are experimental; Phase 6 qualification is incomplete.</p><ul>'
    for item in artifacts:
        name = item['path'][len(prefix):]
        if name.endswith('.zip') and not name.startswith('fuzz/'):
            body += '<li><a href="' + name + '">' + name + '</a> · ' + str(item['bytes']) + ' bytes<br><small>SHA-256 <code>' + item['sha256'] + '</code></small></li>'
    body += '</ul><p><a href="result.json">Machine-readable results and all artifact hashes</a></p>'
    if any(item['path'].endswith('/web/index.html') for item in artifacts):
        body += '<p><a href="web/index.html">Open experimental web examples</a></p>'
    with tempfile.TemporaryDirectory() as temporary:
        path = Path(temporary) / 'run.html'
        path.write_text(page('Archipelago build results', body))
        # Presentation can improve without changing immutable products or evidence.
        put(path, prefix, False)


def publish(root, report):
    source, run, attempt = identity({
        'GITHUB_SHA': report['source_commit'], 'GITHUB_RUN_ID': report['run_id'],
        'GITHUB_RUN_ATTEMPT': report['run_attempt'],
    })
    prefix = f'{source}/{run}-{attempt}/'
    results = report['jobs']
    if not all(re.fullmatch('[a-z][a-z0-9-]*', name) and value in ['success', 'failure', 'cancelled', 'skipped'] for name, value in results.items()):
        raise ValueError('invalid result summary')
    status = report['status']
    if status not in ['success', 'failure', 'cancelled', 'skipped']:
        raise ValueError('invalid run status')
    artifacts = [put(path, prefix + name) for name, path in products(root)]
    summary = {key: report[key] for key in ('source_commit', 'head_commit', 'run_id', 'run_attempt',
               'status', 'workflow', 'pull_request', 'pull_request_title', 'checkout_verified', 'jobs')}
    if 'fuzz' in report:
        summary['fuzz'] = report['fuzz']
    summary.update(artifacts=artifacts, web_qualification='experimental; Phase 6 incomplete')
    with tempfile.TemporaryDirectory() as temporary:
        directory = Path(temporary)
        def write(name, text):
            path = directory / name
            path.write_text(text)
            return path
        put(write('result.json', json.dumps(summary, sort_keys=True, indent=2) + '\n'), prefix + 'result.json')
        refresh_run_page(summary, report['pull_request_title'])
        github = 'https://github.com/' + repository()
        # One serialized publisher updates the discoverable commit index; run bytes
        # remain immutable and each rerun has its own path.
        listed = json.loads(aws('s3api', 'list-objects-v2', '--bucket', BUCKET, '--prefix', source + '/', '--delimiter', '/', '--max-keys', '1000', '--no-paginate').stdout)
        if listed.get('IsTruncated'):
            raise ValueError('commit index exceeds 1,000 runs')
        runs = sorted(p['Prefix'] for p in listed.get('CommonPrefixes', []))
        links = ''
        for p in runs:
            if not re.fullmatch(source + r'/[1-9][0-9]*-[1-9][0-9]*/', p):
                raise ValueError('unexpected run prefix')
            path = directory / 'previous-result.json'
            try:
                aws('s3api', 'get-object', '--bucket', BUCKET, '--key', p + 'result.json', str(path))
            except subprocess.CalledProcessError as error:
                # A cancelled publisher may have uploaded products without its
                # final summary. It must not poison later commit indexes.
                if b'(NoSuchKey)' in (error.stderr or b''):
                    continue
                raise
            result = json.loads(path.read_text())
            label = result['status']
            if label not in ['success', 'failure', 'cancelled', 'skipped']:
                raise ValueError('unexpected build status')
            links += '<li><a href="/' + p + '">Run ' + p.split('/')[1] + '</a> · ' + label + '</li>'
        put(write('commit.html', page('Archipelago ' + source[:12], f'<p>Source <a href="{github}/commit/{source}"><code>{source}</code></a></p><ul>' + links + '</ul>')), source + '/', False)
        put(write('robots.txt', 'User-agent: *\nDisallow: /\n'), 'robots.txt', False)
        put(write('root.html', page('Archipelago builds', '<p>Build results are available at <code>/&lt;source commit&gt;/</code>.</p><p>Web examples are experimental; Phase 6 qualification is incomplete.</p>')), 'index.html', False)
    return summary
