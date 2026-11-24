#!/usr/bin/env python3
"""Exercise exclusions, tier boundaries, and regression behavior with small sources."""
import copy
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('style_report', Path(__file__).with_name('style-report.py'))
style = importlib.util.module_from_spec(spec)
spec.loader.exec_module(style)


class StyleReportTests(unittest.TestCase):
    def measure(self, source, path='playbit/kvdb/src/db.rs'):
        return style.measure([(path, style.production_lines(source))])

    def test_test_module_exclusion_preserves_following_production(self):
        # Braces in strings/comments must not end a test module prematurely.
        source = '''#[cfg(test)]
mod tests {
    fn test() { let s = "}"; /* } */ None.unwrap(); }
}
#[cfg(test)]
mod external_tests;
fn decode_live() { assert!(true); }
'''
        metrics, funcs = self.measure(source)
        self.assertEqual(metrics['lines'], 1)
        self.assertEqual(metrics['unwrap'], 0)
        self.assertEqual([f['name'] for f in funcs], ['decode_live'])
        self.assertEqual(funcs[0]['line'], 7)

    def test_formatted_attributes_still_exclude_test_modules(self):
        source = '''#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "A bracket ] in a reason is not an attribute delimiter.",
)]
#[allow(clippy::items_after_test_module)]
mod tests {
    fn fixture() { assert!(true); None.unwrap(); }
}
fn decode_live() { assert!(true); }
'''
        metrics, funcs = self.measure(source)
        self.assertEqual(metrics['lines'], 1)
        self.assertEqual(metrics['assertions'], 1)
        self.assertEqual(metrics['unwrap'], 0)
        self.assertEqual([f['name'] for f in funcs], ['decode_live'])

    def test_metrics_and_multiline_function(self):
        # Count calls, declarations and FFI; ignore examples inside literal text.
        source = '''// fn fake() { assert!(false); None.unwrap(); }
const MAX_ITEMS: usize = 8;
const CAPACITY: usize = 3;
pub unsafe extern "C" fn decode_value(
    x: usize,
) -> usize {
    let text = r#"{ .unwrap() assert!(false) }"#;
    assert!(x < MAX_ITEMS);
    debug_assert_eq!(x, 1);
    unreachable!();
    Some(x).expect("known");
    Some(x).unwrap()
}
trait Read {\n    fn parse(&self);\n}
'''
        metrics, funcs = self.measure(source)
        self.assertEqual(metrics['functions'], 2)
        self.assertEqual(metrics['assertions'], 3)
        self.assertEqual(metrics['unwrap'], 1)
        self.assertEqual(metrics['expect'], 1)
        self.assertEqual(metrics['unsafe'], 1)
        self.assertEqual(metrics['limit_constants'], 2)
        self.assertEqual(funcs[0]['lines'], 10)
        self.assertEqual(funcs[1]['lines'], 1)

    def test_array_signature_does_not_end_at_its_semicolon(self):
        metrics, funcs = self.measure('fn encode() -> [u8; 16] {\n    [0; 16]\n}\n')
        self.assertEqual(funcs[0]['lines'], 3)

    def test_excluded_paths(self):
        for name in ('vendor/x.rs', 'target/x.rs', 'node_modules/x.rs', 'tests/x.rs',
                     'benchmarks/x.rs', 'benches/x.rs', 'tests.rs', 'x_tests.rs',
                     'test_support.rs', 'prepared_bench.rs'):
            self.assertFalse(style.production_file(Path(name)), name)
        self.assertTrue(style.production_file(Path('playbit/src/runtime/testing.rs')))

    def test_all_regressions_and_report_only_tiers(self):
        # Assertion density is report-only, even when a tier A count decreases.
        old, _ = self.measure('fn f() { assert!(true); }\n' * 100)
        new = copy.deepcopy(old)
        new['assertions'] -= 1
        new['assertions_per_function'] = .99
        new['unwrap'] = 1
        new['long_functions'] = [{'id': 'new', 'path': 'playbit/kvdb/src/db.rs', 'name': 'long', 'line': 5, 'lines': 80}]
        baseline = {'tier_a': {'playbit/kvdb/src': old}, 'directories': {'playbit/src/engine': old}}
        current = {'tier_a': {'playbit/kvdb/src': new}, 'directories': {'playbit/src/engine': new}}
        errors = style.violations(current, baseline)
        self.assertEqual(len(errors), 2)
        self.assertIn('playbit/kvdb/src: bare unwrap count increased from 0 to 1', errors)
        self.assertIn('playbit/kvdb/src/db.rs:5: long is 80 lines (limit 70)', errors)
        current['tier_a']['playbit/kvdb/src'] = old
        self.assertEqual(style.violations(current, baseline), [])

    def test_lower_assertion_density_is_report_only(self):
        old, _ = self.measure('fn f() { assert!(true); }\n')
        new, _ = self.measure('fn f() {}\nfn getter() {}\n')
        self.assertLess(new['assertions_per_function'], old['assertions_per_function'])
        self.assertEqual(style.violations({'tier_a': {'playbit/kvdb/src': new}},
                                          {'tier_a': {'playbit/kvdb/src': old}}), [])

    def test_existing_long_function_and_line_shift(self):
        source = 'fn long() {\n' + '\n' * 78 + '}\n'
        old, _ = self.measure(source)
        new, _ = self.measure('// header\n' + source)
        self.assertEqual(old['long_functions'][0]['lines'], 80)
        self.assertEqual(style.violations({'tier_a': {'playbit/kvdb/src': new}}, {'tier_a': {'playbit/kvdb/src': old}}), [])

    def test_check_publishes_only_regressions_and_logs_every_location(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            scripts = root / 'scripts'
            scripts.mkdir()
            script = shutil.copy(style.__file__, scripts)
            baseline = style.report(root, check_only=True)
            (scripts / 'style-baseline.json').write_text(json.dumps(baseline))
            summary = root / 'summary'
            env = os.environ | {'GITHUB_STEP_SUMMARY': str(summary)}
            command = [sys.executable, script, '--check']
            passed = subprocess.run(command, env=env, capture_output=True, text=True, check=True)
            self.assertEqual(passed.stdout.splitlines(), ['PASS: no tier A regressions.'])
            self.assertFalse(summary.exists())

            path = root / 'playbit/kvdb/src/db.rs'
            path.parent.mkdir(parents=True)
            path.write_text(''.join(f'fn long_{n}() {{' + '\n' * 71 + '}\n' for n in range(12)))
            failed = subprocess.run(command, env=env, capture_output=True, text=True)
            self.assertEqual(failed.returncode, 1)
            self.assertEqual(len(failed.stderr.splitlines()), 12)
            self.assertIn('long_11 is 72 lines', failed.stderr)
            published = summary.read_text()
            self.assertIn('playbit/kvdb/src/db.rs:1: long_0 is 72 lines', published)
            self.assertEqual(published.count('- `'), 10)
            self.assertIn('2 more in the job log', published)

    def test_tier_scopes_and_decoder_coverage(self):
        # Mixed platform trees must not transfer tier B metrics to A.
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            sources = {
                'playbit/kvdb/src/db.rs': 'fn decode_bytes() {}\n// fuzz: custom_target\nfn validate_item() {}\nfn parse_missing() {}\n',
                'playbit/fuzz/fuzz_targets/kvdb_wire.rs': 'decode_bytes(input);',
                'playbit/src/platform/web/mod.rs': 'fn decode_web() {}',
                'playbit/src/platform/ios/mod.rs': 'fn decode_ios() { None.unwrap(); }',
                'playbit/src/engine/mod.rs': 'fn parse_engine() {}',
            }
            for path, source in sources.items():
                p = root / path
                p.parent.mkdir(parents=True, exist_ok=True)
                p.write_text(source)
            with patch.object(style, 'crate_counts', return_value={}):
                report = style.report(root)
            self.assertEqual(style.report(root, check_only=True)['tier_a'], report['tier_a'])
            names = {f['name']: f['covered'] for f in report['decoders']}
            self.assertEqual(names, {'decode_bytes': True, 'validate_item': True, 'parse_missing': False,
                                     'decode_web': False})
            decoder = next(f for f in report['decoders'] if f['name'] == 'decode_bytes')
            self.assertEqual(decoder['fuzz_targets'], ['playbit/fuzz/fuzz_targets/kvdb_wire.rs'])
            self.assertEqual(report['tier_a']['playbit/kvdb/src']['functions'], 3)
            self.assertEqual(report['tier_a']['playbit/src/platform/web']['unwrap'], 0)
            self.assertEqual(report['directories']['playbit/src/platform']['unwrap'], 1)

    def test_crate_names_deduplicate_versions_and_unavailable(self):
        output = subprocess.CompletedProcess([], 0, 'kvdb v1.0\nredb v1.0\nredb v2.0 (*)\n', '')
        with patch.object(style.subprocess, 'run', return_value=output) as run:
            counts = style.crate_counts(Path('/example'))
            self.assertTrue(all(v['count'] == 2 for v in counts.values()))
            self.assertEqual(run.call_args.kwargs['cwd'], Path('/example'))
            self.assertIn('--locked', run.call_args.args[0])
        for error in (FileNotFoundError('no cargo'), subprocess.TimeoutExpired('cargo', 60)):
            with patch.object(style.subprocess, 'run', side_effect=error):
                counts = style.crate_counts(Path('/example'))
                self.assertTrue(all(v['count'] is None and v['unavailable'] for v in counts.values()))


if __name__ == '__main__':
    unittest.main()
