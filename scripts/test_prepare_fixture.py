"""Fixture generation requires an explicit existing kit and build profile."""
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / 'playbit/platform/ios/prepare-fixture.py'


class FixtureArguments(unittest.TestCase):
    def test_missing_kit_fails_without_building_or_creating_output(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            output = root / 'fixture.pbapp'
            result = subprocess.run([sys.executable, str(SCRIPT), 'todo', str(output),
                                     '--kit', str(root / 'missing.app'), '--profile', 'debug'],
                                    cwd=root, capture_output=True, text=True)
            self.assertEqual(result.returncode, 2)
            self.assertIn('kit is missing', result.stderr)
            self.assertEqual(list(root.iterdir()), [])

    def test_kit_and_profile_are_required(self):
        result = subprocess.run([sys.executable, str(SCRIPT), 'todo', 'unused.pbapp'],
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 2)
        self.assertIn('--kit', result.stderr)
        self.assertIn('--profile', result.stderr)


if __name__ == '__main__':
    unittest.main()
