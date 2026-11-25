#!/usr/bin/env python3
"""Publish completed CI runs from trusted main; never execute downloaded products."""
from collections import deque
from datetime import datetime, timedelta
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import stat
import struct
import subprocess
import sys
import tempfile
import urllib.error
import urllib.parse
import urllib.request
import zipfile

import publish
import fuzz
import fuzz_results

BUILDS = '.github/workflows/playbit.yml'
MAIN_CHECKS = '.github/workflows/main-checks.yml'
WORKFLOWS = {BUILDS, MAIN_CHECKS}
MAX_ARCHIVE = 256 * 1024 * 1024
MAX_PRODUCTS = 512 * 1024 * 1024
FAILURES = {'failure', 'timed_out', 'startup_failure'}
MAX_JOB_LOG = 16 * 1024 * 1024
MAX_DIAGNOSTIC_LINE = 500
MAX_LOG_LINE = 4096
MAX_SLACK_TEXT = 35000
MAX_FAILED_STEPS = 1000
# Member IDs are from the Playbit workspace; webhooks need IDs for real mentions.
SLACK_USERS = {
    'forloveofcats': 'U081EJRCWTU',  # Julia.
    'edward-playbit': 'U05UNAU39M4',  # Edward.
    'rsms': 'U04CSF7GQR4',  # Rasmus.
}


def api(path, method='GET', body=None):
    command = ['gh', 'api', 'repos/' + publish.repository() + '/' + path, '--method', method]
    if body is not None:
        command += ['--input', '-']
    result = subprocess.run(command, input=json.dumps(body) if body is not None else None,
                            capture_output=True, text=True, check=True, timeout=30)
    if len(result.stdout) > 8 * 1024 * 1024:
        raise ValueError('GitHub response exceeds limit')
    return json.loads(result.stdout) if result.stdout else None


def pages(path, key=None):
    result = []
    for page in range(1, 11):
        response = api(path + ('&' if '?' in path else '?') + f'per_page=100&page={page}')
        items = response[key] if key else response
        result.extend(items)
        if len(items) < 100:
            return result
    raise ValueError('GitHub pagination exceeds limit')


def outcome(value):
    return value if value in ('success', 'cancelled', 'skipped') else 'failure'


def read_object(bucket, key, path):
    try:
        publish.aws('s3api', 'get-object', '--bucket', bucket, '--key', key, str(path))
    except subprocess.CalledProcessError as error:
        if b'(NoSuchKey)' in (error.stderr or b''):
            return None
        raise
    if path.stat().st_size > 64 * 1024:
        raise ValueError('result metadata exceeds limit')
    return json.loads(path.read_text())


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, request, fp, code, message, headers, url):
        return None


def download(artifact, root):
    if artifact['expired'] or artifact['size_in_bytes'] > MAX_ARCHIVE:
        raise ValueError('artifact expired or exceeds transfer limit')
    request = urllib.request.Request(
        f"https://api.github.com/repos/{publish.repository()}/actions/artifacts/{int(artifact['id'])}/zip",
        headers={'Authorization': 'Bearer ' + os.environ['GH_TOKEN'], 'Accept': 'application/vnd.github+json'})
    # Do not forward the GitHub token to the artifact storage redirect.
    try:
        urllib.request.build_opener(NoRedirect).open(request, timeout=30)
    except urllib.error.HTTPError as response:
        if response.code != 302:
            raise RuntimeError('GitHub artifact download failed') from None
        location = response.headers['Location']
        response.close()
    else:
        raise ValueError('expected artifact storage redirect')
    if urllib.parse.urlsplit(location).scheme != 'https':
        raise ValueError('artifact storage must use HTTPS')
    with tempfile.TemporaryFile() as archive:
        with urllib.request.urlopen(location, timeout=30) as response:
            size = 0
            while chunk := response.read(1024 * 1024):
                size += len(chunk)
                if size > MAX_ARCHIVE:
                    raise ValueError('artifact exceeds transfer limit')
                archive.write(chunk)
        archive.seek(0)
        extract(archive, root)


