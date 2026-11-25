import io
import json
import os
from pathlib import Path
import signal
import stat
import subprocess
import tempfile
import tomllib
import unittest
import zipfile
from unittest.mock import Mock, patch
from contextlib import redirect_stdout

import fuzz
import fuzz_results
import publish

SOURCE = 'a' * 40


def manifest():
    return {'restore': 'success', 'baseline': {'targets': {}}, 'target_triple': fuzz.TRIPLE,
            'toolchain': {'rustc': 'rustc fixture', 'cargo_fuzz': 'cargo-fuzz 0.13.2', 'channel': 'nightly-2026-09-21'},
            'targets': {target: {'status': 'success', 'exit_code': 0, 'starting_inputs': 0}
                        for target in fuzz.TARGETS}}


def evidence(root, failed=None):
    root.mkdir(parents=True, exist_ok=True)
    value = manifest()
    for target in fuzz.TARGETS:
        for kind in ('corpus', 'crashes'):
            entries = {fuzz.digest(b'seed'): b'seed'} if kind == 'corpus' else {}
            if target == failed and kind == 'crashes':
                entries = {'crash-' + fuzz.digest(b'failure'): b'failure'}
            value['targets'][target][kind] = fuzz.write_bundle(root / f'{target}-{kind}.zip', entries, kind == 'crashes')
        if target == failed:
            value['targets'][target].update(status='failure', exit_code=77)
    fuzz.write_json(root / 'manifest.json', value)
    fuzz.write_json(root / 'provenance.json', {'source_commit': SOURCE, 'head_commit': SOURCE,
                                             'run_id': 42, 'run_attempt': 2, 'pull_request': None})
    (root / 'Cargo.lock').write_text('version = 4\n')
    return value


