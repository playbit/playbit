"""The renderer gate must reject empty/upstream selections and propagate failures."""
import runpy
import re
import subprocess
from pathlib import Path
from unittest import TestCase, mock


main = runpy.run_path(str(Path(__file__).with_name('test-renderer.py')))['main']


class RendererSelection(TestCase):
    def test_live_trace_regressions_remain_in_the_renderer_gate(self):
        root = Path(__file__).resolve().parent.parent / 'vendor/gpui/src'
        for source in ('app/scene_preparation.rs', 'platform/mac/metal_atlas.rs',
                       'text_system/decoration_tests.rs', 'text_system/raster_bounds_tests.rs'):
            with self.subTest(source=source):
                tests = re.findall(
                    r'#\[(?:(?:crate|gpui)::)?test(?:\([^]]*\))?\]'
                    r'(?:\s*#\[[^\]]*\])*\s*(?:async\s+)?fn\s+(\w+)',
                    (root / source).read_text())
                self.assertTrue(tests)
                self.assertTrue(all(name.startswith('playbit_') for name in tests), tests)

    @mock.patch('subprocess.run')
    def test_runs_selected_regressions(self, run):
        run.return_value.stdout = 'text::playbit_layout: test\n\n1 test, 0 benchmarks\n'
        main()
        self.assertEqual(run.call_count, 2)
        self.assertIn('playbit_', run.call_args.args[0])
        self.assertTrue(run.call_args.kwargs['check'])

    @mock.patch('subprocess.run')
    def test_empty_or_upstream_selection_fails_before_execution(self, run):
        for listing in ('0 tests, 0 benchmarks\n', 'text::test_upstream: test\n'):
            run.reset_mock()
            run.return_value.stdout = listing
            with self.assertRaises(SystemExit):
                main()
            self.assertEqual(run.call_count, 1)

    @mock.patch('subprocess.run')
    def test_cargo_failure_is_not_hidden(self, run):
        run.side_effect = subprocess.CalledProcessError(1, ['cargo'])
        with self.assertRaises(subprocess.CalledProcessError):
            main()
