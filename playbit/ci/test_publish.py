import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('publisher', Path(__file__).with_name('publish.py'))
publisher = importlib.util.module_from_spec(spec)
spec.loader.exec_module(publisher)


def report(source='a' * 40, status='success'):
    return {'source_commit': source, 'head_commit': source, 'run_id': 42, 'run_attempt': 2,
            'status': status, 'jobs': {'test': status, 'deploy': 'skipped'},
            'workflow': 'playbit', 'pull_request': 7, 'pull_request_title': 'Fix <script> & "quotes"', 'checkout_verified': True}


class Publication(unittest.TestCase):
    def setUp(self):
        environment = patch.dict(os.environ, {'GITHUB_REPOSITORY': 'example/project'})
        environment.start()
        self.addCleanup(environment.stop)
        bucket = patch.object(publisher, 'BUCKET', 'result-fixture')
        bucket.start()
        self.addCleanup(bucket.stop)

    def test_missing_bucket_fails_before_cloud_access(self):
        with patch.object(publisher, 'BUCKET', None), patch.object(publisher, 'aws') as aws:
            with self.assertRaisesRegex(ValueError, 'CI_RESULTS_BUCKET'):
                publisher.put(Path('/unused'), 'result.json')
            aws.assert_not_called()

    def test_rejects_private_outputs_and_symlinks(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for name in ['playbit-ap', 'profile.json', 'source.tar.gz', 'log.txt',
                         'web/private.pbapp', 'web/nested/todo.pbapp']:
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text('private')
                with self.assertRaises(ValueError):
                    publisher.products(root)
                path.unlink()
            for name in ['macos-arm64.zip', 'web/todo.pbapp']:
                path = root / name
                path.symlink_to('/etc/passwd')
                with self.assertRaises(ValueError):
                    publisher.products(root)
                path.unlink()

    def test_admits_public_web_example_packages(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / 'web').mkdir()
            names = ['web/micro-kart.pbapp', 'web/shared-note.pbapp', 'web/todo.pbapp']
            for name in names:
                (root / name).write_bytes(b'public example package')
            self.assertEqual(publisher.products(root), [(name, root / name) for name in names])

    def test_immutable_retry_checks_hash_and_size(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / 'result.json'
            path.write_text('{}')
            digest = publisher.sha256(path)
            failure = subprocess.CalledProcessError(1, ['aws'])
            matching = subprocess.CompletedProcess([], 0, json.dumps({'Metadata': {'sha256': digest}, 'ContentLength': 2}).encode())
            with patch.object(publisher, 'aws', side_effect=[failure, matching]) as aws:
                publisher.put(path, 'a/1-1/result.json')
                self.assertIn('--if-none-match', aws.call_args_list[0].args)
            for head in [{'Metadata': {'sha256': 'wrong'}, 'ContentLength': 2},
                         {'Metadata': {'sha256': digest}, 'ContentLength': 3}]:
                mismatch = subprocess.CompletedProcess([], 0, json.dumps(head).encode())
                with patch.object(publisher, 'aws', side_effect=[failure, mismatch]), self.assertRaises(ValueError):
                    publisher.put(path, 'a/1-1/result.json')
                with patch.object(publisher, 'aws', side_effect=[None, mismatch]), self.assertRaises(ValueError):
                    publisher.put(path, 'a/1-1/result.json')

    def test_status_and_safe_provenance_exclude_unrelated_private_fields(self):
        for status in ('success', 'failure', 'cancelled', 'skipped'):
            with self.subTest(status=status), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                (root / 'macos-arm64.zip').write_bytes(b'zip')
                captured = {}
                def put(path, key, immutable=True):
                    captured[key] = (path.read_bytes(), immutable)
                    return {'path': key, 'sha256': publisher.sha256(path), 'bytes': path.stat().st_size}
                listed = subprocess.CompletedProcess([], 0, b'{"CommonPrefixes":[]}')
                value = {**report(status=status), 'outputs': {'secret': 'DO NOT PUBLISH'}}
                with patch.object(publisher, 'put', side_effect=put), patch.object(publisher, 'aws', return_value=listed):
                    publisher.publish(root, value)
                prefix = 'a' * 40 + '/42-2/'
                result = json.loads(captured[prefix + 'result.json'][0])
                self.assertEqual(result['status'], status)
                self.assertEqual(result['run_attempt'], 2)
                self.assertEqual(result['artifacts'][0]['sha256'], publisher.sha256(root / 'macos-arm64.zip'))
                self.assertNotIn(b'DO NOT PUBLISH', b''.join(value[0] for value in captured.values()))
                page = captured[prefix][0].decode()
                self.assertIn('href="https://github.com/example/project/commit/' + 'a' * 40 + '"', page)
                self.assertIn('href="https://github.com/example/project/actions/runs/42">run 42</a>', page)
                self.assertIn('href="https://github.com/example/project/pull/7">PR #7: Fix &lt;script&gt; &amp; &quot;quotes&quot;</a>', page)
                self.assertLess(page.index('PR #7'), page.index('<strong>'))
                self.assertNotIn('<script>', page)
                self.assertFalse(captured[prefix][1])
                self.assertTrue(captured[prefix + 'result.json'][1])
                self.assertFalse(captured['a' * 40 + '/'][1])
                self.assertEqual(captured['robots.txt'][0], b'User-agent: *\nDisallow: /\n')

    def test_page_refresh_changes_only_presentation(self):
        stored = {**report(), 'artifacts': []}
        with patch.object(publisher, 'put') as put:
            publisher.refresh_run_page(stored, 'New title')
            put.assert_called_once()
            self.assertEqual(put.call_args.args[1:], ('a' * 40 + '/42-2/', False))

    def test_commit_index_skips_incomplete_runs_and_labels_completed_runs(self):
        source = 'b' * 40
        with tempfile.TemporaryDirectory() as temporary:
            captured = {}
            def put(path, key, immutable=True):
                captured[key] = path.read_text()
                return {}
            def aws(*args):
                if args[1] == 'list-objects-v2':
                    return subprocess.CompletedProcess([], 0, json.dumps({'CommonPrefixes': [
                        {'Prefix': source + '/41-1/'}, {'Prefix': source + '/42-1/'}]}).encode())
                if source + '/41-1/result.json' in args:
                    raise subprocess.CalledProcessError(254, ['aws'], stderr=b'An error occurred (NoSuchKey)')
                Path(args[-1]).write_text('{"status":"cancelled"}')
                return subprocess.CompletedProcess([], 0, b'{}')
            with patch.object(publisher, 'put', side_effect=put), patch.object(publisher, 'aws', side_effect=aws):
                publisher.publish(Path(temporary), {**report(source), 'pull_request': None, 'pull_request_title': None})
            self.assertNotIn('PR #', captured[source + '/42-2/'])
            self.assertIn('href="https://github.com/example/project/commit/' + source + '"', captured[source + '/'])
            self.assertNotIn('41-1', captured[source + '/'])
            self.assertIn('42-1', captured[source + '/'])
            self.assertIn('cancelled', captured[source + '/'])


if __name__ == '__main__':
    unittest.main()
