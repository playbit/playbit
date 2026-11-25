import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import provenance


class Provenance(unittest.TestCase):
    def test_records_pr_checkout_separately_from_head_and_refuses_wrong_checkout(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            event = root / 'event.json'
            event.write_text(json.dumps({'pull_request': {'number': 7, 'head': {'sha': 'b' * 40}}}))
            environment = {'GITHUB_SHA': 'a' * 40, 'GITHUB_RUN_ID': '42',
                           'GITHUB_RUN_ATTEMPT': '2', 'GITHUB_EVENT_PATH': str(event)}
            with patch.dict(os.environ, environment), \
                 patch.object(provenance.subprocess, 'check_output', return_value='a' * 40 + '\n'):
                provenance.record(root / 'output')
            value = json.loads((root / 'output/provenance.json').read_text())
            self.assertEqual(value['source_commit'], 'a' * 40)
            self.assertEqual(value['head_commit'], 'b' * 40)
            self.assertEqual((value['run_id'], value['run_attempt'], value['pull_request']), (42, 2, 7))
            with patch.dict(os.environ, environment), \
                 patch.object(provenance.subprocess, 'check_output', return_value='b' * 40), self.assertRaises(ValueError):
                provenance.record(root / 'wrong')
            self.assertFalse((root / 'wrong').exists())


if __name__ == '__main__':
    unittest.main()