def extract(archive, root):
    # Bound the central directory before ZipFile allocates its entry objects.
    archive.seek(0, 2)
    size = archive.tell()
    archive.seek(max(0, size - 65557))
    tail = archive.read()
    offset = tail.rfind(b'PK\x05\x06')
    if offset < 0 or len(tail) - offset < 22:
        raise ValueError('missing ZIP directory')
    _, disk, directory_disk, disk_count, count, directory_bytes, _, comment = struct.unpack_from('<4s4H2IH', tail, offset)
    if (disk or directory_disk or disk_count != count or count > 256 or
            directory_bytes > 1024 * 1024 or offset + 22 + comment != len(tail)):
        raise ValueError('artifact directory exceeds limit')
    archive.seek(0)
    with zipfile.ZipFile(archive) as zipped:
        entries = zipped.infolist()
        if len(entries) > 256 or sum(i.file_size for i in entries) > MAX_PRODUCTS:
            raise ValueError('artifact exceeds extraction limit')
        for item in entries:
            path = PurePosixPath(item.filename)
            mode = item.external_attr >> 16
            if (path.is_absolute() or '..' in path.parts or '\\' in item.filename or
                    stat.S_ISLNK(mode) or (stat.S_IFMT(mode) and not (stat.S_ISREG(mode) or stat.S_ISDIR(mode)))):
                raise ValueError('unsafe artifact path')
            target = root.joinpath(*path.parts)
            if item.is_dir():
                target.mkdir(parents=True, exist_ok=True)
            else:
                target.parent.mkdir(parents=True, exist_ok=True)
                with zipped.open(item) as source, target.open('xb') as output:
                    shutil.copyfileobj(source, output, 1024 * 1024)


def validate_provenance(value, run, *, reused_build=False):
    source, run_id, attempt = publish.identity({
        'GITHUB_SHA': value['source_commit'], 'GITHUB_RUN_ID': value['run_id'],
        'GITHUB_RUN_ATTEMPT': value['run_attempt'],
    })
    # Only export provenance may be reused by a later consumer attempt.
    # Per-attempt fuzz evidence must still match the exact attempt.
    attempt_matches = attempt <= run['run_attempt'] if reused_build else attempt == run['run_attempt']
    if (run_id, value['head_commit']) != (run['id'], run['head_sha']) or not attempt_matches:
        raise ValueError('artifact belongs to another build')
    if run['event'] == 'pull_request':
        parents = api('git/commits/' + source)['parents']
        if len(parents) != 2 or parents[1]['sha'] != run['head_sha']:
            raise ValueError('artifact is not the tested PR merge')
    elif source != run['head_sha'] or value['pull_request'] is not None:
        raise ValueError('artifact source differs from the triggering commit')
    return value


def pull_request(run, number=None):
    if run['event'] != 'pull_request':
        return None
    candidates = [api('pulls/' + str(int(number)))] if number else api('commits/' + run['head_sha'] + '/pulls')
    for pr in candidates:
        if (pr['base']['repo']['full_name'] == publish.repository() and pr['head']['ref'] == run['head_branch'] and
                pr['head']['repo'] and pr['head']['repo']['full_name'] == (run.get('head_repository') or {}).get('full_name')):
            return pr['number']
    if number:
        raise ValueError('provenance PR does not match the triggering run')
    return None


def pull_request_title(number):
    return api(f'pulls/{int(number)}')['title'] if number is not None else None


def subject(run):
    # Branch and commit subject for notifications only; never published.
    message = (run.get('head_commit') or {}).get('message') or ''
    line = message.split('\n', 1)[0].strip()
    return {'branch': run.get('head_branch') or '', 'event': run['event'], 'commit': line[:200]}


def verify_products(summary):
    for artifact in summary['artifacts']:
        publish.verify_object(publish.BUCKET, artifact['path'], artifact['sha256'], artifact['bytes'])


def cleanup(artifacts, summary):
    # No timing logs or unrelated artifacts are removed.
    attempt = summary['run_attempt']
    removable = set()
    # Keep immutable source/provenance artifacts for failed-job retries. GitHub's
    # one-day retention bounds their lifetime; neither is publicly published.
    if summary['artifacts']:
        removable.add(f'public-client-products-{attempt}')
    if summary.get('fuzz') and summary['checkout_verified']:
        removable.add(f'fuzz-evidence-{attempt}')
    for artifact in artifacts:
        if artifact['name'] in removable:
            api(f"actions/artifacts/{int(artifact['id'])}", 'DELETE')


