"""Change selection and required-result contracts shared by local and hosted CI."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import ci


def selected(paths, private=True):
    return {name for name, item in ci.select(paths, private).items() if item['run']}


class Planner(unittest.TestCase):
    def test_check_dependencies_include_cross_language_consumers(self):
        # Whole check groups cover generated fixtures and non-Cargo consumers.
        cases = {
            'service/ap/src/main.rs': {'service', 'export'},
            'service/website/main.go': {'website', 'export'},
            'playbit/platform/web/src/scene.ts': {'web', 'macos', 'website', 'service', 'export'},
            'playbit/platform/ios/Host/GuestScene.swift': {'macos', 'web', 'service', 'clippy', 'style', 'export'},
            'playbit/platform/macos/package.sh': {'macos', 'clippy', 'packaging', 'export'},
            'service/infra/ci/ap-build-image/Dockerfile': {'service', 'image', 'export'},
            'service/playbot/playbot.py': {'playbot', 'export'},
            'service/playbot/capture.swift': {'playbot', 'export'},
            'service/playbot/agent-instructions.md': {'playbot', 'export'},
            'service/infra/release_controller.py': {'export'},
            'service/infra/test_release_controller.py': {'export'},
            'service/infra/release_state.py': {'export'},
            'service/infra/test_release_state.py': {'export'},
            'docs/manual/getting-started.md': {'website', 'export'},
            'playbit/src/desktop/sidebar_view.rs': {'macos', 'clippy', 'style', 'service', 'export'},
            'playbit/src/platform/ios/account.rs': {'macos', 'clippy', 'style', 'service', 'export'},
            'playbit/src/platform/web/account.rs': {'linux', 'web', 'style', 'service', 'export'},
            'playbit/src/bin/pb/main.rs': {'linux', 'macos', 'clippy', 'style', 'service', 'export'},
            'playbit/src/client/mod.rs': {'linux', 'macos', 'clippy', 'style', 'service', 'export'},
            'playbit/src/ui/mod.rs': {'linux', 'macos', 'clippy', 'style', 'service', 'export'},
            'playbit/src/ui/desktop/controls/render.rs': {'macos', 'clippy', 'style', 'service', 'export'},
            'playbit/sdk/rust/playbit.rs': {'linux', 'macos', 'clippy', 'service', 'export'},
            'playbit/sdk/toolchains/catalog.py': {'linux', 'macos', 'clippy', 'service', 'website', 'export'},
            'playbit/skills/new-app.md': {'macos', 'export'},
            'guests/todo/playbit.toml': {'linux', 'macos', 'web', 'clippy', 'style', 'service', 'export'},
            'playbit-api/src/lib.rs': {'linux', 'macos', 'web', 'clippy', 'style', 'service', 'website', 'export'},
            'playbit/kvdb/src/lib.rs': {'linux', 'macos', 'web', 'clippy', 'style', 'service', 'kvdb-coverage', 'export'},
            'playbit/trace/src/lib.rs': {'linux', 'macos', 'web', 'clippy', 'style', 'service', 'export'},
            'playbit/src/engine/mod.rs': {'linux', 'macos', 'clippy', 'style', 'service', 'export'},
            'playbit/src/channels.json': {'linux', 'macos', 'web', 'clippy', 'style', 'service', 'website', 'export'},
            'vendor/gpui/src/lib.rs': {'macos', 'clippy', 'service', 'export'},
            'vendor/accesskit_macos/src/lib.rs': {'macos', 'clippy', 'export'},
        }
        for path, expected in cases.items():
            with self.subTest(path=path):
                self.assertEqual(selected([path]), expected)
        self.assertEqual(selected(['service/playbot/playbot.py', 'service/ap/src/main.rs']),
                         {'playbot', 'service', 'export'})

    def test_unknown_and_shared_inputs_run_every_group(self):
        # Unclassified inputs cannot silently omit a consumer or target.
        for path in ('new-tool/file', 'new-tool/instructions.md', 'Cargo.toml', 'Cargo.lock',
                     'scripts/build-version.rs', '.github/workflows/public.yml', 'playbit/Cargo.toml',
                     'playbit/build.rs', 'playbit/new-core/file.rs',
                     'service/infra/release.py', 'service/infra/release_ap.py',
                     'service/infra/release_controller.py.new', 'service/playbot-extra/tool.py'):
            with self.subTest(path=path):
                self.assertEqual(selected([path]), set(ci.PUBLIC + ci.PRIVATE))
        self.assertEqual(selected(None), set(ci.PUBLIC + ci.PRIVATE))

    def test_long_histories_follow_storage_dependencies_and_default_to_full(self):
        # UI/adapters retain their ordinary tests without rebuilding the long
        # storage target. Core inputs and uncertain dependency changes keep it.
        for path in ('playbit/src/desktop/sidebar_view.rs', 'vendor/gpui/src/lib.rs',
                     'playbit/src/platform/ios/account.rs', 'playbit/platform/ios/Host/Runtime.swift',
                     'playbit/platform/web/src/scene.ts', 'playbit/src/bin/pb/main.rs',
                     'playbit/src/engine/service_call.rs', 'playbit/src/runtime/host/files.rs',
                     'playbit/src/network/ap/wire.rs',
                     'playbit/sdk/rust/playbit.rs', 'service/website/main.go',
                     'service/ap/src/main.rs', 'service/playbot/playbot.py', 'README.md'):
            with self.subTest(path=path):
                self.assertFalse(ci.storage_history([path])['run'])
        for path in ('playbit/src/store/volume_index.rs', 'playbit/src/package/mod.rs',
                     'playbit/src/lib.rs', 'playbit/src/identifier.rs', 'playbit/kvdb/src/lib.rs',
                     'playbit-api/src/lib.rs', 'playbit/trace/src/lib.rs', 'Cargo.lock',
                     'playbit/Cargo.toml', 'playbit/build.rs', 'scripts/ci/macos.sh',
                     'guests/todo/src/lib.rs', 'unknown/input'):
            with self.subTest(path=path):
                self.assertTrue(ci.storage_history([path])['run'])
        self.assertTrue(ci.storage_history(None)['run'])
        self.assertFalse(ci.storage_history([])['run'])
        self.assertTrue(ci.storage_history(['playbit/src/desktop/a.rs', 'playbit/src/store/a.rs'])['run'])
        # Prefix boundaries cannot turn a new shared module into a UI exclusion.
        self.assertTrue(ci.storage_history(['playbit/src/desktop_shared.rs'])['run'])

    def test_prose_skips_builds_and_public_selection_needs_no_private_tree(self):
        # Private source classification remains checked even for prose-only PRs.
        runbook = 'service/infra/ci/self-hosted/README.md'
        self.assertEqual(selected(['docs/building.md', 'README.md', runbook]), {'export'})
        self.assertEqual(selected([runbook, 'service/website/main.go']), {'website', 'export'})
        self.assertEqual(selected([runbook], private=False), set())
        self.assertFalse(ci.storage_history([runbook])['run'])
        self.assertEqual(selected([], private=False), set())
        self.assertEqual(selected(['playbit/platform/web/src/scene.ts'], private=False), {'web', 'macos'})
        self.assertEqual(selected(None, private=False), set(ci.PUBLIC))

    def test_rename_and_deletion_use_both_paths_from_real_git(self):
        # A rename across groups affects the old consumer as well as the new one.
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            def git(*args):
                return subprocess.check_output(['git', '-C', root, *args], stderr=subprocess.DEVNULL, text=True).strip()
            git('init', '-b', 'main')
            git('config', 'user.name', 'CI Test')
            git('config', 'user.email', 'ci-test@example.invalid')
            source = root / 'service/ap/a.txt'
            source.parent.mkdir(parents=True)
            source.write_text('same content\n')
            git('add', '.')
            git('commit', '-m', 'base')
            base = git('rev-parse', 'HEAD')
            target = root / 'playbit/platform/ios/a.txt'
            target.parent.mkdir(parents=True)
            source.rename(target)
            git('add', '-A')
            git('commit', '-m', 'move')
            with patch.object(ci, 'ROOT', root):
                value = ci.plan(base, private=True)
            self.assertEqual({name for name, item in value['groups'].items() if item['run']},
                             {'service', 'macos', 'web', 'clippy', 'style', 'export'})
            self.assertEqual(value['base'], base)
            self.assertEqual(value['head'], git('rev-parse', 'HEAD'))
            self.assertFalse(value['storage_history']['run'])
            with patch.object(ci, 'ROOT', root):
                full = ci.plan(base, private=True, full=True)
            self.assertEqual({name for name, item in full['groups'].items() if item['run']},
                             set(ci.PUBLIC + ci.PRIVATE))
            self.assertTrue(full['storage_history']['run'])
            # Deleting a storage input must still run its long workloads even
            # when its replacement lives entirely in a frontend directory.
            storage = root / 'playbit/src/store/deleted.rs'
            storage.parent.mkdir(parents=True)
            storage.write_text('storage source\n')
            git('add', '.')
            git('commit', '-m', 'storage')
            base = git('rev-parse', 'HEAD')
            storage.rename(root / 'playbit/platform/ios/moved.rs')
            git('add', '-A')
            git('commit', '-m', 'move storage')
            with patch.object(ci, 'ROOT', root):
                self.assertTrue(ci.plan(base, private=True)['storage_history']['run'])

    def test_github_outputs_carry_selected_checks_and_storage_workload(self):
        # Local plans and reusable/public GitHub workflows share the decision.
        for changed in (None, ['playbit/src/desktop/sidebar_view.rs'], ['playbit/src/store/mod.rs']):
            with self.subTest(changed=changed), tempfile.TemporaryDirectory() as temporary:
                output = Path(temporary) / 'output'
                value = dict(base='base', head='head', groups=ci.select(changed, private=True),
                             storage_history=ci.storage_history(changed))
                with patch.dict(os.environ, {'GITHUB_OUTPUT': str(output)}):
                    ci.github_outputs(value)
                actual = dict(line.split('=', 1) for line in output.read_text().splitlines())
                self.assertEqual(actual['storage_history'], str(value['storage_history']['run']).lower())
                self.assertEqual(set(json.loads(actual['selected'])), selected(changed))
                for name, item in value['groups'].items():
                    self.assertEqual(actual[name.replace('-', '_')], str(item['run']).lower())


class Results(unittest.TestCase):
    def test_selected_success_and_intentional_skip(self):
        # Only a successful selected job establishes the required result.
        ci.validate_results({'macos': True, 'web': False},
                            {'macos': {'result': 'success'}, 'web': {'result': 'skipped'}})
        for result in ('failure', 'cancelled', 'skipped', None):
            with self.subTest(result=result), self.assertRaises(ValueError):
                ci.validate_results({'macos': True}, {'macos': {'result': result}})
        with self.assertRaises(ValueError):
            ci.validate_results({'macos': True}, {})
        with self.assertRaises(ValueError):
            ci.validate_results({'web': False}, {'web': {'result': 'success'}})

    def test_gate_explains_propagated_dependency_failure(self):
        # Both gate levels identify the dependency whose failure they propagate.
        results = {name: {'result': 'skipped'} for name in ci.PRIVATE}
        results.update(plan={'result': 'success'}, web={'result': 'failure'}, test={'result': 'failure'})
        results['playbot'] = {'result': 'failure'}
        for job, selection in (('web', {'CI_EXPECTED': '{"web":true}'}),
                               ('test', {'CI_SELECTED': '["web"]'}),
                               ('playbot', {'CI_SELECTED': '["playbot"]'})):
            env = {'CI_RESULTS': json.dumps(results), **selection}
            with self.subTest(job=job), patch.dict(os.environ, env, clear=True):
                with self.assertRaises(ValueError) as failure:
                    ci.gate()
                self.assertIn('This gate only aggregates results', str(failure.exception))
                self.assertIn("inspect the listed jobs' logs", str(failure.exception))
                self.assertIn(f'{job}: expected success, got failure', str(failure.exception))

    def test_failed_or_missing_plan_cannot_turn_skips_green(self):
        # The aggregate must fail even when all dependent jobs were skipped.
        for result in ('failure', 'cancelled', 'skipped', None):
            results = {name: {'result': 'skipped'} for name in ci.PRIVATE + ('test',)}
            if result:
                results['plan'] = {'result': result}
            env = {'CI_RESULTS': json.dumps(results), 'CI_SELECTED': '[]'}
            with patch.dict(os.environ, env, clear=True), self.assertRaises(ValueError):
                ci.gate()


class ShellChecks(unittest.TestCase):
    def test_invalid_workload_flag_fails_before_running_checks(self):
        with patch.dict(os.environ, {'CI_STORAGE_HISTORY': 'typo'}):
            result = subprocess.run(['bash', ci.ROOT / 'scripts/ci/check.sh', 'tooling'],
                                    capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('CI_STORAGE_HISTORY must be true or false', result.stderr)

    def test_group_failure_preserves_exit_status_and_reports_duration(self):
        # A failed check must remain a failure after cleanup and timing output.
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            scripts = root / 'scripts/ci'
            scripts.mkdir(parents=True)
            shutil.copy(ci.ROOT / 'scripts/ci/check.sh', scripts / 'check.sh')
            (scripts / 'style.sh').write_text('exit 17\n')
            result = subprocess.run(['bash', scripts / 'check.sh', 'style'], capture_output=True, text=True)
            self.assertEqual(result.returncode, 17, result.stderr)
            self.assertIn('style: exit 17,', result.stdout)


if __name__ == '__main__':
    unittest.main()