class Corpus(unittest.TestCase):
    def test_registered_targets_match_cargo_and_fit_transfer_budget(self):
        manifest = Path(__file__).resolve().parent.parent / 'fuzz/Cargo.toml'
        targets = {entry['name'] for entry in tomllib.loads(manifest.read_text())['bin']}
        self.assertEqual(set(fuzz.TARGETS), targets)
        # Both archives: payload plus bounded ZIP headers, digest names and
        # per-entry compression overhead. Keep ample room for the manifest.
        header_bytes = 256 * (fuzz.MAX_INPUTS + fuzz.MAX_CRASHES)
        bound = len(targets) * (2 * fuzz.MAX_BYTES + header_bytes) + fuzz.MAX_METADATA + fuzz.MAX_LOCK
        self.assertLess(bound, 256 * 1024 * 1024)

    def setUp(self):
        environment = patch.dict(os.environ, {'GITHUB_REPOSITORY': 'example/project'})
        environment.start()
        self.addCleanup(environment.stop)

    def test_toolchain_uses_distribution_date_instead_of_compiler_commit_date(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            channel = root / 'lib/rustlib/multirust-channel-manifest.toml'
            channel.parent.mkdir(parents=True)
            channel.write_text('manifest-version = "2"\ndate = "2026-09-21"\n')
            with patch.object(fuzz.subprocess, 'check_output', side_effect=[
                    'rustc 1.100.0-nightly (abcdef 2026-09-19)', 'cargo-fuzz 0.13.2', str(root)]):
                self.assertEqual(fuzz.toolchain()['channel'], 'nightly-2026-09-21')

    def test_complete_reproduction_snippets_have_valid_shell_syntax(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            value = evidence(root, failed='decode_view')
            report = {'fuzz': value, 'source_commit': SOURCE, 'run_id': 42, 'run_attempt': 2}
            for crashes in (False, True):
                code = fuzz_results.reproduction(report, 'decode_view', crashes)
                subprocess.run(['bash', '-n'], input=code, text=True, check=True)
                self.assertIn('gh repo clone example/project', code)
                self.assertIn('/42-2/fuzz/Cargo.lock', code)
                self.assertIn('set -eu', code)

    def test_bundle_deduplicates_and_is_deterministic(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            directory = root / 'inputs'
            directory.mkdir()
            (directory / 'seed').write_bytes(b'one')
            (directory / 'duplicate').write_bytes(b'one')
            (directory / 'other').write_bytes(b'two')
            values = fuzz.inputs(directory)
            self.assertEqual(len(values), 2)
            first = fuzz.write_bundle(root / 'one.zip', values)
            second = fuzz.write_bundle(root / 'two.zip', dict(reversed(list(values.items()))))
            self.assertEqual(first, second)
            self.assertEqual(fuzz.read_bundle(root / 'one.zip'), values)

    def test_bundle_rejects_bad_names_links_hashes_and_capacity(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / 'bad.zip'
            for name, mode in [('../outside', stat.S_IFREG), ('a' * 64, stat.S_IFLNK),
                               ('a' * 64, stat.S_IFREG), ('/absolute', stat.S_IFREG)]:
                with self.subTest(name=name, mode=mode):
                    with zipfile.ZipFile(path, 'w') as archive:
                        info = zipfile.ZipInfo(name)
                        info.external_attr = mode << 16
                        archive.writestr(info, b'bytes')
                    with self.assertRaises(ValueError):
                        fuzz.read_bundle(path)
            values = {fuzz.digest(b'one'): b'one', fuzz.digest(b'two'): b'two'}
            fuzz.write_bundle(path, values)
            for key, limit in [('MAX_INPUTS', 1), ('MAX_INPUT', 2), ('MAX_BYTES', 5), ('MAX_ARCHIVE', 30)]:
                with self.subTest(limit=key), patch.object(fuzz, key, limit), self.assertRaises(ValueError):
                    fuzz.read_bundle(path)
            with patch.object(fuzz, 'MAX_BYTES', 5), self.assertRaises(ValueError):
                fuzz.write_bundle(path, values)

    def test_symlink_input_and_duplicate_archive_entry_are_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            directory = root / 'inputs'
            directory.mkdir()
            (directory / 'link').symlink_to('/etc/passwd')
            with self.assertRaises(ValueError):
                fuzz.inputs(directory)
            path = root / 'duplicate.zip'
            with zipfile.ZipFile(path, 'w') as archive:
                archive.writestr(fuzz.digest(b'x'), b'x')
                with self.assertWarns(UserWarning):
                    archive.writestr(fuzz.digest(b'x'), b'x')
            with self.assertRaises(ValueError):
                fuzz.read_bundle(path)

    def test_restore_uses_verified_snapshots_and_never_masks_fetch_errors(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            transfer = root / 'transfer'
            transfer.mkdir()
            fuzz.write_json(transfer / 'manifest.json', {**manifest(), 'restore': 'pending'})
            archive = root / 'snapshot.zip'
            values = {fuzz.digest(b'previous discovery'): b'previous discovery'}
            description = fuzz.write_bundle(archive, values)
            key = f'fuzz/corpus/decode_view/{description["sha256"]}.zip'
            baseline = {'targets': {'decode_view': {**description, 'path': key}}}
            def fetch(origin, requested, path, limit):
                path.write_bytes(json.dumps(baseline).encode() if requested == fuzz.BASELINE_KEY else archive.read_bytes())
            with patch.object(fuzz, 'ROOT', root / 'fuzz'), patch.object(fuzz, 'fetch', side_effect=fetch):
                fuzz.restore(transfer, 'https://ci.example.test')
            self.assertEqual(fuzz.inputs(root / 'fuzz/corpus/decode_view'), values)
            self.assertEqual(fuzz.read_json(transfer / 'manifest.json')['baseline'], baseline)
            for error in [subprocess.CalledProcessError(22, ['curl']), subprocess.TimeoutExpired(['curl'], 30)]:
                fuzz.write_json(transfer / 'manifest.json', {**manifest(), 'restore': 'pending'})
                with patch.object(fuzz, 'fetch', side_effect=error), self.assertRaises(type(error)):
                    fuzz.restore(transfer, 'https://ci.example.test')
                self.assertEqual(fuzz.read_json(transfer / 'manifest.json')['restore'], 'pending')
            baseline['targets']['decode_view']['sha256'] = 'b' * 64
            with patch.object(fuzz, 'fetch', side_effect=fetch), self.assertRaises(ValueError):
                fuzz.restore(transfer, 'https://ci.example.test')

    def test_restore_selects_checkout_targets_without_fetching_other_corpora(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            transfer = root / 'transfer'
            transfer.mkdir()
            fuzz.write_json(transfer / 'manifest.json', {**manifest(), 'restore': 'pending'})
            archive = root / 'snapshot.zip'
            values = {fuzz.digest(b'discovery'): b'discovery'}
            description = fuzz.write_bundle(archive, values)
            baseline = {'targets': {target: {**description,
                'path': f'fuzz/corpus/{target}/{description["sha256"]}.zip'}
                for target in ('decode_view', 'another_checkout')}}
            fetched = []
            def fetch(origin, key, path, limit):
                fetched.append(key)
                path.write_bytes(json.dumps(baseline).encode() if key == fuzz.BASELINE_KEY else archive.read_bytes())
            with patch.object(fuzz, 'ROOT', root / 'fuzz'), patch.object(fuzz, 'fetch', side_effect=fetch):
                fuzz.restore(transfer, 'https://ci.example.test')
            self.assertEqual(fetched, [fuzz.BASELINE_KEY, baseline['targets']['decode_view']['path']])
            self.assertEqual(fuzz.inputs(root / 'fuzz/corpus/decode_view'), values)
            self.assertFalse((root / 'fuzz/corpus/another_checkout').exists())
            self.assertEqual(fuzz.read_json(transfer / 'manifest.json')['baseline'],
                             {'targets': {'decode_view': baseline['targets']['decode_view']}})
            # Even unused index entries must have safe names and matching paths.
            for name in ('../outside', '/absolute', '<script>', 'a' * 65):
                with self.subTest(name=name), self.assertRaises(ValueError):
                    fuzz.check_baseline({'targets': {name: baseline['targets']['another_checkout']}})
            with patch.object(fuzz, 'MAX_BASELINE_TARGETS', 1), self.assertRaises(ValueError):
                fuzz.check_baseline(baseline)
            baseline['targets']['another_checkout']['path'] = 'outside.zip'
            with self.assertRaises(ValueError):
                fuzz.check_baseline(baseline)

    def test_runner_continues_after_crash_and_kills_timed_out_process_group(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            transfer = root / 'transfer'
            transfer.mkdir()
            fuzz.write_json(transfer / 'manifest.json', manifest())
            processes = [Mock(pid=index + 100) for index in range(len(fuzz.TARGETS))]
            for process in processes:
                process.wait.return_value = 0
            processes[0].wait.return_value = 77
            processes[1].wait.side_effect = [subprocess.TimeoutExpired(['cargo'], 180), -9]
            with patch.object(fuzz, 'ROOT', root / 'fuzz'), \
                 patch.object(fuzz, 'toolchain', return_value=manifest()['toolchain']), \
                 patch.object(fuzz.subprocess, 'run'), patch.object(fuzz.subprocess, 'Popen', side_effect=processes) as popen, \
                 patch.object(fuzz.os, 'killpg') as kill:
                self.assertEqual(fuzz.run(transfer), 1)
            kill.assert_called_once_with(101, signal.SIGKILL)
            self.assertEqual(popen.call_count, len(fuzz.TARGETS))
            value = fuzz.read_json(transfer / 'manifest.json')
            self.assertEqual([value['targets'][target]['status'] for target in fuzz.TARGETS],
                             ['failure', 'timeout'] + ['success'] * (len(fuzz.TARGETS) - 2))

    def test_full_corpus_does_not_prevent_crash_collection(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            transfer = root / 'transfer'
            transfer.mkdir()
            fuzz.write_json(transfer / 'manifest.json', manifest())
            crash = root / 'fuzz/artifacts/decode_view'
            crash.mkdir(parents=True)
            (crash / ('crash-' + 'a' * 40)).write_bytes(b'failure')
            corpus = root / 'fuzz/corpus/decode_view'
            corpus.mkdir(parents=True)
            (corpus / 'too-big').write_bytes(b'x' * 20)
            with patch.object(fuzz, 'ROOT', root / 'fuzz'), patch.object(fuzz, 'MAX_INPUT', 10):
                self.assertEqual(fuzz.collect(transfer), 1)
            value = fuzz.read_json(transfer / 'manifest.json')
            self.assertIsNone(value['targets']['decode_view']['corpus'])
            self.assertEqual(value['targets']['decode_view']['crashes']['inputs'], 1)
            self.assertFalse((transfer / 'decode_view-corpus.zip').exists())

    def test_runner_admits_seed_growth_above_the_archive_budget(self):
        # Restoring a nearly full snapshot and adding seeds must reach fuzzing
        # and compaction instead of failing the starting-input census.
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            transfer = root / 'transfer'
            transfer.mkdir()
            fuzz.write_json(transfer / 'manifest.json', manifest())
            corpus = root / 'fuzz/corpus/package_contract'
            corpus.mkdir(parents=True)
            (corpus / 'restored').write_bytes(b'aaaa')
            (corpus / 'seed').write_bytes(b'bbbb')
            with patch.object(fuzz, 'ROOT', root / 'fuzz'), patch.object(fuzz, 'MAX_BYTES', 6), \
                 patch.object(fuzz, 'TARGETS', ('package_contract',)), \
                 patch.object(fuzz, 'toolchain', return_value=manifest()['toolchain']), \
                 patch.object(fuzz.subprocess, 'run'), \
                 patch.object(fuzz.subprocess, 'Popen', return_value=Mock(wait=Mock(return_value=0))) as popen:
                self.assertEqual(fuzz.run(transfer), 0)
            self.assertEqual(popen.call_count, 1)
            self.assertEqual(fuzz.read_json(transfer / 'manifest.json')['targets']['package_contract']['starting_inputs'], 2)

    def test_collection_compacts_an_oversized_corpus_and_preserves_originals(self):
        # The raw corpus exceeds the archive budget; only the minimized output
        # is admitted to the transfer, without deleting the worker's originals.
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            transfer = root / 'transfer'
            transfer.mkdir()
            fuzz.write_json(transfer / 'manifest.json', manifest())
            corpus = root / 'fuzz/corpus/package_contract'
            corpus.mkdir(parents=True)
            (corpus / 'one').write_bytes(b'aaaa')
            (corpus / 'two').write_bytes(b'bbbb')
            def merge(command, **kwargs):
                index = command.index('package_contract')
                destination = Path(command[index + 1])
                self.assertEqual(Path(command[index + 2]), corpus.resolve())
                self.assertIn('-merge=1', command)
                self.assertEqual(kwargs, {'start_new_session': True})
                (destination / 'minimal').write_bytes(b'a')
                return Mock(wait=Mock(return_value=0))
            log = io.StringIO()
            with patch.object(fuzz, 'ROOT', root / 'fuzz'), patch.object(fuzz, 'MAX_BYTES', 6), \
                 patch.object(fuzz.subprocess, 'Popen', side_effect=merge) as popen, redirect_stdout(log):
                self.assertEqual(fuzz.collect(transfer), 0)
                self.assertEqual(fuzz.read_bundle(transfer / 'package_contract-corpus.zip'), {fuzz.digest(b'a'): b'a'})
            self.assertEqual(popen.call_count, 1)
            self.assertIn('before compaction 2 files, 8 bytes', log.getvalue())
            self.assertIn('after compaction 1 inputs, 1 bytes', log.getvalue())
            self.assertEqual((corpus / 'one').read_bytes(), b'aaaa')
            self.assertEqual((corpus / 'two').read_bytes(), b'bbbb')

    def test_compaction_errors_preserve_reproducers_and_other_targets(self):
        # Timeouts, nonzero exits and still-oversized results cannot turn a
        # collection into success or prevent independent crash collection.
        for failure in ('timeout', 'exit', 'capacity'):
            with self.subTest(failure=failure), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                transfer = root / 'transfer'
                transfer.mkdir()
                fuzz.write_json(transfer / 'manifest.json', manifest())
                corpus = root / 'fuzz/corpus/package_contract'
                corpus.mkdir(parents=True)
                (corpus / 'seed').write_bytes(b'aaaa')
                crash = root / 'fuzz/artifacts/package_contract'
                crash.mkdir(parents=True)
                (crash / 'crash-abcd').write_bytes(b'bug')
                process = Mock(pid=123, wait=Mock(return_value=77 if failure == 'exit' else 0))
                if failure == 'timeout':
                    process.wait.side_effect = [subprocess.TimeoutExpired(['merge'], fuzz.COMPACT_TIMEOUT), -9]
                def merge(command, **kwargs):
                    destination = Path(command[command.index('package_contract') + 1])
                    (destination / 'oversized').write_bytes(b'x' * 7)
                    return process
                with patch.object(fuzz, 'ROOT', root / 'fuzz'), patch.object(fuzz, 'MAX_BYTES', 6), \
                     patch.object(fuzz.subprocess, 'Popen', side_effect=merge), patch.object(fuzz.os, 'killpg') as kill:
                    self.assertEqual(fuzz.collect(transfer), 1)
                value = fuzz.read_json(transfer / 'manifest.json')['targets']
                self.assertIsNone(value['package_contract']['corpus'])
                self.assertEqual(value['package_contract']['crashes']['inputs'], 1)
                self.assertIsNotNone(value['abi_reader']['corpus'])
                self.assertEqual((corpus / 'seed').read_bytes(), b'aaaa')
                if failure == 'timeout':
                    kill.assert_called_once_with(123, signal.SIGKILL)
                    self.assertEqual(process.wait.call_args_list[0].kwargs, {'timeout': fuzz.COMPACT_TIMEOUT})
                else:
                    kill.assert_not_called()

    def test_crash_resistant_merge_cannot_hide_a_new_reproducer(self):
        # libFuzzer merge can succeed after a child crashes. Collect its new
        # artifact and fail the target even when the outer process exits zero.
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            transfer = root / 'transfer'
            transfer.mkdir()
            fuzz.write_json(transfer / 'manifest.json', manifest())
            corpus = root / 'fuzz/corpus/package_contract'
            corpus.mkdir(parents=True)
            (corpus / 'seed').write_bytes(b'seed')
            def merge(command, **kwargs):
                crash = root / 'fuzz/artifacts/package_contract'
                crash.mkdir(parents=True)
                (crash / 'crash-abcd').write_bytes(b'new bug')
                return Mock(wait=Mock(return_value=0))
            with patch.object(fuzz, 'ROOT', root / 'fuzz'), patch.object(fuzz.subprocess, 'Popen', side_effect=merge):
                self.assertEqual(fuzz.collect(transfer), 1)
            value = fuzz.read_json(transfer / 'manifest.json')['targets']['package_contract']
            self.assertEqual(value['status'], 'failure')
            self.assertEqual(value['crashes']['inputs'], 1)

    def test_working_corpus_is_bounded_before_merge(self):
        # The larger working budget admits growth, but invalid files and the
        # working count/byte ceilings fail before executing any input.
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            corpus = root / 'corpus/package_contract'
            corpus.mkdir(parents=True)
            (corpus / 'one').write_bytes(b'aaaa')
            (corpus / 'two').write_bytes(b'bbbb')
            with patch.object(fuzz, 'ROOT', root), patch.object(fuzz.subprocess, 'Popen') as popen:
                for key, limit in [('MAX_WORKING_INPUTS', 1), ('MAX_WORKING_BYTES', 7), ('MAX_INPUT', 3)]:
                    with self.subTest(limit=key), patch.object(fuzz, key, limit), self.assertRaises(ValueError):
                        fuzz.compact('package_contract')
                (corpus / 'link').symlink_to(corpus / 'one')
                with self.assertRaises(ValueError):
                    fuzz.compact('package_contract')
                popen.assert_not_called()

    def test_manifest_rejects_unknown_files_targets_and_false_success(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            value = evidence(root)
            self.assertEqual(fuzz_results.validate(root), value)
            (root / 'raw-log.txt').write_text('not a product')
            with self.assertRaises(ValueError):
                fuzz_results.validate(root)
            (root / 'raw-log.txt').unlink()
            value['targets']['unknown'] = value['targets']['decode_view']
            fuzz.write_json(root / 'manifest.json', value)
            with self.assertRaises(ValueError):
                fuzz_results.validate(root)
            del value['targets']['unknown']
            description = value['targets']['decode_view']['corpus']
            value['baseline']['targets']['another_checkout'] = {**description,
                'path': f'fuzz/corpus/another_checkout/{description["sha256"]}.zip'}
            fuzz.write_json(root / 'manifest.json', value)
            with self.assertRaises(ValueError):
                fuzz_results.validate(root)
            value['baseline']['targets'].clear()
            value['targets']['decode_view']['exit_code'] = 77
            fuzz.write_json(root / 'manifest.json', value)
            with self.assertRaises(ValueError):
                fuzz_results.validate(root)

    def test_failed_target_page_includes_reproducer_and_snapshot_identity(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            value = evidence(root, failed='decode_view')
            description = value['targets']['abi_reader']['corpus']
            value['baseline']['targets']['abi_reader'] = {**description,
                'path': f'fuzz/corpus/abi_reader/{description["sha256"]}.zip'}
            page = fuzz_results.render({'fuzz': value, 'source_commit': SOURCE, 'run_id': 42, 'run_attempt': 2,
                                        'artifacts': [{'path': 'run/fuzz/Cargo.lock'}]})
            self.assertIn('decode_view-crashes.zip', page)
            self.assertIn('--debug-assertions &quot;$pb_fuzz_target&quot; &quot;$pb_fuzz_input&quot;', page)
            self.assertIn('unzip inputs.zip -d &quot;playbit/fuzz/corpus/$pb_fuzz_target&quot;', page)
            self.assertIn('gh repo clone example/project', page)
            self.assertIn('git checkout --detach ' + SOURCE, page)
            self.assertIn('rustup toolchain install nightly-2026-09-21', page)
            self.assertIn('install cargo-fuzz --version 0.13.2 --locked', page)
            self.assertIn('/42-2/fuzz/Cargo.lock&quot; -o playbit/fuzz/Cargo.lock', page)
            self.assertIn(SOURCE, page)
            self.assertIn(description['sha256'], page)
            self.assertIn('Cargo.lock for this run', page)
            self.assertNotIn('abi_reader-crashes.zip', page)
            value['targets']['decode_view']['corpus'] = None
            self.assertIn('collection incomplete', fuzz_results.render({'fuzz': value, 'source_commit': SOURCE, 'run_id': 42, 'run_attempt': 2, 'artifacts': []}))


class Promotion(unittest.TestCase):
    def test_compaction_removes_restored_inputs_and_preserves_concurrent_discoveries(self):
        # Replace only the baseline that this run exercised. A newer baseline's
        # discoveries survive, and retries after either publication boundary
        # produce the same snapshot.
        for concurrent in (False, True):
            with self.subTest(concurrent=concurrent), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                stored = {}
                def snapshot(entries):
                    description = fuzz.write_bundle(root / 'snapshot.zip', {fuzz.digest(data): data for data in entries})
                    key = f'fuzz/corpus/package_contract/{description["sha256"]}.zip'
                    stored[key] = (root / 'snapshot.zip').read_bytes()
                    return {**description, 'path': key}
                restored = snapshot([b'obsolete', b'keep'])
                current = snapshot([b'obsolete', b'keep', b'parallel']) if concurrent else restored
                incoming = snapshot([b'keep', b'new'])
                retired = {**current, 'path': current['path'].replace('/package_contract/', '/retired_target/')}
                stored[fuzz.BASELINE_KEY] = json.dumps({'targets': {
                    'package_contract': current, 'retired_target': retired}}).encode()
                value = manifest()
                for target in value['targets'].values():
                    target['status'] = 'failure'
                value['baseline']['targets']['package_contract'] = restored
                value['targets']['package_contract'].update(
                    status='success', corpus={k: v for k, v in incoming.items() if k != 'path'},
                    crashes={'inputs': 0})
                report = {'source_commit': SOURCE, 'run_id': 42, 'run_attempt': 1, 'fuzz': value}
                run = {'event': 'push', 'head_branch': 'main', 'head_repository': {'full_name': 'example/project'}}
                stored[f'{SOURCE}/42-1/fuzz/package_contract-corpus.zip'] = stored[incoming['path']]
                def read_json(key, destination):
                    return json.loads(stored[key]) if key in stored else None
                def read_bundle(item, destination):
                    destination.write_bytes(stored[item['path']])
                    return fuzz.read_bundle(destination)
                def put(path, key, immutable=True):
                    stored[key] = path.read_bytes()
                with patch.dict(os.environ, {'GITHUB_REPOSITORY': 'example/project'}), \
                     patch.object(fuzz_results, 'read_s3_json', side_effect=read_json), \
                     patch.object(fuzz_results, 'read_s3_bundle', side_effect=read_bundle), \
                     patch.object(publish, 'put', side_effect=put):
                    fuzz_results.promote(report, run)
                    self.assertNotIn('retired_target', json.loads(stored[fuzz.BASELINE_KEY])['targets'])
                    result = json.loads(stored[fuzz.BASELINE_KEY])['targets']['package_contract']
                    self.assertEqual(set(read_bundle(result, root / 'result.zip').values()),
                                     {b'keep', b'new', b'parallel'} if concurrent else {b'keep', b'new'})
                    before = dict(stored)
                    fuzz_results.promote(report, run)
                    self.assertEqual(stored, before)
                    del stored['fuzz/promotions/42-1.json']
                    fuzz_results.promote(report, run)
                    self.assertEqual(stored, before)

    def test_capacity_skips_one_promotion_with_warning_and_keeps_evidence(self):
        # A concurrent union can still exceed the bounded snapshot. Publication
        # completes with a visible maintenance warning and keeps the old baseline.
        for bound in ('MAX_BYTES', 'MAX_INPUTS'):
            with self.subTest(bound=bound), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                current = {'sha256': 'b' * 64, 'bytes': 200, 'inputs': 1, 'expanded_bytes': 4,
                           'path': 'fuzz/corpus/package_contract/' + 'b' * 64 + '.zip'}
                baseline = {'targets': {'package_contract': current}}
                value = manifest()
                for item in value['targets'].values():
                    item['status'] = 'failure'
                value['targets']['package_contract'].update(status='success', corpus={'sha256': 'c' * 64},
                                                           crashes={'inputs': 0})
                value['targets']['abi_reader'].update(status='success', corpus={'sha256': 'd' * 64},
                                                     crashes={'inputs': 0})
                report = {'source_commit': SOURCE, 'run_id': 42, 'run_attempt': 1, 'fuzz': value}
                run = {'event': 'push', 'head_branch': 'main', 'head_repository': {'full_name': 'example/project'}}
                written = {}
                def put(path, key, immutable=True):
                    written[key] = path.read_bytes()
                log = io.StringIO()
                with patch.dict(os.environ, {'GITHUB_REPOSITORY': 'example/project',
                                              'GITHUB_STEP_SUMMARY': str(root / 'summary')}), \
                     patch.object(fuzz, bound, 6 if bound == 'MAX_BYTES' else 1), \
                     patch.object(fuzz_results, 'read_s3_json', side_effect=[None, baseline]), \
                     patch.object(fuzz_results, 'read_s3_bundle', side_effect=[
                         {fuzz.digest(b'x'): b'x'},
                         {fuzz.digest(b'old!'): b'old!'}, {fuzz.digest(b'new!'): b'new!'}]), \
                     patch.object(publish, 'put', side_effect=put), redirect_stdout(log):
                    fuzz_results.promote(report, run)
                updated = json.loads(written[fuzz.BASELINE_KEY])['targets']
                self.assertEqual(updated['package_contract'], current)
                self.assertEqual(updated['abi_reader']['inputs'], 1)
                self.assertEqual(set(written), {fuzz.BASELINE_KEY, 'fuzz/promotions/42-1.json',
                                               updated['abi_reader']['path']})
                self.assertIn('::warning::package_contract: shared corpus promotion needs compaction', log.getvalue())
                self.assertIn(f'{SOURCE}/42-1/fuzz/package_contract-corpus.zip', (root / 'summary').read_text())

    def test_overlapping_runs_union_inputs_retries_and_failed_targets(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            stored = {fuzz.BASELINE_KEY: b'{"targets":{}}'}
            environment = {'GITHUB_REPOSITORY': 'example/project'}
            run = {'event': 'push', 'head_branch': 'main', 'head_repository': {'full_name': 'example/project'}}
            def read_json(key, destination):
                return json.loads(stored[key]) if key in stored else None
            def read_bundle(item, destination):
                destination.write_bytes(stored[item['path']])
                return fuzz.read_bundle(destination)
            def put(path, key, immutable=True):
                data = path.read_bytes()
                if immutable and key in stored:
                    self.assertEqual(stored[key], data)
                stored[key] = data
            reports = []
            for number, data in [(42, b'first run'), (43, b'second run')]:
                value = evidence(root / str(number), failed='decode_view')
                description = fuzz.write_bundle(root / 'input.zip', {fuzz.digest(data): data})
                value['targets']['abi_reader']['corpus'] = description
                report = {'source_commit': SOURCE, 'run_id': number, 'run_attempt': 1, 'fuzz': value}
                for target in fuzz.TARGETS:
                    archive = root / 'input.zip' if target == 'abi_reader' else root / str(number) / f'{target}-corpus.zip'
                    stored[f'{SOURCE}/{number}-1/fuzz/{target}-corpus.zip'] = archive.read_bytes()
                reports.append(report)
            with patch.dict(os.environ, environment), patch.object(fuzz_results, 'read_s3_json', side_effect=read_json), \
                 patch.object(fuzz_results, 'read_s3_bundle', side_effect=read_bundle), patch.object(publish, 'put', side_effect=put):
                fuzz_results.promote(reports[0], run)
                fuzz_results.promote(reports[1], run)
                before = dict(stored)
                fuzz_results.promote(reports[0], run)
                self.assertEqual(stored, before)
                baseline = json.loads(stored[fuzz.BASELINE_KEY])
                self.assertNotIn('decode_view', baseline['targets'])
                item = baseline['targets']['abi_reader']
                self.assertEqual(item['inputs'], 2)
                # A crash after publishing latest but before its receipt is safe.
                del stored['fuzz/promotions/43-1.json']
                fuzz_results.promote(reports[1], run)
                self.assertEqual(stored, before)
                # PR and fork reports are never promoted.
                for change in [{'event': 'pull_request'}, {'head_branch': 'feature'},
                               {'head_repository': {'full_name': 'other/fork'}}]:
                    fuzz_results.promote(reports[1], {**run, **change})
                    self.assertEqual(stored, before)

    def test_bootstrap_cannot_overwrite_an_existing_baseline_and_uses_short_cache(self):
        with patch.object(publish, 'BUCKET', 'fixture'), \
             patch.object(publish, 'aws', side_effect=subprocess.CalledProcessError(1, ['aws'])) as aws, \
             patch.object(publish, 'verify_object', side_effect=ValueError('different baseline')) as verify:
            with self.assertRaises(ValueError):
                fuzz_results.initialize_baseline()
            self.assertIn('--if-none-match', aws.call_args.args)
            self.assertIn('public,max-age=60', aws.call_args.args)
            verify.assert_called_once()

    def test_failed_snapshot_upload_does_not_advance_manifest(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            value = evidence(root)
            report = {'source_commit': SOURCE, 'run_id': 42, 'run_attempt': 1, 'fuzz': value}
            run = {'event': 'push', 'head_branch': 'main', 'head_repository': {'full_name': 'example/project'}}
            with patch.dict(os.environ, {'GITHUB_REPOSITORY': 'example/project'}), \
                 patch.object(fuzz_results, 'read_s3_json', side_effect=[None, {'targets': {}}]), \
                 patch.object(fuzz_results, 'read_s3_bundle', return_value={fuzz.digest(b'x'): b'x'}), \
                 patch.object(publish, 'put', side_effect=RuntimeError('upload failed')) as put:
                with self.assertRaises(RuntimeError):
                    fuzz_results.promote(report, run)
                self.assertEqual(put.call_count, 1)
                self.assertNotEqual(put.call_args.args[1], fuzz.BASELINE_KEY)


if __name__ == '__main__':
    unittest.main()