def prepare(output):
    run_id, attempt = int(os.environ['CI_RUN_ID']), int(os.environ['CI_RUN_ATTEMPT'])
    if min(run_id, attempt) <= 0:
        raise ValueError('invalid run identity')
    run = api(f'actions/runs/{run_id}/attempts/{attempt}')
    if (run['id'], run['run_attempt'], run['status']) != (run_id, attempt, 'completed') or run['path'] not in WORKFLOWS:
        raise ValueError('not a completed supported CI attempt')
    if run['path'] == MAIN_CHECKS and (run['event'] != 'push' or run['head_branch'] != 'main'):
        raise ValueError('main checks must be a main branch push')
    if run['repository']['full_name'] != publish.repository() or not re.fullmatch('[0-9a-f]{40}', run['head_sha']):
        raise ValueError('unexpected repository or source')
    report = {'source_commit': run['head_sha'], 'head_commit': run['head_sha'],
              'run_id': run_id, 'run_attempt': attempt, 'status': outcome(run['conclusion']),
              'workflow': run['name'], 'pull_request': pull_request(run),
              'pull_request_title': None, 'checkout_verified': False, 'jobs': {}}
    report['pull_request_title'] = pull_request_title(report['pull_request'])
    # Preserve enough context to send an honest failure notice if publication fails.
    output.write_text(json.dumps({'report': report, 'url': None, 'subject': subject(run), 'conclusion': run['conclusion']}))
    return run, report


def publish_run(output):
    run, report = prepare(output)
    run_id, attempt = run['id'], run['run_attempt']
    artifacts = pages(f'actions/runs/{run_id}/artifacts', 'artifacts')
    def artifact(name):
        matches = [a for a in artifacts if a['name'] == name]
        if len(matches) > 1:
            raise ValueError('duplicate transfer artifact')
        return matches[0] if matches else None
    with tempfile.TemporaryDirectory() as temporary:
        directory = Path(temporary)
        pointer_key = f'runs/{run_id}-{attempt}.json'
        pointer = read_object(publish.BUCKET, pointer_key, directory / 'pointer.json')
        if pointer:
            source = pointer['source_commit']
            if not re.fullmatch('[0-9a-f]{40}', source):
                raise ValueError('invalid stored source')
            summary = read_object(publish.BUCKET, f'{source}/{run_id}-{attempt}/result.json', directory / 'result.json')
            if not summary or (summary['source_commit'], summary['run_id'], summary['run_attempt'], summary['head_commit']) != (source, run_id, attempt, run['head_sha']):
                raise ValueError('stored run identity mismatch')
            verify_products(summary)
            publish.refresh_run_page(summary, pull_request_title(summary['pull_request']))
        else:
            products = directory / 'products'
            # A partial rerun keeps the last successful producer's inputs. A
            # later full rerun must not change an earlier attempt's report.
            producer_attempts = [int(match[1]) for item in artifacts
                                 if (match := re.fullmatch(r'build-provenance-([1-9][0-9]*)', item['name']))
                                 and int(match[1]) <= attempt]
            provenance = artifact(f'build-provenance-{max(producer_attempts)}') if producer_attempts and run['path'] == BUILDS else None
            # Fork outputs never enter the public bucket or a privileged process.
            trusted = (run.get('head_repository') or {}).get('full_name') == publish.repository()
            if provenance and trusted:
                download(provenance, directory / 'provenance')
                path = directory / 'provenance/provenance.json'
                if path.stat().st_size > 64 * 1024:
                    raise ValueError('provenance exceeds limit')
                value = validate_provenance(json.loads(path.read_text()), run, reused_build=True)
                report.update(source_commit=value['source_commit'], checkout_verified=True,
                              pull_request=pull_request(run, value['pull_request']))
                report['pull_request_title'] = pull_request_title(report['pull_request'])
            fuzz_artifact = artifact(f'fuzz-evidence-{attempt}') if run['path'] == MAIN_CHECKS else None
            fuzz_root = directory / 'fuzz'
            if fuzz_artifact and trusted:
                download(fuzz_artifact, fuzz_root)
                value = validate_provenance(fuzz.read_json(fuzz_root / 'provenance.json'), run)
                if report['checkout_verified'] and report['source_commit'] != value['source_commit']:
                    raise ValueError('fuzz and build checkout provenance differ')
                report.update(source_commit=value['source_commit'], checkout_verified=True,
                              pull_request=pull_request(run, value['pull_request']))
                report['pull_request_title'] = pull_request_title(report['pull_request'])
            product = artifact(f'public-client-products-{attempt}') if run['path'] == BUILDS else None
            if product and trusted:
                if not report['checkout_verified']:
                    raise ValueError('refusing products without exact checkout provenance')
                download(product, products)
            if fuzz_artifact and trusted:
                fuzz_results.stage(fuzz_root, products, report)
            jobs = pages(f'actions/runs/{run_id}/attempts/{attempt}/jobs', 'jobs')
            for job in jobs:
                name = re.sub('[^a-z0-9]+', '-', job['name'].lower()).strip('-')
                if not name or len(name) > 128 or name in report['jobs']:
                    raise ValueError('ambiguous job identity')
                report['jobs'][name] = outcome(job['conclusion'])
            summary = publish.publish(products, report)
            pointer_file = directory / 'pointer.json'
            pointer_file.write_text(json.dumps({'source_commit': summary['source_commit']}) + '\n')
            publish.put(pointer_file, pointer_key)
        fuzz_results.promote(summary, run)
        url = f"https://ci.playbit.app/{summary['source_commit']}/{run_id}-{attempt}/"
        output.write_text(json.dumps({'report': summary, 'url': url, 'subject': subject(run), 'conclusion': run['conclusion']}))
        # Failure before this point retains all GitHub transfer copies for recovery.
        cleanup(artifacts, summary)
        print(url)
        if os.environ.get('GITHUB_STEP_SUMMARY'):
            with open(os.environ['GITHUB_STEP_SUMMARY'], 'a') as step:
                step.write(f"[{summary['status'].upper()} — build results and downloads]({url})\n")


