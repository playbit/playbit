import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import release_range


class ReleaseRange(unittest.TestCase):
    def test_records_actual_multi_commit_main_push_before_checkout(self):
        # The parent of HEAD is not necessarily the start of the pushed range.
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            event = root/'event.json'
            payload = dict(ref='refs/heads/main', before='a'*40, after='b'*40)
            event.write_text(json.dumps(payload))
            environment = dict(GITHUB_EVENT_NAME='push', GITHUB_REF='refs/heads/main', GITHUB_SHA='b'*40,
                               GITHUB_RUN_ID='42', GITHUB_RUN_ATTEMPT='2', GITHUB_EVENT_PATH=str(event))
            with patch.dict(os.environ, environment), patch.object(release_range.subprocess, 'check_output', return_value='b'*40+'\n'):
                release_range.record(root/'output')
                value = json.loads((root/'output/range.json').read_text())
                self.assertEqual(value, dict(schema=1, source_commit='b'*40, before='a'*40, run_id=42, run_attempt=2))
                for changed in (dict(after='c'*40), dict(ref='refs/heads/other'), dict(before=None)):
                    event.write_text(json.dumps(payload | changed))
                    with self.assertRaises(ValueError):
                        release_range.record(root/'bad')
                event.write_text(json.dumps(payload | dict(before='0'*40)))
                release_range.record(root/'initial')
                self.assertIsNone(json.loads((root/'initial/range.json').read_text())['before'])

    def test_refuses_non_main_events_and_unbounded_event_input(self):
        # A PR/dispatch cannot manufacture the main push's path-filter evidence.
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            event = root/'event.json'; event.write_text('x'*100)
            environment = dict(GITHUB_EVENT_NAME='pull_request', GITHUB_REF='refs/heads/main', GITHUB_SHA='b'*40,
                               GITHUB_RUN_ID='42', GITHUB_RUN_ATTEMPT='1', GITHUB_EVENT_PATH=str(event))
            with patch.dict(os.environ, environment):
                with self.assertRaises(ValueError):
                    release_range.record(root/'output')
                with patch.dict(os.environ, GITHUB_EVENT_NAME='push'), patch.object(release_range.subprocess, 'check_output', return_value='b'*40), patch.object(release_range, 'MAX_EVENT_BYTES', 20):
                    with self.assertRaisesRegex(ValueError, 'exceeds'):
                        release_range.record(root/'output')


if __name__ == '__main__':
    unittest.main()
