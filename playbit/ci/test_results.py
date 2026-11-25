import io
import json
import os
from pathlib import Path
import stat
import subprocess
import tempfile
import unittest
import urllib.error
import zipfile
from unittest.mock import patch

import results
from test_fuzz import evidence

SOURCE = 'a' * 40
HEAD = 'b' * 40


def run(event='pull_request'):
    return {'id': 42, 'run_attempt': 2, 'status': 'completed', 'conclusion': 'success',
            'path': '.github/workflows/playbit.yml', 'name': 'playbit', 'title': 'Example change', 'head_sha': HEAD, 'event': event,
            'head_branch': 'feature' if event == 'pull_request' else 'main',
            'head_commit': {'message': 'Fix <script> & "quotes"\n\nLonger description.'},
            'repository': {'full_name': 'example/project'}, 'head_repository': {'full_name': 'example/project'}}


def provenance():
    return {'source_commit': SOURCE, 'head_commit': HEAD, 'run_id': 42, 'run_attempt': 2, 'pull_request': 7}


def report():
    return {**provenance(), 'checkout_verified': True, 'status': 'success',
            'workflow': 'playbit', 'pull_request_title': 'Example change', 'jobs': {'test': 'success'}, 'artifacts': []}


class Results(unittest.TestCase):
    def setUp(self):
        # Publication tests must never append fixture URLs to the real CI summary.
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.step_summary = Path(temporary.name) / 'summary.md'
        environment = patch.dict(os.environ, {'GITHUB_REPOSITORY': 'example/project',
                                              'GITHUB_STEP_SUMMARY': str(self.step_summary)})
        environment.start()
        self.addCleanup(environment.stop)

    def test_only_completed_known_workflows_from_this_repository(self):
        with tempfile.TemporaryDirectory() as temporary, \
             patch.dict(os.environ, {'CI_RUN_ID': '42', 'CI_RUN_ATTEMPT': '2'}), \
             patch.object(results, 'pull_request', return_value=7):
            output = Path(temporary) / 'notice.json'
            for changes in ({'path': '.github/workflows/unrelated.yml'},
                            {'path': '.github/workflows/kvdb.yml'}, {'status': 'in_progress'},
                            {'repository': {'full_name': 'another/repo'}}):
                with self.subTest(changes=changes), patch.object(results, 'api', return_value={**run(), **changes}):
                    with self.assertRaises(ValueError):
                        results.prepare(output)
            # Display-name changes must not invalidate earlier runs of the workflow.
            with patch.object(results, 'api', return_value={**run(), 'name': 'Previous display name'}):
                results.prepare(output)
            main_checks = {**run('push'), 'path': results.MAIN_CHECKS, 'name': 'main checks'}
            with patch.object(results, 'api', return_value=main_checks):
                results.prepare(output)
            for changes in ({'event': 'pull_request'}, {'head_branch': 'feature'}):
                with self.subTest(changes=changes), patch.object(results, 'api', return_value={**main_checks, **changes}):
                    with self.assertRaisesRegex(ValueError, 'main branch push'):
                        results.prepare(output)

    def test_missing_object_is_distinct_from_permission_failure(self):
        with tempfile.TemporaryDirectory() as temporary:
            for message, missing in [(b'(NoSuchKey)', True), (b'(AccessDenied)', False)]:
                error = subprocess.CalledProcessError(1, ['aws'], stderr=message)
                with self.subTest(message=message), patch.object(results.publish, 'aws', side_effect=error):
                    if missing:
                        self.assertIsNone(results.read_object('result-fixture', 'absent', Path(temporary) / 'object'))
                    else:
                        with self.assertRaises(subprocess.CalledProcessError):
                            results.read_object('result-fixture', 'absent', Path(temporary) / 'object')

    def test_pr_requires_exact_merge_and_attempt(self):
        with patch.object(results, 'api', return_value={'parents': [{'sha': 'c' * 40}, {'sha': HEAD}]}):
            self.assertEqual(results.validate_provenance(provenance(), run()), provenance())
            for key, value in [('run_id', 43), ('run_attempt', 1), ('head_commit', SOURCE)]:
                with self.subTest(key=key), self.assertRaises(ValueError):
                    results.validate_provenance({**provenance(), key: value}, run())
        with patch.object(results, 'api', return_value={'parents': [{'sha': HEAD}, {'sha': SOURCE}]}), self.assertRaises(ValueError):
            results.validate_provenance(provenance(), run())
        with self.assertRaises(ValueError):
            results.validate_provenance(provenance(), run('push'))

    def test_reused_build_provenance_keeps_run_head_source_and_attempt_bounds(self):
        # A failed consumer retries with the successful export's earlier input.
        parents = {'parents': [{'sha': 'c' * 40}, {'sha': HEAD}]}
        previous = {**provenance(), 'run_attempt': 1}
        with patch.object(results, 'api', return_value=parents):
            self.assertEqual(results.validate_provenance(previous, run(), reused_build=True), previous)
            for changes in ({'run_id': 43}, {'head_commit': SOURCE}, {'run_attempt': 3}, {'run_attempt': 0}):
                with self.subTest(changes=changes), self.assertRaises(ValueError):
                    results.validate_provenance({**previous, **changes}, run(), reused_build=True)
            # Fuzz transfers still belong to the exact current attempt.
            with self.assertRaises(ValueError):
                results.validate_provenance(previous, run())
        with patch.object(results, 'api', return_value={'parents': [{'sha': HEAD}, {'sha': SOURCE}]}):
            with self.assertRaises(ValueError):
                results.validate_provenance(previous, run(), reused_build=True)

    def test_failed_run_keeps_retry_source_and_provenance_until_retention_expiry(self):
        artifacts = [{'name': 'public-build-inputs-1', 'id': 1},
                     {'name': 'build-provenance-1', 'id': 2},
                     {'name': 'public-client-products-2', 'id': 3}]
        for status in ('failure', 'cancelled', 'success'):
            with self.subTest(status=status), patch.object(results, 'api') as api:
                results.cleanup(artifacts, {**report(), 'status': status, 'artifacts': [{'path': 'product'}]})
                self.assertEqual([call.args for call in api.call_args_list], [('actions/artifacts/3', 'DELETE')])

    def test_archive_rejects_paths_links_duplicates_and_capacity(self):
        for name, mode in [('../private.py', 0), ('/private.py', 0), ('x\\private.py', 0),
                           ('link', stat.S_IFLNK | 0o777), ('pipe', stat.S_IFIFO | 0o600)]:
            with self.subTest(name=name), tempfile.TemporaryDirectory() as temporary:
                data = io.BytesIO()
                with zipfile.ZipFile(data, 'w') as z:
                    entry = zipfile.ZipInfo(name)
                    entry.external_attr = mode << 16
                    z.writestr(entry, b'bad')
                data.seek(0)
                with self.assertRaises(ValueError):
                    results.extract(data, Path(temporary))
        with tempfile.TemporaryDirectory() as temporary:
            data = io.BytesIO()
            with zipfile.ZipFile(data, 'w') as z:
                z.writestr('web/index.html', 'safe')
                z.writestr('macos-arm64.zip', 'archive')
            data.seek(0)
            results.extract(data, Path(temporary))
            self.assertEqual((Path(temporary) / 'web/index.html').read_text(), 'safe')
            data.seek(0)
            with self.assertRaises(FileExistsError):
                results.extract(data, Path(temporary))
            data.seek(0)
            with patch.object(results, 'MAX_PRODUCTS', 2), self.assertRaises(ValueError):
                results.extract(data, Path(temporary))

    def test_artifact_redirect_does_not_forward_github_token(self):
        archive = io.BytesIO()
        with zipfile.ZipFile(archive, 'w') as z:
            z.writestr('web/index.html', 'public')
        archive.seek(0)
        location = 'https://artifact.example.invalid/signed'
        redirect = urllib.error.HTTPError('https://api.github.com/', 302, 'redirect', {'Location': location}, io.BytesIO())
        with tempfile.TemporaryDirectory() as temporary, \
             patch.dict(os.environ, {'GH_TOKEN': 'fixture-token'}), \
             patch.object(results.urllib.request, 'build_opener') as opener, \
             patch.object(results.urllib.request, 'urlopen', return_value=archive) as storage:
            opener.return_value.open.side_effect = redirect
            results.download({'id': 7, 'expired': False, 'size_in_bytes': len(archive.getvalue())}, Path(temporary))
            self.assertEqual(opener.return_value.open.call_args.args[0].get_header('Authorization'), 'Bearer fixture-token')
            storage.assert_called_once_with(location, timeout=30)
            self.assertEqual((Path(temporary) / 'web/index.html').read_text(), 'public')

    def test_cleanup_removes_only_verified_public_transfers(self):
        artifacts = [{'name': 'public-client-products-2', 'id': 1}, {'name': 'build-provenance-1', 'id': 2},
                     {'name': 'private-service-2', 'id': 3}, {'name': 'build-timings', 'id': 4},
                     {'name': 'public-build-inputs-1', 'id': 5}]
        summary = {**report(), 'artifacts': [{'path': 'product'}]}
        with patch.object(results, 'api') as api:
            results.cleanup(artifacts, summary)
            self.assertEqual([call.args for call in api.call_args_list], [
                ('actions/artifacts/1', 'DELETE')])
        with patch.object(results, 'api') as api:
            results.cleanup(artifacts, {**summary, 'checkout_verified': False, 'artifacts': []})
            api.assert_not_called()

    def test_comments_use_pr_or_head_commit_and_retry_updates_bot_only(self):
        for pr in [7, None]:
            with self.subTest(pr=pr):
                value = {**report(), 'pull_request': pr}
                path = 'issues/7/comments' if pr else 'commits/' + HEAD + '/comments'
                comments = [{'id': 1, 'user': {'login': 'person'}, 'body': '<!-- archipelago-ci:42:2 -->'}]
                with patch.object(results, 'pages', return_value=comments), patch.object(results, 'api') as api:
                    results.github_comment(value, 'result')
                    self.assertEqual(api.call_args.args[:2], (path, 'POST'))
                    self.assertEqual(api.call_args.args[2]['body'], 'result\n\n<!-- archipelago-ci:42:2 -->')
                comments.append({'id': 2, 'user': {'login': 'github-actions[bot]'}, 'body': '<!-- archipelago-ci:42:2 -->'})
                with patch.object(results, 'pages', return_value=comments), patch.object(results, 'api') as api:
                    results.github_comment(value, 'result')
                    self.assertEqual(api.call_args.args[:2], ('issues/comments/2' if pr else 'comments/2', 'PATCH'))

    def test_notify_selects_exact_failure_conclusions_and_preserves_success_comments(self):
        # Exercise each raw GitHub conclusion, including publication failures.
        for event, branch, pr in [('push', 'main', None), ('workflow_dispatch', 'main', None),
                                  ('push', 'feature', None), ('workflow_dispatch', 'feature', None),
                                  ('pull_request', 'feature', 7), ('pull_request', 'main', 7),
                                  ('pull_request', 'main', None)]:
            for conclusion in ['success', 'failure', 'cancelled', 'skipped', 'timed_out',
                               'neutral', 'action_required', 'stale', 'startup_failure', 'unknown']:
                for url in ['https://ci.playbit.app/example/', None]:
                    with self.subTest(event=event, branch=branch, pr=pr, conclusion=conclusion, url=url), tempfile.TemporaryDirectory() as temporary:
                        output = Path(temporary) / 'result.json'
                        output.write_text(json.dumps({'report': {**report(), 'pull_request': pr,
                                                      'status': results.outcome(conclusion)}, 'conclusion': conclusion,
                                                      'url': url, 'subject': results.subject({**run(event), 'head_branch': branch})}))
                        with patch.object(results, 'pages', return_value=[]) as pages, \
                             patch.object(results, 'api') as api, patch.object(results, 'slack_message') as slack, \
                             patch.object(results, 'failure_messages', return_value=iter(['failure details'])) as messages:
                            results.notify(output)
                        if conclusion == 'success':
                            path = 'issues/7/comments' if pr else 'commits/' + HEAD + '/comments'
                            pages.assert_called_once_with(path)
                            self.assertEqual(api.call_args.args[:2], (path, 'POST'))
                            self.assertIn('**playbit: SUCCESS**', api.call_args.args[2]['body'])
                            if url:
                                self.assertIn('[Build results and downloads](' + url + ')', api.call_args.args[2]['body'])
                        else:
                            pages.assert_not_called()
                            api.assert_not_called()
                        if conclusion in results.FAILURES and branch == 'main' and event != 'pull_request':
                            messages.assert_called_once()
                            slack.assert_called_once_with('failure details')
                        else:
                            messages.assert_not_called()
                            slack.assert_not_called()

    def test_slack_names_the_main_branch_and_commit(self):
        # Failure messages keep escaped subjects, exact attempt/source, mentions and result links.
        push = {**report(), 'pull_request': None, 'pull_request_title': None}
        subject = results.subject(run('push'))
        self.assertEqual(subject, {'branch': 'main', 'event': 'push', 'commit': 'Fix <script> & "quotes"'})
        for value, expected in [
                (push, '[main] <https://github.com/example/project/commit/' + HEAD + '|Fix &lt;script&gt; &amp; "quotes">')]:
            result = {'report': value, 'url': 'https://ci.playbit.app/example/',
                      'subject': subject, 'conclusion': 'failure'}
            with patch.object(results, 'contributors', return_value='<@U04CSF7GQR4>'), \
                 patch.object(results, 'failure_sections', return_value=iter(['diagnostic'])):
                message, = results.failure_messages(result)
            self.assertEqual(message.split('\n')[0], '❌ [playbit] ' + expected)
            self.assertIn('CI run 42 (2)', message)
            self.assertIn('source ' + SOURCE[:12], message)
            self.assertIn('CC <@U04CSF7GQR4>', message)
            self.assertTrue(message.endswith('<https://ci.playbit.app/example/|Build results and downloads>'))

    def test_prepare_records_raw_conclusion_and_subject_for_failure_notices(self):
        # A neutral conclusion stays distinguishable from the public summary's normalized status.
        def api(path, method='GET', body=None):
            return {'title': 'Example change'} if path == 'pulls/7' else {**run(), 'conclusion': 'neutral'}
        with tempfile.TemporaryDirectory() as temporary, \
             patch.dict(os.environ, {'CI_RUN_ID': '42', 'CI_RUN_ATTEMPT': '2'}), \
             patch.object(results, 'api', side_effect=api), \
             patch.object(results, 'pull_request', return_value=7):
            output = Path(temporary) / 'notice.json'
            results.prepare(output)
            notice = json.loads(output.read_text())
            self.assertEqual(notice['report']['pull_request_title'], 'Example change')
            self.assertEqual(notice['conclusion'], 'neutral')
            self.assertEqual(notice['subject'], {'branch': 'feature', 'event': 'pull_request', 'commit': 'Fix <script> & "quotes"'})

    def test_slack_secret_never_in_error(self):
        # Delivery failures report an actionable error without exposing the webhook URL.
        secret = 'https://hooks.slack.com/services/PRIVATE/SECRET'
        with patch.dict(os.environ, {'SLACK_WEBHOOK_URL': secret}), \
             patch.object(results.urllib.request, 'build_opener') as opener:
            opener.return_value.open.side_effect = urllib.error.URLError(secret)
            with self.assertRaises(RuntimeError) as caught:
                results.slack_message('test')
            self.assertNotIn(secret, str(caught.exception))
            self.assertTrue(caught.exception.__suppress_context__)
            self.assertEqual(opener.return_value.open.call_args.kwargs['timeout'], 15)

    def test_diagnostics_use_first_error_block_or_last_meaningful_lines(self):
        # Ignore successful steps, runner framing, ANSI escapes and exit-code boilerplate.
        step = {'started_at': '2026-10-01T15:55:19Z', 'completed_at': '2026-10-01T15:56:16Z'}
        cases = [
            ('\x1b[31merror\x1b[0m: used unwrap()\n    --> file.rs:10:2\nlater error',
             ['error: used unwrap()', '    --> file.rs:10:2']),
            ('failures:\n\n---- test_name stdout ----\nthread panicked at file.rs:10:2\nextra detail',
             ['---- test_name stdout ----', 'thread panicked at file.rs:10:2']),
            ('ordinary output\nlast useful line\nfinal useful line\n##[error]Process completed with exit code 1.',
             ['last useful line', 'final useful line']),
            ('##[group]Run command\n##[endgroup]\n', ['Error output unavailable; see the job log.']),
            ('##[error]Something broke\ncontext\nextra detail', ['Something broke', 'context'])]
        for text, expected in cases:
            log = '2026-10-01T15:55:18.0000000Z error: earlier successful step\n'
            log += '\n'.join('2026-10-01T15:56:16.5000000Z ' + line for line in text.splitlines())
            log += '\n2026-10-01T15:56:17.0000000Z error: later step\n'
            with self.subTest(text=text):
                self.assertEqual(results.diagnostic(log, step), expected)
        long = '2026-10-01T15:55:20Z error: ' + 'x' * 1000
        self.assertEqual(len(results.diagnostic(long, step)[0]), results.MAX_DIAGNOSTIC_LINE)

    def test_every_failed_step_is_linked_with_its_own_diagnostic(self):
        # Multiple failed steps use one bounded log fetch per job and the exact attempt's jobs.
        steps = [
            {'name': 'Compile', 'conclusion': 'failure', 'started_at': '2026-10-01T15:55:19Z', 'completed_at': '2026-10-01T15:55:20Z'},
            {'name': 'Test', 'conclusion': 'timed_out', 'started_at': '2026-10-01T15:55:22Z', 'completed_at': '2026-10-01T15:55:23Z'},
            {'name': 'Cleanup', 'conclusion': 'success'}]
        jobs = [{'id': 100, 'name': 'check <&>', 'conclusion': 'failure', 'steps': steps},
                {'id': 101, 'name': 'startup', 'conclusion': 'startup_failure', 'steps': []},
                {'id': 102, 'name': 'success', 'conclusion': 'success', 'steps': []}]
        log = '2026-10-01T15:55:19Z error: compile\n2026-10-01T15:55:20Z location\n'
        log += '2026-10-01T15:55:22Z test still waiting\n2026-10-01T15:55:23Z deadline reached\n'
        with patch.object(results, 'pages', return_value=jobs) as pages, \
             patch.object(results, 'job_log', return_value=log) as fetch:
            sections = list(results.failure_sections(report(), 'failure'))
        pages.assert_called_once_with('actions/runs/42/attempts/2/jobs', 'jobs')
        fetch.assert_called_once_with(100)
        self.assertEqual(len(sections), 3)
        self.assertIn('job/100|check &lt;&amp;&gt; (Compile)', sections[0])
        self.assertIn('error: compile\nlocation', sections[0])
        self.assertIn('test still waiting\ndeadline reached', sections[1])
        self.assertIn('Job startup failure', sections[2])

    def test_missing_logs_or_metadata_still_produce_failure_details(self):
        # Unavailable logs/people/publication do not suppress the failure notification.
        step = {'name': 'Test', 'conclusion': 'failure', 'started_at': None, 'completed_at': None}
        job = {'id': 100, 'name': 'tests', 'conclusion': 'failure', 'steps': [step]}
        with patch.object(results, 'pages', return_value=[job]), \
             patch.object(results, 'job_log', side_effect=urllib.error.URLError('unavailable')):
            sections = list(results.failure_sections(report(), 'failure'))
        self.assertIn('Error output unavailable', sections[0])
        result = {'report': report(), 'subject': results.subject(run()), 'conclusion': 'startup_failure', 'url': None}
        with patch.object(results, 'contributors', side_effect=ValueError('unavailable')), \
             patch.object(results, 'pages', side_effect=subprocess.CalledProcessError(1, ['gh'])):
            message, = results.failure_messages(result)
        self.assertIn('Failed-step details unavailable', message)
        self.assertIn('Contributor details unavailable', message)
        self.assertIn('Result publication failed', message)
        with patch.object(results, 'pages', return_value=[]):
            self.assertEqual(list(results.failure_sections(report(), 'startup_failure')),
                             ['Run startup failure; no failed steps were recorded.'])

    def test_failure_messages_split_without_losing_failed_steps(self):
        # Large reports split at section boundaries, retaining links and mentions in every message.
        result = {'report': report(), 'subject': results.subject(run()), 'conclusion': 'failure', 'url': None}
        sections = ['step ' + str(i) + ': ' + 'x' * 250 for i in range(20)]
        with patch.object(results, 'MAX_SLACK_TEXT', 1000), \
             patch.object(results, 'contributors', return_value='<@U04CSF7GQR4>'), \
             patch.object(results, 'failure_sections', return_value=iter(sections)):
            messages = list(results.failure_messages(result))
        self.assertGreater(len(messages), 1)
        for message in messages:
            self.assertLessEqual(len(message), 1000)
            self.assertIn('CC <@U04CSF7GQR4>', message)
        for section in sections:
            self.assertEqual(sum(section in message for message in messages), 1)

    def test_job_log_redirect_keeps_token_private_and_bounds_reads(self):
        # Fetch signed log storage without authorization and reject oversized/non-HTTPS transfers.
        for location, content, expected in [('https://logs.example.invalid/signed', b'log', None),
                                            ('http://logs.example.invalid/signed', b'log', 'HTTPS'),
                                            ('https://logs.example.invalid/signed', b'123456', '16 MiB')]:
            redirect = urllib.error.HTTPError('https://api.github.com/', 302, 'redirect', {'Location': location}, io.BytesIO())
            with self.subTest(location=location, content=content), patch.dict(os.environ, {'GH_TOKEN': 'fixture-token'}), \
                 patch.object(results.urllib.request, 'build_opener') as opener, \
                 patch.object(results.urllib.request, 'urlopen') as storage, patch.object(results, 'MAX_JOB_LOG', 5):
                opener.return_value.open.side_effect = redirect
                storage.return_value.__enter__.return_value.read.return_value = content
                if expected:
                    with self.assertRaisesRegex(ValueError, expected):
                        results.job_log(100)
                else:
                    self.assertEqual(results.job_log(100), 'log')
                    storage.assert_called_once_with(location, timeout=30)
                    storage.return_value.__enter__.return_value.read.assert_called_once_with(6)
                self.assertEqual(opener.return_value.open.call_args.args[0].get_header('Authorization'), 'Bearer fixture-token')

    def test_commit_authors_include_coauthors_and_branch_committer(self):
        # GitHub's authors connection resolves Co-authored-by trailers to account logins.
        def actor(login):
            return {'name': login, 'user': {'login': login}}
        node = {'authors': {'nodes': [actor('rsms'), actor('ForLoveOfCats')], 'pageInfo': {'hasNextPage': False}},
                'committer': actor('edward-playbit')}
        response = subprocess.CompletedProcess([], 0, json.dumps({'data': {'nodes': [node]}}))
        with patch.object(results.subprocess, 'run', return_value=response) as call:
            self.assertEqual(results.commit_actors(['node'], False), {'rsms': '@rsms', 'forloveofcats': '@ForLoveOfCats'})
            self.assertEqual(results.commit_actors(['node'], True),
                             {'rsms': '@rsms', 'forloveofcats': '@ForLoveOfCats', 'edward-playbit': '@edward-playbit'})
        payload = json.loads(call.call_args.kwargs['input'])
        self.assertEqual(payload['variables']['ids'], ['node'])
        node['authors']['pageInfo']['hasNextPage'] = True
        response.stdout = json.dumps({'data': {'nodes': [node]}})
        with patch.object(results.subprocess, 'run', return_value=response), self.assertRaises(ValueError):
            results.commit_actors(['node'], True)

    def test_main_contributors_include_commit_authors_and_committer(self):
        with patch.object(results, 'api', return_value={'node_id': 'head'}), \
             patch.object(results, 'commit_actors', return_value={'edward-playbit': '@edward-playbit'}) as actors:
            self.assertEqual(results.contributors({**report(), 'pull_request': None}), '<@U05UNAU39M4>')
        actors.assert_called_once_with(['head'], True)

    def test_ambiguous_failed_step_boundaries_do_not_misattribute_errors(self):
        # Same-second failures keep both names but avoid assigning one step's error to the other.
        steps = [{'name': name, 'conclusion': 'failure', 'started_at': '2026-10-01T15:55:19Z',
                  'completed_at': '2026-10-01T15:55:19Z'} for name in ['First', 'Second']]
        job = {'id': 100, 'name': 'tests', 'conclusion': 'failure', 'steps': steps}
        with patch.object(results, 'pages', return_value=[job]), \
             patch.object(results, 'job_log', return_value='2026-10-01T15:55:19.5000000Z error: second step'):
            sections = list(results.failure_sections(report(), 'failure'))
        self.assertEqual(len(sections), 2)
        for section in sections:
            self.assertIn('timestamps overlap', section)
            self.assertNotIn('error: second step', section)

    def test_diagnostic_tail_stops_at_step_exit(self):
        # Fallback lines belong to the failed command, rather than same-second cleanup work.
        step = {'started_at': '2026-10-01T15:55:19Z', 'completed_at': '2026-10-01T15:55:20Z'}
        log = '\n'.join('2026-10-01T15:55:20.5000000Z ' + line for line in [
            'last useful line', 'final useful line', '##[error]Process completed with exit code 1.',
            'unrelated cleanup output'])
        self.assertEqual(results.diagnostic(log, step), ['last useful line', 'final useful line'])

    def test_unassociated_git_identity_cannot_impersonate_mapped_account(self):
        # A git name equal to a mapped username is plain text, never that person's Slack mention.
        node = {'authors': {'nodes': [{'name': 'rsms', 'user': None}], 'pageInfo': {'hasNextPage': False}},
                'committer': {'name': 'Unknown', 'user': None}}
        response = subprocess.CompletedProcess([], 0, json.dumps({'data': {'nodes': [node]}}))
        with patch.object(results.subprocess, 'run', return_value=response):
            names = results.commit_actors(['node'], False)
        self.assertEqual(names, {'name:rsms': 'rsms'})
        with patch.object(results, 'api', return_value={'node_id': 'head'}), \
             patch.object(results, 'commit_actors', return_value=names):
            self.assertEqual(results.contributors({**report(), 'pull_request': None}), 'rsms')

    def test_reporting_retry_preserves_raw_failure_conclusion_and_private_metadata(self):
        # A retry uses the requested attempt and keeps diagnostics out of immutable public summaries.
        summary = {**report(), 'status': 'failure'}
        with tempfile.TemporaryDirectory() as temporary, \
             patch.dict(os.environ, {'CI_RUN_ID': '42', 'CI_RUN_ATTEMPT': '2'}), \
             patch.object(results, 'api', return_value={**run(), 'conclusion': 'timed_out'}), \
             patch.object(results, 'pull_request', return_value=7), patch.object(results, 'pages', return_value=[]), \
             patch.object(results, 'read_object', side_effect=[{'source_commit': SOURCE}, summary]), \
             patch.object(results, 'verify_products'), patch.object(results.publish, 'refresh_run_page'), \
             patch.object(results, 'cleanup'):
            output = Path(temporary) / 'notice.json'
            results.publish_run(output)
            value = json.loads(output.read_text())
        self.assertEqual(value['conclusion'], 'timed_out')
        self.assertEqual(value['report'], summary)
        self.assertNotIn('conclusion', value['report'])
        self.assertNotIn('contributors', value['report'])
        self.assertNotIn('logs', value['report'])

    def test_current_and_reused_export_publish_before_product_deletion(self):
        for producer_attempt, conclusion in ((1, "success"), (2, "success"), (2, "failure")):
            with self.subTest(producer_attempt=producer_attempt, conclusion=conclusion):
                artifacts = [{'name': f'build-provenance-{producer_attempt}', 'id': 1},
                             {'name': 'build-provenance-3', 'id': 99},
                             {'name': 'public-client-products-2', 'id': 2},
                             {'name': 'service-build-timings-macos', 'id': 3}]
                stored = {}
                deleted = []
                def download(artifact, root):
                    self.assertNotEqual(artifact['id'], 99, 'future export must not replace earlier report provenance')
                    root.mkdir()
                    if artifact['id'] == 1:
                        (root / 'provenance.json').write_text(json.dumps({**provenance(), 'run_attempt': producer_attempt}))
                    else:
                        (root / 'macos-arm64.zip').write_bytes(b'compiled public kit')
                def api(path, method='GET', body=None):
                    if method == 'DELETE':
                        self.assertIn('runs/42-2.json', stored)
                        self.assertIn(SOURCE + '/42-2/result.json', stored)
                        deleted.append(path)
                        return None
                    if path == 'pulls/7':
                        return {'title': 'Example change'}
                    return {'parents': [{'sha': 'c' * 40}, {'sha': HEAD}]} if path.startswith('git/commits/') else {**run(), 'conclusion': conclusion}
                def put(path, key, immutable=True):
                    stored[key] = path.read_bytes()
                    return {'path': key, 'sha256': results.publish.sha256(path), 'bytes': path.stat().st_size}
                with tempfile.TemporaryDirectory() as temporary, \
                     patch.dict(os.environ, {'CI_RUN_ID': '42', 'CI_RUN_ATTEMPT': '2'}), \
                     patch.object(results, 'api', side_effect=api), \
                     patch.object(results, 'pull_request', return_value=7), \
                     patch.object(results, 'pages', side_effect=[artifacts, [
                         {'name': 'test / macos', 'conclusion': 'success'},
                         {'name': 'test / web', 'conclusion': conclusion}, {'name': 'deploy', 'conclusion': 'skipped'}]]), \
                     patch.object(results, 'read_object', return_value=None), \
                     patch.object(results, 'download', side_effect=download), \
                     patch.object(results.publish, 'put', side_effect=put), \
                     patch.object(results.publish, 'aws', return_value=subprocess.CompletedProcess([], 0, b'{"CommonPrefixes":[]}')):
                    output = Path(temporary) / 'notice.json'
                    results.publish_run(output)
                    summary = json.loads(stored[SOURCE + '/42-2/result.json'])
                    self.assertEqual(summary['source_commit'], SOURCE)
                    self.assertEqual(summary['head_commit'], HEAD)
                    self.assertEqual(summary['status'], conclusion)
                    self.assertEqual(summary['jobs']['test-web'], conclusion)
                    self.assertEqual(summary['jobs']['test-macos'], 'success')
                    self.assertEqual(summary['pull_request'], 7)
                    self.assertEqual(summary['pull_request_title'], 'Example change')
                    self.assertEqual(len(summary['artifacts']), 1)
                    notice = json.loads(output.read_text())
                    url = f'https://ci.playbit.app/{SOURCE}/42-2/'
                    self.assertEqual(notice['url'], url)
                    self.assertIn(url.removeprefix('https://ci.playbit.app/'), stored)
                    self.assertEqual(self.step_summary.read_text().splitlines()[-1],
                                     f'[{conclusion.upper()} — build results and downloads]({url})')
                    self.assertEqual(notice['subject']['branch'], 'feature')
                    self.assertEqual(deleted, ['actions/artifacts/2'])

    def test_fuzz_failure_publishes_without_export_provenance_and_cleans_up(self):
        artifacts = [{'name': 'fuzz-evidence-2', 'id': 9}]
        stored, deleted = {}, []
        failed_run = {**run('push'), 'path': results.MAIN_CHECKS, 'name': 'main checks',
                      'conclusion': 'failure', 'head_sha': SOURCE}
        def download(artifact, root):
            evidence(root, failed='decode_view')
        def api(path, method='GET', body=None):
            if method == 'DELETE':
                self.assertIn(SOURCE + '/42-2/fuzz/decode_view-crashes.zip', stored)
                self.assertIn('runs/42-2.json', stored)
                deleted.append(path)
                return None
            return failed_run
        def put(path, key, immutable=True):
            stored[key] = path.read_bytes()
            return {'path': key, 'sha256': results.publish.sha256(path), 'bytes': path.stat().st_size}
        with tempfile.TemporaryDirectory() as temporary, \
             patch.dict(os.environ, {'CI_RUN_ID': '42', 'CI_RUN_ATTEMPT': '2'}), \
             patch.object(results, 'api', side_effect=api), \
             patch.object(results, 'pull_request', return_value=None), \
             patch.object(results, 'pages', side_effect=[artifacts, [{'name': 'fuzz', 'conclusion': 'failure'}]]), \
             patch.object(results, 'read_object', return_value=None), \
             patch.object(results, 'download', side_effect=download), \
             patch.object(results.publish, 'put', side_effect=put), \
             patch.object(results.publish, 'aws', return_value=subprocess.CompletedProcess([], 0, b'{"CommonPrefixes":[]}')), \
             patch.object(results.fuzz_results, 'promote') as promote:
            results.publish_run(Path(temporary) / 'notice.json')
        summary = json.loads(stored[SOURCE + '/42-2/result.json'])
        self.assertEqual(summary['status'], 'failure')
        self.assertTrue(summary['checkout_verified'])
        self.assertEqual(summary['fuzz']['targets']['decode_view']['crashes']['inputs'], 1)
        self.assertIn(b'decode_view-crashes.zip', stored[SOURCE + '/42-2/'])
        self.assertEqual(deleted, ['actions/artifacts/9'])
        promote.assert_called_once_with(summary, failed_run)

    def test_failed_promotion_preserves_transfer_and_retry_promotes_stored_evidence(self):
        summary = report()
        with tempfile.TemporaryDirectory() as temporary, \
             patch.dict(os.environ, {'CI_RUN_ID': '42', 'CI_RUN_ATTEMPT': '2'}), \
             patch.object(results, 'api', return_value=run()), \
             patch.object(results, 'pull_request', return_value=7), \
             patch.object(results, 'pages', return_value=[]), \
             patch.object(results, 'read_object', side_effect=[{'source_commit': SOURCE}, summary]), \
             patch.object(results, 'verify_products'), \
             patch.object(results.publish, 'refresh_run_page'), \
             patch.object(results.fuzz_results, 'promote', side_effect=RuntimeError('failed promotion')) as promote, \
             patch.object(results, 'cleanup') as cleanup:
            with self.assertRaises(RuntimeError):
                results.publish_run(Path(temporary) / 'notice.json')
            cleanup.assert_not_called()
            promote.assert_called_once_with(summary, run())

    def test_publication_failure_keeps_artifacts_and_a_notice_context(self):
        with tempfile.TemporaryDirectory() as temporary, \
             patch.dict(os.environ, {'CI_RUN_ID': '42', 'CI_RUN_ATTEMPT': '2'}), \
             patch.object(results, 'api', return_value=run()), \
             patch.object(results, 'pull_request', return_value=7), \
             patch.object(results, 'pages', return_value=[]), \
             patch.object(results, 'read_object', return_value=None), \
             patch.object(results.publish, 'publish', side_effect=RuntimeError('storage failed')), \
             patch.object(results, 'cleanup') as cleanup:
            output = Path(temporary) / 'notice.json'
            with self.assertRaises(RuntimeError):
                results.publish_run(output)
            cleanup.assert_not_called()
            self.assertIsNone(json.loads(output.read_text())['url'])
            self.assertEqual(json.loads(output.read_text())['conclusion'], 'success')

    def test_repeat_reporting_uses_published_provenance_after_transfer_deletion(self):
        summary = report()
        with tempfile.TemporaryDirectory() as temporary, \
             patch.dict(os.environ, {'CI_RUN_ID': '42', 'CI_RUN_ATTEMPT': '2'}), \
             patch.object(results, 'api', return_value=run()), \
             patch.object(results, 'pull_request', return_value=7), \
             patch.object(results, 'pages', return_value=[]), \
             patch.object(results, 'read_object', side_effect=[{'source_commit': SOURCE}, summary]), \
             patch.object(results, 'verify_products') as verify, \
             patch.object(results, 'download') as download, \
             patch.object(results.publish, 'publish') as publish, \
             patch.object(results.publish, 'refresh_run_page') as refresh, \
             patch.object(results, 'cleanup'):
            output = Path(temporary) / 'notice.json'
            results.publish_run(output)
            verify.assert_called_once_with(summary)
            download.assert_not_called()
            publish.assert_not_called()
            refresh.assert_called_once_with(summary, 'Example change')
            url = f'https://ci.playbit.app/{SOURCE}/42-2/'
            self.assertEqual(json.loads(output.read_text())['url'], url)
            self.assertEqual(self.step_summary.read_text(),
                             f'[SUCCESS — build results and downloads]({url})\n')

    def test_fork_outputs_are_not_downloaded_or_published(self):
        fork = {**run(), 'head_repository': {'full_name': 'someone/fork'}}
        artifacts = [{'name': 'public-client-products-2'}, {'name': 'build-provenance-1'}]
        with tempfile.TemporaryDirectory() as temporary, \
             patch.dict(os.environ, {'CI_RUN_ID': '42', 'CI_RUN_ATTEMPT': '2'}), \
             patch.object(results, 'api', return_value=fork), \
             patch.object(results, 'pull_request', return_value=7), \
             patch.object(results, 'pages', side_effect=[artifacts, []]), \
             patch.object(results, 'read_object', return_value=None), \
             patch.object(results, 'download') as download, \
             patch.object(results.publish, 'publish', side_effect=lambda root, value: {**value, 'artifacts': []}), \
             patch.object(results.publish, 'put'), patch.object(results, 'cleanup'):
            results.publish_run(Path(temporary) / 'notice.json')
            download.assert_not_called()


class SummaryIsolation(unittest.TestCase):
    def test_publication_tests_leave_inherited_ci_summary_untouched(self):
        # Simulate GitHub's inherited output file while running real publication tests.
        with tempfile.TemporaryDirectory() as temporary:
            summary = Path(temporary) / 'summary.md'
            summary.write_text('Existing CI summary\n')
            with patch.dict(os.environ, {'GITHUB_STEP_SUMMARY': str(summary)}):
                suite = unittest.TestSuite([
                    Results('test_current_and_reused_export_publish_before_product_deletion'),
                    Results('test_repeat_reporting_uses_published_provenance_after_transfer_deletion'),
                ])
                outcome = unittest.TestResult()
                suite.run(outcome)
                self.assertTrue(outcome.wasSuccessful(), outcome.errors + outcome.failures)
                self.assertEqual(os.environ['GITHUB_STEP_SUMMARY'], str(summary))
            self.assertEqual(summary.read_text(), 'Existing CI summary\n')


if __name__ == '__main__':
    unittest.main()