def github_comment(report, body):
    run, attempt = report['run_id'], report['run_attempt']
    marker = f'<!-- archipelago-ci:{run}:{attempt} -->'
    body = body + '\n\n' + marker
    pr = report['pull_request']
    path = f'issues/{pr}/comments' if pr else f"commits/{report['head_commit']}/comments"
    for comment in pages(path):
        if comment['user']['login'] == 'github-actions[bot]' and marker in comment['body']:
            endpoint = f"issues/comments/{comment['id']}" if pr else f"comments/{comment['id']}"
            return api(endpoint, 'PATCH', {'body': body})
    return api(path, 'POST', {'body': body})


def slack_escape(text):
    return text.replace('&', '&amp;').replace('<', '&lt;').replace('>', '&gt;')


def slack_subject(report, subject):
    github = 'https://github.com/' + publish.repository()
    branch = slack_escape(subject.get('branch') or report['head_commit'][:12])
    commit = slack_escape(subject.get('commit') or report['head_commit'][:12])
    return f"[{branch}] <{github}/commit/{report['head_commit']}|{commit}>"


def slack_message(text, *, url=None):
    url = (os.environ.get('SLACK_WEBHOOK_URL', '') if url is None else url).strip()
    parsed = urllib.parse.urlsplit(url)
    if parsed.scheme != 'https' or parsed.hostname != 'hooks.slack.com' or not parsed.path.startswith('/services/'):
        raise ValueError('configure the SLACK_WEBHOOK_URL repository secret')
    request = urllib.request.Request(url, data=json.dumps({'text': text, 'unfurl_links': False,
                                    'unfurl_media': False}).encode(), headers={'Content-Type': 'application/json'})
    try:
        with urllib.request.build_opener(NoRedirect).open(request, timeout=15) as response:
            if response.status != 200 or response.read(4096) != b'ok':
                raise RuntimeError('Slack did not acknowledge the CI result')
    except (urllib.error.URLError, TimeoutError):
        # Exception URLs may contain the webhook secret. Never print them.
        raise RuntimeError('Slack delivery failed; check the channel webhook') from None


def job_log(job_id):
    request = urllib.request.Request(
        f"https://api.github.com/repos/{publish.repository()}/actions/jobs/{int(job_id)}/logs",
        headers={'Authorization': 'Bearer ' + os.environ['GH_TOKEN'], 'Accept': 'application/vnd.github+json'})
    # The signed storage URL must never receive the GitHub token.
    try:
        urllib.request.build_opener(NoRedirect).open(request, timeout=30)
    except urllib.error.HTTPError as response:
        if response.code != 302:
            raise RuntimeError('GitHub job log unavailable') from None
        location = response.headers['Location']
        response.close()
    else:
        raise ValueError('expected job log storage redirect')
    if urllib.parse.urlsplit(location).scheme != 'https':
        raise ValueError('job log storage must use HTTPS')
    with urllib.request.urlopen(location, timeout=30) as response:
        data = response.read(MAX_JOB_LOG + 1)
    if len(data) > MAX_JOB_LOG:
        raise ValueError('job log exceeds 16 MiB limit')
    return data.decode('utf-8', errors='replace')


def diagnostic(log, step):
    start = datetime.fromisoformat(step['started_at'].replace('Z', '+00:00'))
    # GitHub step boundaries have one-second precision; log timestamps have fractions.
    end = datetime.fromisoformat(step['completed_at'].replace('Z', '+00:00')) + timedelta(seconds=1)
    first, tail = [], deque(maxlen=2)
    marker = re.compile(r'^(?:---- .+ stdout ----|(?:error|fatal)(?:\[|:)|Traceback \(|thread .+ panicked|FAILED\b|FAIL(?:ED)?:|AssertionError\b)', re.I)
    for entry in re.finditer(r'[^\n]+', log):
        raw = log[entry.start():min(entry.end(), entry.start() + MAX_LOG_LINE)].rstrip('\r')
        raw = re.sub(r'\x1b\[[0-?]*[ -/]*[@-~]', '', raw.lstrip('\ufeff'))
        timestamp, separator, line = raw.partition(' ')
        if not separator:
            continue
        try:
            time = datetime.fromisoformat(timestamp.replace('Z', '+00:00'))
        except ValueError:
            continue
        if not start <= time < end:
            continue
        annotated = line.startswith('##[error]')
        if annotated:
            line = line[len('##[error]'):]
        elif line.startswith(('##[', '[command]')):
            continue
        if line.startswith(('Process completed with exit code', 'Error: Process completed with exit code')):
            break
        if not line.strip():
            continue
        line = line[:MAX_DIAGNOSTIC_LINE].replace('```', "'''")
        tail.append(line)
        if len(first) == 1 or (not first and (annotated or marker.match(line))):
            first.append(line)
            if len(first) == 2:
                return first
    return first or list(tail) or ['Error output unavailable; see the job log.']


def commit_actors(node_ids, include_committer):
    query = """query($ids: [ID!]!) {
        nodes(ids: $ids) { ... on Commit {
            authors(first: 100) { nodes { name user { login } } pageInfo { hasNextPage } }
            committer { name user { login } }
        } }
    }"""
    names = {}
    for offset in range(0, len(node_ids), 100):
        payload = {'query': query, 'variables': {'ids': node_ids[offset:offset + 100]}}
        response = subprocess.run(['gh', 'api', 'graphql', '--input', '-'], input=json.dumps(payload),
                                  capture_output=True, text=True, check=True, timeout=30)
        if len(response.stdout) > 8 * 1024 * 1024:
            raise ValueError('GitHub author response exceeds limit')
        value = json.loads(response.stdout)
        if value.get('errors'):
            raise ValueError('GitHub author lookup failed')
        for node in value['data']['nodes']:
            if node is None or node['authors']['pageInfo']['hasNextPage']:
                raise ValueError('GitHub commit authors unavailable or exceed 100 per commit')
            actors = node['authors']['nodes'] + ([node['committer']] if include_committer else [])
            for actor in actors:
                user = actor.get('user')
                name = user['login'] if user else actor['name']
                if name:
                    names[('' if user else 'name:') + name.lower()] = ('@' + name) if user else name
    return names


def contributors(report):
    head = report['head_commit']
    commit = api('commits/' + head)
    names = commit_actors([commit['node_id']], True)
    people = ' '.join('<@' + SLACK_USERS[key] + '>' if key in SLACK_USERS else slack_escape(names[key])
                      for key in sorted(names))
    if len(people) > MAX_SLACK_TEXT // 4:
        raise ValueError('contributor list exceeds notification allocation')
    return people


def failure_sections(report, conclusion):
    path = f"actions/runs/{report['run_id']}/attempts/{report['run_attempt']}/jobs"
    try:
        jobs = pages(path, 'jobs')
    except (subprocess.SubprocessError, ValueError):
        yield 'Failed-step details unavailable; see the CI run.'
        return
    count = 0
    for job in jobs:
        failed = [s for s in job['steps'] if s['conclusion'] in FAILURES]
        if not failed and job['conclusion'] not in FAILURES:
            continue
        try:
            log = job_log(job['id']) if failed else ''
        except (urllib.error.URLError, TimeoutError, ValueError, RuntimeError):
            log = ''
        job_url = f"https://github.com/{publish.repository()}/actions/runs/{report['run_id']}/job/{int(job['id'])}"
        for step in failed or [None]:
            count += 1
            if count > MAX_FAILED_STEPS:
                raise ValueError('failed-step details exceed 1,000 entries')
            label = slack_escape(job['name'][:200])
            if step:
                label += ' (' + slack_escape(step['name'][:200]) + ')'
                lines = ['Error output unavailable; see the job log.']
                if log and step['started_at'] and step['completed_at']:
                    # Whole-second API boundaries cannot distinguish overlapping failed steps.
                    overlap = any(other is not step and other['started_at'] and other['completed_at']
                                  and other['started_at'] <= step['completed_at']
                                  and step['started_at'] <= other['completed_at'] for other in failed)
                    if overlap:
                        lines = ['Error output unavailable: failed-step timestamps overlap; see the job log.']
                    else:
                        lines = diagnostic(log, step)
            else:
                lines = [f"Job {job['conclusion'].replace('_', ' ')} before a failed step was recorded."]
            yield f'<{job_url}|{label}>:\n```\n' + slack_escape('\n'.join(lines)) + '\n```'
    if not count:
        yield f"Run {conclusion.replace('_', ' ')}; no failed steps were recorded."


def failure_messages(result):
    report = result['report']
    run_url = f"https://github.com/{publish.repository()}/actions/runs/{report['run_id']}/attempts/{report['run_attempt']}"
    header = f"❌ [{slack_escape(report['workflow'])}] {slack_subject(report, result['subject'])}\n"
    header += f"<{run_url}|CI run {report['run_id']} ({report['run_attempt']})>, source {report['source_commit'][:12]}"
    try:
        people = contributors(report)
        footer = '\n\nCC ' + people if people else ''
    except (subprocess.SubprocessError, ValueError):
        footer = '\n\nContributor details unavailable; see the commit.'
    if result['url']:
        footer += f"\n\n<{result['url']}|Build results and downloads>"
    else:
        footer += '\n\nResult publication failed; see CI reporting logs.'
    text = header
    for section in failure_sections(report, result['conclusion']):
        if len(text) + len(section) + len(footer) + 2 > MAX_SLACK_TEXT:
            if text != header:
                yield text + footer
                text = header
            if len(header) + len(section) + len(footer) + 2 > MAX_SLACK_TEXT:
                raise ValueError('Slack failure section exceeds message limit')
        text += '\n\n' + section
    yield text + footer


def notify(output):
    result = json.loads(output.read_text())
    report, url = result['report'], result['url']
    if (result['conclusion'] in FAILURES and result['subject']['branch'] == 'main' and
            result['subject']['event'] in ('push', 'workflow_dispatch')):
        for text in failure_messages(result):
            slack_message(text)
    elif result['conclusion'] == 'success':
        run_url = f"https://github.com/{publish.repository()}/actions/runs/{report['run_id']}/attempts/{report['run_attempt']}"
        identity = f"run {report['run_id']}, attempt {report['run_attempt']}, source {report['source_commit'][:12]}"
        body = f"**{report['workflow']}: SUCCESS** — {identity}.\n\n[CI run]({run_url})"
        if url:
            body += f" · [Build results and downloads]({url})"
        else:
            body += '\n\nResult publication failed; build artifacts remain in GitHub for recovery.'
        github_comment(report, body)


if __name__ == '__main__':
    action, output = sys.argv[1], Path(sys.argv[2])
    if action == 'prepare':
        prepare(output)
    elif action == 'publish':
        publish_run(output)
    elif action == 'notify':
        notify(output)
    else:
        raise ValueError('expected prepare, publish or notify')
